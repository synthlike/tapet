use crate::agent::AgentSnapshot;
use crate::config::ProviderKind;
use crate::room::{Room, RoomId, RoomMessage};
use crate::stream::Completion;
use std::fs;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use tokio_rusqlite::Connection;
use tokio_rusqlite::rusqlite::{self, TransactionBehavior, params};

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

        if version != 0 && version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version == 0 {
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

    pub async fn create_room(
        &self,
        participants: Vec<AgentSnapshot>,
        description: String,
        prompt: String,
    ) -> Result<Room, StoreError> {
        let id = RoomId::new();
        let stored_id = id.to_string();
        let stored_participants = participants.clone();
        let stored_description = description.clone();
        let stored_prompt = prompt.clone();
        let now = now_millis();

        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute(
                    "INSERT INTO rooms (
                        id, description, system_prompt, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![stored_id, stored_description, stored_prompt, now],
                )?;
                for (position, participant) in stored_participants.iter().enumerate() {
                    transaction.execute(
                        "INSERT INTO room_participants (
                            room_id, agent_name, provider_kind, base_url, api_key_env,
                            model, system_prompt, position
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            stored_id,
                            participant.agent_name(),
                            participant.provider_kind().as_str(),
                            participant.base_url(),
                            participant.api_key_env(),
                            participant.model(),
                            participant.system_prompt(),
                            i64::try_from(position).expect("participant position fits in i64"),
                        ],
                    )?;
                }
                transaction.commit()
            })
            .await?;

        Ok(Room::new(id, participants, description, prompt))
    }

    pub async fn load_room(&self, id: &RoomId) -> Result<Room, StoreError> {
        let stored_id = id.to_string();
        let (participants, description, prompt) = self
            .connection
            .call(move |connection| {
                let (description, prompt): (String, String) = connection.query_row(
                    "SELECT description, system_prompt FROM rooms WHERE id = ?1",
                    [&stored_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let participants = query_room_participants(connection, &stored_id)?;
                Ok((participants, description, prompt))
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows) => {
                    StoreError::UnknownRoom(id.to_string())
                }
                other => StoreError::Worker(other),
            })?;

        Ok(Room::new(id.clone(), participants, description, prompt))
    }

    pub async fn room_history(&self, id: &RoomId) -> Result<Vec<RoomMessage>, StoreError> {
        let stored_id = id.to_string();
        Ok(self
            .connection
            .call(move |connection| query_room_messages(connection, &stored_id))
            .await?)
    }

    pub(crate) async fn append_room_user_message(
        &self,
        room_id: &RoomId,
        content: String,
    ) -> Result<AppendedRoomMessage, StoreError> {
        let stored_id = room_id.to_string();
        let now = now_millis();
        let (message_id, messages) = self
            .connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let room_exists = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM rooms WHERE id = ?1)",
                    [&stored_id],
                    |row| row.get::<_, bool>(0),
                )?;
                if !room_exists {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                transaction.execute(
                    "INSERT INTO room_messages (
                        room_id, speaker_kind, speaker_name, content, created_at
                     ) VALUES (?1, 'user', NULL, ?2, ?3)",
                    params![stored_id, content, now],
                )?;
                let message_id = transaction.last_insert_rowid();
                transaction.execute(
                    "UPDATE rooms SET updated_at = ?2 WHERE id = ?1",
                    params![stored_id, now],
                )?;
                let messages = query_room_messages(&transaction, &stored_id)?;
                transaction.commit()?;
                Ok((message_id, messages))
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows) => {
                    StoreError::UnknownRoom(room_id.to_string())
                }
                other => StoreError::Worker(other),
            })?;

        Ok(AppendedRoomMessage {
            message_id,
            messages,
        })
    }

    pub(crate) async fn begin_room_call(
        &self,
        room_id: &RoomId,
        participant_name: &str,
        user_message_id: i64,
    ) -> Result<i64, StoreError> {
        let stored_id = room_id.to_string();
        let participant_name = participant_name.to_owned();
        let now = now_millis();
        Ok(self
            .connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute(
                    "INSERT INTO room_calls (
                        room_id, participant_name, user_message_id, status, started_at
                     ) VALUES (?1, ?2, ?3, 'running', ?4)",
                    params![stored_id, participant_name, user_message_id, now],
                )?;
                let call_id = transaction.last_insert_rowid();
                transaction.commit()?;
                Ok(call_id)
            })
            .await?)
    }

    pub(crate) async fn complete_room_call(
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
                let (room_id, participant_name): (String, String) = transaction.query_row(
                    "SELECT room_id, participant_name FROM room_calls
                     WHERE id = ?1 AND status = 'running'",
                    [call_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                transaction.execute(
                    "INSERT INTO room_messages (
                        room_id, speaker_kind, speaker_name, content, created_at
                     ) VALUES (?1, 'agent', ?2, ?3, ?4)",
                    params![room_id, participant_name, assistant_message, now],
                )?;
                let assistant_message_id = transaction.last_insert_rowid();
                transaction.execute(
                    "UPDATE room_calls SET
                        assistant_message_id = ?2, status = 'completed',
                        provider_response_id = ?3, input_tokens = ?4,
                        output_tokens = ?5, finished_at = ?6
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
                    "UPDATE rooms SET updated_at = ?2 WHERE id = ?1",
                    params![room_id, now],
                )?;
                transaction.commit()
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn fail_room_call(
        &self,
        call_id: i64,
        error: String,
    ) -> Result<(), StoreError> {
        let now = now_millis();
        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let room_id: String = transaction.query_row(
                    "SELECT room_id FROM room_calls WHERE id = ?1 AND status = 'running'",
                    [call_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "UPDATE room_calls SET status = 'failed', error = ?2, finished_at = ?3
                     WHERE id = ?1 AND status = 'running'",
                    params![call_id, error, now],
                )?;
                transaction.execute(
                    "UPDATE rooms SET updated_at = ?2 WHERE id = ?1",
                    params![room_id, now],
                )?;
                transaction.commit()
            })
            .await?;
        Ok(())
    }
}

