use crate::agent::AgentSnapshot;
use crate::config::ProviderKind;
use crate::room::{Room, RoomId, RoomMessage, RoomName};
use crate::stream::Completion;
use crate::stream::ToolCall;
use std::fs;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use tokio_rusqlite::Connection;
use tokio_rusqlite::rusqlite::{self, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 3;
const MAX_RANDOM_ROOM_ATTEMPTS: usize = 64;
const MIGRATION_001: &str = include_str!("../migrations/001_initial.sql");
const MIGRATION_002: &str = include_str!("../migrations/002_tool_calls.sql");
const MIGRATION_003: &str = include_str!("../migrations/003_room_names.sql");

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

        match version {
            0 => {
                connection
                    .call(|connection| {
                        let transaction = connection.transaction()?;
                        transaction.execute_batch(MIGRATION_001)?;
                        transaction.execute_batch(MIGRATION_002)?;
                        transaction.execute_batch(MIGRATION_003)?;
                        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                        transaction.commit()
                    })
                    .await?;
            }
            1 => {
                connection
                    .call(|connection| {
                        let transaction = connection.transaction()?;
                        transaction.execute_batch(MIGRATION_002)?;
                        transaction.execute_batch(MIGRATION_003)?;
                        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                        transaction.commit()
                    })
                    .await?;
            }
            2 => {
                connection
                    .call(|connection| {
                        let transaction = connection.transaction()?;
                        transaction.execute_batch(MIGRATION_003)?;
                        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                        transaction.commit()
                    })
                    .await?;
            }
            SCHEMA_VERSION => {}
            found => {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
        }

        Ok(Self { connection })
    }

    pub async fn create_room(
        &self,
        requested_name: Option<RoomName>,
        participants: Vec<AgentSnapshot>,
        description: String,
        prompt: String,
    ) -> Result<Room, StoreError> {
        if let Some(name) = requested_name {
            return self
                .insert_room(RoomId::new(), name, participants, description, prompt)
                .await;
        }

        for _ in 0..MAX_RANDOM_ROOM_ATTEMPTS {
            let name = RoomName::generate();
            match self
                .insert_room(
                    RoomId::new(),
                    name,
                    participants.clone(),
                    description.clone(),
                    prompt.clone(),
                )
                .await
            {
                Err(StoreError::RoomAlreadyExists(_)) => continue,
                result => return result,
            }
        }
        Err(StoreError::RandomRoomNameExhausted)
    }

    async fn insert_room(
        &self,
        id: RoomId,
        name: RoomName,
        participants: Vec<AgentSnapshot>,
        description: String,
        prompt: String,
    ) -> Result<Room, StoreError> {
        let stored_id = id.to_string();
        let stored_name = name.to_string();
        let stored_participants = participants.clone();
        let stored_description = description.clone();
        let stored_prompt = prompt.clone();
        let now = now_millis();

        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let room_exists = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM rooms WHERE name = ?1)",
                    [&stored_name],
                    |row| row.get::<_, bool>(0),
                )?;
                if room_exists {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                transaction.execute(
                    "INSERT INTO rooms (
                        id, name, description, system_prompt, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    params![
                        stored_id,
                        stored_name,
                        stored_description,
                        stored_prompt,
                        now
                    ],
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
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows) => {
                    StoreError::RoomAlreadyExists(name.to_string())
                }
                other => StoreError::Worker(other),
            })?;

        Ok(Room::new(id, name, participants, description, prompt))
    }

    pub async fn load_room(&self, name: &RoomName) -> Result<Room, StoreError> {
        let stored_name = name.to_string();
        let (id, participants, description, prompt) = self
            .connection
            .call(move |connection| {
                let (id, description, prompt): (String, String, String) = connection.query_row(
                    "SELECT id, description, system_prompt FROM rooms WHERE name = ?1",
                    [&stored_name],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                let participants = query_room_participants(connection, &id)?;
                Ok((id, participants, description, prompt))
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows) => {
                    StoreError::UnknownRoom(name.to_string())
                }
                other => StoreError::Worker(other),
            })?;
        let id: RoomId = id.parse().expect("stored room id is a valid uuid");

        Ok(Room::new(
            id,
            name.clone(),
            participants,
            description,
            prompt,
        ))
    }

    /// Rooms already persisted in the local database — the ones `tapet enter`
    /// can resume — most recently active first.
    pub async fn list_rooms(&self) -> Result<Vec<RoomSummary>, StoreError> {
        Ok(self
            .connection
            .call(|connection| {
                let mut statement =
                    connection.prepare("SELECT id, name FROM rooms ORDER BY updated_at DESC")?;
                let rooms = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                let mut summaries = Vec::with_capacity(rooms.len());
                for (id, name) in rooms {
                    let participants = query_room_participant_names(connection, &id)?;
                    summaries.push(RoomSummary {
                        name: name.parse().expect("stored room name is valid"),
                        participants,
                    });
                }
                Ok(summaries)
            })
            .await?)
    }

    pub async fn add_room_participant(
        &self,
        room_id: &RoomId,
        participant: AgentSnapshot,
    ) -> Result<(), StoreError> {
        let stored_id = room_id.to_string();
        let agent_name = participant.agent_name().to_owned();
        let now = now_millis();

        self.connection
            .call(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let exists = transaction.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM room_participants WHERE room_id = ?1 AND agent_name = ?2
                     )",
                    params![stored_id, participant.agent_name()],
                    |row| row.get::<_, bool>(0),
                )?;
                if exists {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                let next_position: i64 = transaction.query_row(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM room_participants
                     WHERE room_id = ?1",
                    [&stored_id],
                    |row| row.get(0),
                )?;
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
                        next_position,
                    ],
                )?;
                transaction.execute(
                    "UPDATE rooms SET updated_at = ?2 WHERE id = ?1",
                    params![stored_id, now],
                )?;
                transaction.commit()
            })
            .await
            .map_err(|error| match error {
                tokio_rusqlite::Error::Error(rusqlite::Error::QueryReturnedNoRows) => {
                    StoreError::ParticipantAlreadyExists(agent_name)
                }
                other => StoreError::Worker(other),
            })?;

        Ok(())
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

    pub(crate) async fn record_tool_proposal(
        &self,
        room_call_id: i64,
        call: &ToolCall,
    ) -> Result<i64, StoreError> {
        let provider_call_id = call.call_id.clone();
        let tool_name = call.name.clone();
        let arguments = call.arguments.clone();
        let now = now_millis();
        Ok(self
            .connection
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO tool_calls (
                        room_call_id, provider_call_id, tool_name, arguments,
                        status, proposed_at
                     ) VALUES (?1, ?2, ?3, ?4, 'proposed', ?5)",
                    params![room_call_id, provider_call_id, tool_name, arguments, now],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .await?)
    }

    pub(crate) async fn approve_tool_call(&self, tool_call_id: i64) -> Result<(), StoreError> {
        self.transition_tool_call(
            tool_call_id,
            "UPDATE tool_calls SET status = 'approved', decided_at = ?2
             WHERE id = ?1 AND status = 'proposed'",
            now_millis(),
        )
        .await
    }

    pub(crate) async fn deny_tool_call(&self, tool_call_id: i64) -> Result<(), StoreError> {
        self.transition_tool_call(
            tool_call_id,
            "UPDATE tool_calls SET status = 'denied', decided_at = ?2, finished_at = ?2
             WHERE id = ?1 AND status = 'proposed'",
            now_millis(),
        )
        .await
    }

    pub(crate) async fn start_tool_call(&self, tool_call_id: i64) -> Result<(), StoreError> {
        self.transition_tool_call(
            tool_call_id,
            "UPDATE tool_calls SET status = 'running', started_at = ?2
             WHERE id = ?1 AND status = 'approved'",
            now_millis(),
        )
        .await
    }

    pub(crate) async fn complete_tool_call(
        &self,
        tool_call_id: i64,
        bytes: u64,
        lines: u64,
    ) -> Result<(), StoreError> {
        let bytes = token_count(bytes)?;
        let lines = token_count(lines)?;
        let now = now_millis();
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE tool_calls SET status = 'completed', result_bytes = ?2,
                        result_lines = ?3, finished_at = ?4
                     WHERE id = ?1 AND status = 'running'",
                    params![tool_call_id, bytes, lines, now],
                )?;
                require_transition(changed)
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn fail_tool_call(
        &self,
        tool_call_id: i64,
        error: String,
    ) -> Result<(), StoreError> {
        let now = now_millis();
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE tool_calls SET status = 'failed', error = ?2, finished_at = ?3
                     WHERE id = ?1 AND status IN ('proposed', 'approved', 'running')",
                    params![tool_call_id, error, now],
                )?;
                require_transition(changed)
            })
            .await?;
        Ok(())
    }

    async fn transition_tool_call(
        &self,
        tool_call_id: i64,
        statement: &'static str,
        now: i64,
    ) -> Result<(), StoreError> {
        self.connection
            .call(move |connection| {
                let changed = connection.execute(statement, params![tool_call_id, now])?;
                require_transition(changed)
            })
            .await?;
        Ok(())
    }
}

