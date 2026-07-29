use crate::config::ProviderKind;
use crate::message::Message;
use crate::session::{AgentSnapshot, Session, SessionId};
use crate::stream::Completion;
use std::fs;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use tokio_rusqlite::Connection;
use tokio_rusqlite::rusqlite::{self, OptionalExtension, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 1;
const MIGRATION_001: &str = include_str!("../migrations/001_initial.sql");

#[derive(Clone)]
pub struct Store {
    connection: Connection,
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| StoreError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let connection = Connection::open(path).await?;
        let version = connection
            .call(|connection| {
                connection.busy_timeout(Duration::from_secs(5))?;
                connection.pragma_update(None, "foreign_keys", "ON")?;
                connection.pragma_update(None, "journal_mode", "WAL")?;
                connection.pragma_query_value(None, "user_version", |row| row.get(0))
            })
            .await?;

        if version > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < SCHEMA_VERSION {
            connection
                .call(|connection| {
                    let transaction = connection.transaction()?;
                    transaction.execute_batch(MIGRATION_001)?;
                    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                    transaction.commit()
                })
                .await?;
        }

        Ok(Self { connection })
    }

    pub async fn create_session(&self, agent: AgentSnapshot) -> Result<Session, StoreError> {
        let id = SessionId::new();
        let stored_id = id.to_string();
        let stored_agent = agent.clone();
        let now = now_millis();

        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute(
                    "INSERT INTO sessions (
                        id, agent_name, provider_kind, base_url, api_key_env, model,
                        system_prompt, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                    params![
                        stored_id,
                        stored_agent.agent_name(),
                        stored_agent.provider_kind().as_str(),
                        stored_agent.base_url(),
                        stored_agent.api_key_env(),
                        stored_agent.model(),
                        stored_agent.system_prompt(),
                        now,
                    ],
                )?;
                transaction.commit()
            })
            .await?;

        Ok(Session::new(id, agent))
    }

    pub async fn load_session(&self, id: &SessionId) -> Result<Session, StoreError> {
        let stored_id = id.to_string();
        let agent = self
            .connection
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT agent_name, provider_kind, base_url, api_key_env, model, system_prompt
                         FROM sessions WHERE id = ?1",
                        [stored_id],
                        |row| {
                            let stored_kind: String = row.get(1)?;
                            let provider_kind = ProviderKind::from_stored(&stored_kind)
                                .ok_or(rusqlite::Error::InvalidQuery)?;
                            Ok(AgentSnapshot::from_stored(
                                row.get(0)?,
                                provider_kind,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                            ))
                        },
                    )
                    .optional()
            })
            .await?
            .ok_or_else(|| StoreError::UnknownSession(id.to_string()))?;

        Ok(Session::new(id.clone(), agent))
    }

    pub async fn history(&self, id: &SessionId) -> Result<Vec<Message>, StoreError> {
        let stored_id = id.to_string();
        let messages = self
            .connection
            .call(move |connection| query_messages(connection, &stored_id))
            .await?;
        Ok(messages)
    }

    pub async fn sessions(&self) -> Result<Vec<SessionSummary>, StoreError> {
        Ok(self
            .connection
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, agent_name, model, updated_at
                     FROM sessions ORDER BY updated_at DESC, id ASC",
                )?;
                statement
                    .query_map([], |row| {
                        Ok(SessionSummary {
                            id: row.get(0)?,
                            agent_name: row.get(1)?,
                            model: row.get(2)?,
                            updated_at_ms: row.get(3)?,
                        })
                    })?
                    .collect()
            })
            .await?)
    }

    pub(crate) async fn begin_call(
        &self,
        session_id: &SessionId,
        user_message: String,
    ) -> Result<BegunCall, StoreError> {
        let stored_id = session_id.to_string();
        let now = now_millis();
        let (call_id, messages) = self
            .connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let session_exists = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                    [&stored_id],
                    |row| row.get::<_, bool>(0),
                )?;
                if !session_exists {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }

                transaction.execute(
                    "INSERT INTO messages (session_id, role, content, created_at)
                     VALUES (?1, 'user', ?2, ?3)",
                    params![stored_id, user_message, now],
                )?;
                let user_message_id = transaction.last_insert_rowid();
                transaction.execute(
                    "INSERT INTO model_calls (
                        session_id, user_message_id, status, started_at
                     ) VALUES (?1, ?2, 'running', ?3)",
                    params![stored_id, user_message_id, now],
                )?;
                let call_id = transaction.last_insert_rowid();
                transaction.execute(
                    "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                    params![stored_id, now],
                )?;
                let messages = query_messages(&transaction, &stored_id)?;
                transaction.commit()?;
                Ok((call_id, messages))
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows) => {
                    StoreError::UnknownSession(session_id.to_string())
                }
                other => StoreError::Worker(other),
            })?;

        Ok(BegunCall { call_id, messages })
    }

    pub(crate) async fn complete_call(
        &self,
        call_id: i64,
        assistant_message: String,
        completion: Completion,
    ) -> Result<(), StoreError> {
        let now = now_millis();
        let input_tokens = token_count(completion.input_tokens)?;
        let output_tokens = token_count(completion.output_tokens)?;
        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let session_id: String = transaction.query_row(
                    "SELECT session_id FROM model_calls
                     WHERE id = ?1 AND status = 'running'",
                    [call_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "INSERT INTO messages (session_id, role, content, created_at)
                     VALUES (?1, 'assistant', ?2, ?3)",
                    params![session_id, assistant_message, now],
                )?;
                let assistant_message_id = transaction.last_insert_rowid();
                transaction.execute(
                    "UPDATE model_calls SET
                        assistant_message_id = ?2,
                        status = 'completed',
                        provider_response_id = ?3,
                        input_tokens = ?4,
                        output_tokens = ?5,
                        finished_at = ?6
                     WHERE id = ?1 AND status = 'running'",
                    params![
                        call_id,
                        assistant_message_id,
                        completion.provider_response_id,
                        input_tokens,
                        output_tokens,
                        now,
                    ],
                )?;
                transaction.execute(
                    "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                    params![session_id, now],
                )?;
                transaction.commit()
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn fail_call(&self, call_id: i64, error: String) -> Result<(), StoreError> {
        let now = now_millis();
        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let session_id: String = transaction.query_row(
                    "SELECT session_id FROM model_calls
                     WHERE id = ?1 AND status = 'running'",
                    [call_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "UPDATE model_calls SET status = 'failed', error = ?2, finished_at = ?3
                     WHERE id = ?1 AND status = 'running'",
                    params![call_id, error, now],
                )?;
                transaction.execute(
                    "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                    params![session_id, now],
                )?;
                transaction.commit()
            })
            .await?;
        Ok(())
    }

    #[cfg(test)]
    async fn call_record(&self, call_id: i64) -> Result<CallRecord, StoreError> {
        Ok(self
            .connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT status, provider_response_id, input_tokens, output_tokens, error
                     FROM model_calls WHERE id = ?1",
                    [call_id],
                    |row| {
                        Ok(CallRecord {
                            status: row.get(0)?,
                            provider_response_id: row.get(1)?,
                            input_tokens: row.get(2)?,
                            output_tokens: row.get(3)?,
                            error: row.get(4)?,
                        })
                    },
                )
            })
            .await?)
    }
}