pub(crate) struct AppendedRoomMessage {
    pub message_id: i64,
    pub messages: Vec<RoomMessage>,
}

fn query_room_participants(
    connection: &rusqlite::Connection,
    room_id: &str,
) -> rusqlite::Result<Vec<AgentSnapshot>> {
    let mut statement = connection.prepare(
        "SELECT agent_name, provider_kind, base_url, api_key_env, model, system_prompt
         FROM room_participants WHERE room_id = ?1 ORDER BY position ASC",
    )?;
    statement
        .query_map([room_id], |row| {
            let stored_kind: String = row.get(1)?;
            let provider_kind =
                ProviderKind::from_stored(&stored_kind).ok_or(rusqlite::Error::InvalidQuery)?;
            Ok(AgentSnapshot::from_stored(
                row.get(0)?,
                provider_kind,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect()
}

fn query_room_messages(
    connection: &rusqlite::Connection,
    room_id: &str,
) -> rusqlite::Result<Vec<RoomMessage>> {
    let mut statement = connection.prepare(
        "SELECT speaker_kind, speaker_name, content
         FROM room_messages WHERE room_id = ?1 ORDER BY id ASC",
    )?;
    statement
        .query_map([room_id], |row| {
            let kind: String = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            let content: String = row.get(2)?;
            match (kind.as_str(), name) {
                ("user", None) => Ok(RoomMessage::user(content)),
                ("agent", Some(name)) => Ok(RoomMessage::agent(name, content)),
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
    #[error("unsupported database schema version {found}; expected {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("unknown room `{0}`")]
    UnknownRoom(String),
    #[error("token count {0} cannot be stored")]
    TokenCountOutOfRange(u64),
}

#[cfg(test)]
mod tests {
    use super::{Store, StoreError};
    use crate::agent::AgentSnapshot;
    use crate::room::RoomMessage;
    use crate::stream::Completion;
    use tempfile::TempDir;
    use tokio_rusqlite::rusqlite::{Connection, TransactionBehavior};

    #[tokio::test]
    async fn persists_room_snapshots_history_and_calls() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let store = Store::open(&path).await.unwrap();
        let participants = vec![
            AgentSnapshot::fixture_for("explorer", "fast-model", "Explore"),
            AgentSnapshot::fixture_for("reviewer", "deep-model", "Review"),
        ];
        let room = store
            .create_room(
                participants.clone(),
                "Research room".to_owned(),
                "Cite evidence".to_owned(),
            )
            .await
            .unwrap();
        let appended = store
            .append_room_user_message(room.id(), "@explorer investigate".to_owned())
            .await
            .unwrap();
        let call_id = store
            .begin_room_call(room.id(), "explorer", appended.message_id)
            .await
            .unwrap();
        store
            .complete_room_call(call_id, "Done".to_owned(), completion())
            .await
            .unwrap();

        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(
            reopened.load_room(room.id()).await.unwrap().participants(),
            participants
        );
        assert_eq!(
            reopened.room_history(room.id()).await.unwrap(),
            [
                RoomMessage::user("@explorer investigate"),
                RoomMessage::agent("explorer", "Done"),
            ]
        );
        let loaded = reopened.load_room(room.id()).await.unwrap();
        assert_eq!(loaded.description(), "Research room");
        assert_eq!(loaded.prompt(), "Cite evidence");
    }

    #[tokio::test]
    async fn supports_single_agent_rooms() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let participant = AgentSnapshot::fixture("Explore");
        let room = store
            .create_room(vec![participant.clone()], String::new(), String::new())
            .await
            .unwrap();
        assert_eq!(room.participants(), [participant]);
    }

    #[tokio::test]
    async fn no_transaction_remains_open_during_a_provider_call() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let store = Store::open(&path).await.unwrap();
        let room = store
            .create_room(
                vec![AgentSnapshot::fixture("Explore")],
                String::new(),
                String::new(),
            )
            .await
            .unwrap();
        let appended = store
            .append_room_user_message(room.id(), "question".to_owned())
            .await
            .unwrap();
        let call_id = store
            .begin_room_call(room.id(), "explorer", appended.message_id)
            .await
            .unwrap();

        let mut independent = Connection::open(&path).unwrap();
        independent
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap()
            .commit()
            .unwrap();
        store
            .fail_room_call(call_id, "test cleanup".to_owned())
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
    async fn configures_sqlite_for_concurrent_local_processes() {
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

    fn completion() -> Completion {
        Completion {
            provider_response_id: Some("resp_test".to_owned()),
            input_tokens: 3,
            output_tokens: 1,
        }
    }
}