pub(crate) struct AppendedRoomMessage {
    pub message_id: i64,
    pub messages: Vec<RoomMessage>,
}

pub struct RoomSummary {
    name: RoomName,
    participants: Vec<String>,
}

impl RoomSummary {
    pub fn name(&self) -> &RoomName {
        &self.name
    }

    pub fn participants(&self) -> &[String] {
        &self.participants
    }
}

#[cfg(test)]
impl RoomSummary {
    pub fn fixture(name: &str, participants: &[&str]) -> Self {
        Self {
            name: name.parse().expect("fixture room name is valid"),
            participants: participants.iter().map(|name| (*name).to_owned()).collect(),
        }
    }
}

fn query_room_participant_names(
    connection: &rusqlite::Connection,
    room_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT agent_name FROM room_participants WHERE room_id = ?1 ORDER BY position ASC",
    )?;
    statement.query_map([room_id], |row| row.get(0))?.collect()
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

fn require_transition(changed: usize) -> rusqlite::Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::InvalidQuery)
    }
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
    #[error("a room named `{0}` already exists")]
    RoomAlreadyExists(String),
    #[error("could not generate an unused random room name")]
    RandomRoomNameExhausted,
    #[error("`@{0}` is already in this room")]
    ParticipantAlreadyExists(String),
    #[error("token count {0} cannot be stored")]
    TokenCountOutOfRange(u64),
}