pub(crate) struct BegunCall {
    pub call_id: i64,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    id: String,
    agent_name: String,
    model: String,
    updated_at_ms: i64,
}

impl SessionSummary {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn updated_at_ms(&self) -> i64 {
        self.updated_at_ms
    }
}

#[cfg(test)]
impl SessionSummary {
    pub fn fixture(id: &str, agent_name: &str, model: &str, updated_at_ms: i64) -> Self {
        Self {
            id: id.to_owned(),
            agent_name: agent_name.to_owned(),
            model: model.to_owned(),
            updated_at_ms,
        }
    }
}

fn query_messages(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<Message>> {
    let mut statement = connection
        .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id ASC")?;
    statement
        .query_map([session_id], |row| {
            let role: String = row.get(0)?;
            let content: String = row.get(1)?;
            match role.as_str() {
                "user" => Ok(Message::user(content)),
                "assistant" => Ok(Message::assistant(content)),
                _ => Err(rusqlite::Error::InvalidQuery),
            }
        })?
        .collect()
}

fn now_millis() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .expect("current timestamps fit in an i64 millisecond value")
}

fn token_count(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::TokenCountOutOfRange(value))
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot create state directory `{path}`: {source}")]
    CreateDirectory {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("SQLite worker error: {0}")]
    Worker(#[from] tokio_rusqlite::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("unknown session `{0}`")]
    UnknownSession(String),
    #[error("token count {0} cannot be stored")]
    TokenCountOutOfRange(u64),
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct CallRecord {
    status: String,
    provider_response_id: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{CallRecord, Store, StoreError};
    use crate::message::Message;
    use crate::session::AgentSnapshot;
    use crate::stream::Completion;
    use tempfile::TempDir;
    use tokio_rusqlite::rusqlite::{Connection, TransactionBehavior};

    #[tokio::test]
    async fn persists_the_snapshot_messages_and_completed_call() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let store = Store::open(&path).await.unwrap();
        let snapshot = AgentSnapshot::fixture("Original prompt");
        let session = store.create_session(snapshot.clone()).await.unwrap();

        let call = store
            .begin_call(session.id(), "Hello".to_owned())
            .await
            .unwrap();
        assert_eq!(call.messages, [Message::user("Hello")]);
        store
            .complete_call(
                call.call_id,
                "Hi there".to_owned(),
                Completion {
                    provider_response_id: Some("resp_1".to_owned()),
                    input_tokens: 7,
                    output_tokens: 3,
                },
            )
            .await
            .unwrap();

        let second_process = Store::open(&path).await.unwrap();
        let loaded = second_process.load_session(session.id()).await.unwrap();
        assert_eq!(loaded.agent(), &snapshot);
        assert_eq!(
            second_process.history(session.id()).await.unwrap(),
            [Message::user("Hello"), Message::assistant("Hi there")]
        );
        let resumed = second_process
            .begin_call(session.id(), "Remember me?".to_owned())
            .await
            .unwrap();
        assert_eq!(
            resumed.messages,
            [
                Message::user("Hello"),
                Message::assistant("Hi there"),
                Message::user("Remember me?")
            ]
        );
        second_process
            .fail_call(resumed.call_id, "test cleanup".to_owned())
            .await
            .unwrap();
        assert_eq!(
            second_process.call_record(call.call_id).await.unwrap(),
            CallRecord {
                status: "completed".to_owned(),
                provider_response_id: Some("resp_1".to_owned()),
                input_tokens: Some(7),
                output_tokens: Some(3),
                error: None,
            }
        );
    }

    #[tokio::test]
    async fn failed_calls_are_durable_without_an_assistant_message() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let session = store
            .create_session(AgentSnapshot::fixture("Prompt"))
            .await
            .unwrap();
        let call = store
            .begin_call(session.id(), "Will fail".to_owned())
            .await
            .unwrap();

        store
            .fail_call(call.call_id, "provider failed".to_owned())
            .await
            .unwrap();

        assert_eq!(
            store.history(session.id()).await.unwrap(),
            [Message::user("Will fail")]
        );
        assert_eq!(
            store.call_record(call.call_id).await.unwrap(),
            CallRecord {
                status: "failed".to_owned(),
                provider_response_id: None,
                input_tokens: None,
                output_tokens: None,
                error: Some("provider failed".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn no_transaction_remains_open_during_the_provider_call() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let store = Store::open(&path).await.unwrap();
        let session = store
            .create_session(AgentSnapshot::fixture("Prompt"))
            .await
            .unwrap();
        let call = store
            .begin_call(session.id(), "Question".to_owned())
            .await
            .unwrap();

        let mut independent = Connection::open(&path).unwrap();
        independent
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap()
            .commit()
            .unwrap();

        store
            .fail_call(call.call_id, "test cleanup".to_owned())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rejects_newer_database_versions() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);

        assert!(matches!(
            Store::open(path).await,
            Err(StoreError::UnsupportedSchemaVersion {
                found: 2,
                supported: 1
            })
        ));
    }

    #[tokio::test]
    async fn configures_sqlite_for_local_concurrent_processes() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();

        let (journal_mode, foreign_keys, busy_timeout, user_version) = store
            .connection
            .call(|connection| {
                Ok::<_, tokio_rusqlite::rusqlite::Error>((
                    connection
                        .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?,
                    connection
                        .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?,
                    connection
                        .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))?,
                    connection
                        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();

        assert_eq!(journal_mode, "wal");
        assert!(foreign_keys);
        assert_eq!(busy_timeout, 5_000);
        assert_eq!(user_version, 1);
    }

    #[tokio::test]
    async fn lists_sessions_by_updated_time_with_stable_ties() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let explorer = store
            .create_session(AgentSnapshot::fixture_for(
                "explorer",
                "fast-model",
                "Explore",
            ))
            .await
            .unwrap();
        let reviewer = store
            .create_session(AgentSnapshot::fixture_for(
                "reviewer",
                "deep-model",
                "Review",
            ))
            .await
            .unwrap();
        let explorer_id = explorer.id().to_string();
        let reviewer_id = reviewer.id().to_string();
        store
            .connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE sessions SET updated_at = 100 WHERE id = ?1",
                    [explorer_id],
                )?;
                connection.execute(
                    "UPDATE sessions SET updated_at = 200 WHERE id = ?1",
                    [reviewer_id],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();

        let sessions = store.sessions().await.unwrap();

        assert_eq!(sessions[0].id(), reviewer.id().to_string());
        assert_eq!(sessions[0].agent_name(), "reviewer");
        assert_eq!(sessions[0].model(), "deep-model");
        assert_eq!(sessions[0].updated_at_ms(), 200);
        assert_eq!(sessions[1].id(), explorer.id().to_string());
    }

    #[tokio::test]
    async fn independent_agents_can_turn_concurrently_without_history_leaks() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let setup = Store::open(&path).await.unwrap();
        let explorer = setup
            .create_session(AgentSnapshot::fixture_for(
                "explorer",
                "fast-model",
                "Explore",
            ))
            .await
            .unwrap();
        let reviewer = setup
            .create_session(AgentSnapshot::fixture_for(
                "reviewer",
                "deep-model",
                "Review",
            ))
            .await
            .unwrap();
        let explorer_store = Store::open(&path).await.unwrap();
        let reviewer_store = Store::open(&path).await.unwrap();

        let (explorer_call, reviewer_call) = tokio::join!(
            explorer_store.begin_call(explorer.id(), "Explore this".to_owned()),
            reviewer_store.begin_call(reviewer.id(), "Review this".to_owned())
        );
        let explorer_call = explorer_call.unwrap();
        let reviewer_call = reviewer_call.unwrap();

        assert_eq!(explorer_call.messages, [Message::user("Explore this")]);
        assert_eq!(reviewer_call.messages, [Message::user("Review this")]);
        explorer_store
            .complete_call(
                explorer_call.call_id,
                "Explored".to_owned(),
                completion("resp_explorer"),
            )
            .await
            .unwrap();
        reviewer_store
            .complete_call(
                reviewer_call.call_id,
                "Reviewed".to_owned(),
                completion("resp_reviewer"),
            )
            .await
            .unwrap();

        assert_eq!(
            explorer_store.history(explorer.id()).await.unwrap(),
            [
                Message::user("Explore this"),
                Message::assistant("Explored")
            ]
        );
        assert_eq!(
            reviewer_store.history(reviewer.id()).await.unwrap(),
            [Message::user("Review this"), Message::assistant("Reviewed")]
        );
        assert_eq!(
            explorer_store
                .load_session(explorer.id())
                .await
                .unwrap()
                .agent()
                .model(),
            "fast-model"
        );
        assert_eq!(
            reviewer_store
                .load_session(reviewer.id())
                .await
                .unwrap()
                .agent()
                .model(),
            "deep-model"
        );
    }

    fn completion(response_id: &str) -> Completion {
        Completion {
            provider_response_id: Some(response_id.to_owned()),
            input_tokens: 1,
            output_tokens: 1,
        }
    }
}