#[cfg(test)]
mod tests {
    use super::{Store, StoreError};
    use crate::agent::AgentSnapshot;
    use crate::room::{RoomMessage, RoomName};
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
                None,
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
            reopened
                .load_room(room.name())
                .await
                .unwrap()
                .participants(),
            participants
        );
        assert_eq!(
            reopened.room_history(room.id()).await.unwrap(),
            [
                RoomMessage::user("@explorer investigate"),
                RoomMessage::agent("explorer", "Done"),
            ]
        );
        let loaded = reopened.load_room(room.name()).await.unwrap();
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
            .create_room(
                None,
                vec![participant.clone()],
                String::new(),
                String::new(),
            )
            .await
            .unwrap();
        assert_eq!(room.participants(), [participant]);
    }

    #[tokio::test]
    async fn persists_custom_room_names_and_rejects_duplicates() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let name = "sweaty-warroom".parse::<RoomName>().unwrap();
        let participant = AgentSnapshot::fixture("Explore");

        let room = store
            .create_room(
                Some(name.clone()),
                vec![participant.clone()],
                String::new(),
                String::new(),
            )
            .await
            .unwrap();
        assert_eq!(room.name(), &name);
        assert!(matches!(
            store
                .create_room(
                    Some(name),
                    vec![participant],
                    String::new(),
                    String::new(),
                )
                .await,
            Err(StoreError::RoomAlreadyExists(name)) if name == "sweaty-warroom"
        ));
    }

    #[tokio::test]
    async fn adds_a_participant_and_rejects_duplicates() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let store = Store::open(&path).await.unwrap();
        let explorer = AgentSnapshot::fixture_for("explorer", "fast-model", "Explore");
        let doubter = AgentSnapshot::fixture_for("doubter", "fast-model", "Doubt");
        let room = store
            .create_room(None, vec![explorer.clone()], String::new(), String::new())
            .await
            .unwrap();

        store
            .add_room_participant(room.id(), doubter.clone())
            .await
            .unwrap();

        let reopened = Store::open(&path).await.unwrap();
        let loaded = reopened.load_room(room.name()).await.unwrap();
        assert_eq!(loaded.participants(), [explorer, doubter.clone()]);

        assert!(matches!(
            store.add_room_participant(room.id(), doubter).await,
            Err(StoreError::ParticipantAlreadyExists(name)) if name == "doubter"
        ));
    }

    #[tokio::test]
    async fn lists_rooms_most_recently_updated_first() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let explorer = AgentSnapshot::fixture_for("explorer", "fast-model", "Explore");
        let duck = AgentSnapshot::fixture_for("duck", "fast-model", "Ask");

        let first = store
            .create_room(
                Some("first-room".parse().unwrap()),
                vec![explorer],
                String::new(),
                String::new(),
            )
            .await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store
            .create_room(
                Some("second-room".parse().unwrap()),
                vec![duck.clone()],
                String::new(),
                String::new(),
            )
            .await
            .unwrap();

        let rooms = store.list_rooms().await.unwrap();
        let names: Vec<_> = rooms.iter().map(|room| room.name().to_string()).collect();
        assert_eq!(names, ["second-room", "first-room"]);
        assert_eq!(rooms[0].participants(), ["duck".to_owned()]);
        assert_eq!(rooms[1].participants(), ["explorer".to_owned()]);

        std::thread::sleep(std::time::Duration::from_millis(5));
        store.add_room_participant(first.id(), duck).await.unwrap();
        let rooms = store.list_rooms().await.unwrap();
        assert_eq!(rooms[0].name().to_string(), "first-room");
    }

    #[tokio::test]
    async fn no_transaction_remains_open_during_a_provider_call() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let store = Store::open(&path).await.unwrap();
        let room = store
            .create_room(
                None,
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
        connection.pragma_update(None, "user_version", 4).unwrap();
        drop(connection);

        assert!(matches!(
            Store::open(path).await,
            Err(StoreError::UnsupportedSchemaVersion {
                found: 4,
                supported: 3
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
        assert_eq!(user_version, 3);
    }

    #[tokio::test]
    async fn migrates_version_one_databases() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(super::MIGRATION_001).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        drop(connection);

        let store = Store::open(path).await.unwrap();
        let (version, tool_calls_exists) = store
            .connection
            .call(|connection| {
                let version = connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
                let exists = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master
                     WHERE type = 'table' AND name = 'tool_calls')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>((version, exists))
            })
            .await
            .unwrap();

        assert_eq!(version, 3);
        assert!(tool_calls_exists);
    }

    #[tokio::test]
    async fn migrates_version_two_databases() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("tapet.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(super::MIGRATION_001).unwrap();
        connection.execute_batch(super::MIGRATION_002).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        drop(connection);

        let store = Store::open(path).await.unwrap();
        let (version, name_column_exists) = store
            .connection
            .call(|connection| {
                let version = connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
                let exists = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('rooms')
                     WHERE name = 'name')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>((version, exists))
            })
            .await
            .unwrap();

        assert_eq!(version, 3);
        assert!(name_column_exists);
    }

    #[tokio::test]
    async fn persists_tool_lifecycle_metadata_without_results() {
        let temporary = TempDir::new().unwrap();
        let store = Store::open(temporary.path().join("tapet.db"))
            .await
            .unwrap();
        let room = store
            .create_room(
                None,
                vec![AgentSnapshot::fixture("Explore")],
                String::new(),
                String::new(),
            )
            .await
            .unwrap();
        let message = store
            .append_room_user_message(room.id(), "read it".to_owned())
            .await
            .unwrap();
        let room_call = store
            .begin_room_call(room.id(), "explorer", message.message_id)
            .await
            .unwrap();
        let tool_call = store
            .record_tool_proposal(
                room_call,
                &crate::stream::ToolCall {
                    call_id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
                },
            )
            .await
            .unwrap();
        store.approve_tool_call(tool_call).await.unwrap();
        store.start_tool_call(tool_call).await.unwrap();
        store.complete_tool_call(tool_call, 2048, 64).await.unwrap();

        let row = store
            .connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT status, result_bytes, result_lines, error
                     FROM tool_calls WHERE id = ?1",
                    [tool_call],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
            })
            .await
            .unwrap();

        assert_eq!(row, ("completed".to_owned(), 2048, 64, None));
    }

    fn completion() -> Completion {
        Completion {
            provider_response_id: Some("resp_test".to_owned()),
            input_tokens: 3,
            output_tokens: 1,
        }
    }
}
