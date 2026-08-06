use crate::agent::AgentSnapshot;
use crate::message::Message;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomId(String);

impl RoomId {
    pub fn new() -> Self {
        Self(format!("room_{}", Uuid::new_v4().simple()))
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for RoomId {
    type Err = RoomIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let original = value;
        let uuid = value
            .strip_prefix("room_")
            .ok_or_else(|| RoomIdError(original.to_owned()))
            .and_then(|value| {
                Uuid::parse_str(value).map_err(|_| RoomIdError(original.to_owned()))
            })?;
        Ok(Self(format!("room_{}", uuid.simple())))
    }
}

#[derive(Debug, Error)]
#[error("invalid room ID `{0}`")]
pub struct RoomIdError(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Room {
    id: RoomId,
    participants: Vec<AgentSnapshot>,
    description: String,
    prompt: String,
}

impl Room {
    pub(crate) fn new(
        id: RoomId,
        participants: Vec<AgentSnapshot>,
        description: String,
        prompt: String,
    ) -> Self {
        Self {
            id,
            participants,
            description,
            prompt,
        }
    }

    pub fn id(&self) -> &RoomId {
        &self.id
    }

    pub fn participants(&self) -> &[AgentSnapshot] {
        &self.participants
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn participant(&self, name: &str) -> Option<&AgentSnapshot> {
        self.participants
            .iter()
            .find(|participant| participant.agent_name() == name)
    }

    pub fn route(&self, message: &RoomMessage) -> Result<Vec<&AgentSnapshot>, RoomError> {
        if matches!(message.speaker(), RoomSpeaker::Agent(_)) {
            return Ok(Vec::new());
        }

        let mentions = mentions(message.content());
        if mentions.is_empty() {
            return Ok(self.participants.first().into_iter().collect());
        }

        mentions
            .into_iter()
            .map(|name| {
                self.participant(name)
                    .ok_or_else(|| RoomError::UnknownMention {
                        name: name.to_owned(),
                        participants: self.participant_names(),
                    })
            })
            .collect()
    }

    fn participant_names(&self) -> Vec<String> {
        self.participants
            .iter()
            .map(|participant| participant.agent_name().to_owned())
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoomSpeaker {
    User,
    Agent(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomMessage {
    speaker: RoomSpeaker,
    content: String,
}

impl RoomMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            speaker: RoomSpeaker::User,
            content: content.into(),
        }
    }

    pub fn agent(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            speaker: RoomSpeaker::Agent(name.into()),
            content: content.into(),
        }
    }

    pub fn speaker(&self) -> &RoomSpeaker {
        &self.speaker
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn visible_content(&self) -> &str {
        match &self.speaker {
            RoomSpeaker::User => &self.content,
            RoomSpeaker::Agent(name) => strip_self_attribution(name, &self.content),
        }
    }

    pub fn as_provider_message(&self) -> Message {
        match &self.speaker {
            RoomSpeaker::User => Message::user(format!("you: {}", self.content)),
            RoomSpeaker::Agent(name) => {
                Message::assistant(format!("@{name}: {}", self.visible_content()))
            }
        }
    }
}

fn strip_self_attribution<'a>(agent_name: &str, content: &'a str) -> &'a str {
    let prefix = format!("@{agent_name}");
    let Some(candidate) = content.get(..prefix.len()) else {
        return content;
    };
    if !candidate.eq_ignore_ascii_case(&prefix) {
        return content;
    }

    let remainder = &content[prefix.len()..];
    if let Some(remainder) = remainder.strip_prefix(':') {
        remainder.trim_start_matches(char::is_whitespace)
    } else if remainder.starts_with(char::is_whitespace) || remainder.is_empty() {
        remainder.trim_start_matches(char::is_whitespace)
    } else {
        content
    }
}

pub fn room_instructions(room: &Room, agent: &AgentSnapshot) -> String {
    let mut instructions = format!(
        "{}\n\nYou are @{} in a shared room. Messages are prefixed with their speaker. Respond only as this participant and do not impersonate others. Your response is labeled automatically, so do not begin it with @{} or any speaker label.",
        agent.system_prompt(),
        agent.agent_name(),
        agent.agent_name()
    );
    if !room.prompt().is_empty() {
        instructions.push_str("\n\nRoom instructions:\n");
        instructions.push_str(room.prompt());
    }
    instructions.push_str(
        "\n\nTapet may expose proposal-only tools. You may request one when it is needed, but this version will display the request without executing it. Never claim that a proposed tool ran or that you observed its result.",
    );
    instructions
}

fn mentions(message: &str) -> Vec<&str> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    for token in message.split_whitespace() {
        let Some(candidate) = token.strip_prefix('@') else {
            continue;
        };
        let end = candidate
            .find(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_' && character != '-'
            })
            .unwrap_or(candidate.len());
        let name = &candidate[..end];
        if !name.is_empty() && seen.insert(name) {
            result.push(name);
        }
    }

    result
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RoomError {
    #[error("a room needs at least one agent and cannot contain duplicates")]
    InvalidParticipants,
    #[error("`@{name}` is not in this room; participants: {participants}", participants = .participants.iter().map(|name| format!("@{name}")).collect::<Vec<_>>().join(", "))]
    UnknownMention {
        name: String,
        participants: Vec<String>,
    },
}

pub fn validate_participants(participants: &[AgentSnapshot]) -> Result<(), RoomError> {
    let unique: HashSet<_> = participants.iter().map(AgentSnapshot::agent_name).collect();
    if unique.is_empty() || unique.len() != participants.len() {
        return Err(RoomError::InvalidParticipants);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Room, RoomError, RoomId, validate_participants};
    use crate::agent::AgentSnapshot;

    fn room() -> Room {
        Room::new(
            RoomId::new(),
            vec![
                AgentSnapshot::fixture_for("explorer", "model", "Explore"),
                AgentSnapshot::fixture_for("reviewer", "model", "Review"),
            ],
            "Research".to_owned(),
            "Cite evidence".to_owned(),
        )
    }

    #[test]
    fn routes_unique_mentions_in_message_order() {
        let room = room();
        let routed = room
            .route(&super::RoomMessage::user(
                "@reviewer, compare with @explorer's idea. Then @reviewer",
            ))
            .unwrap();
        let names: Vec<_> = routed.iter().map(|agent| agent.agent_name()).collect();
        assert_eq!(names, ["reviewer", "explorer"]);
    }

    #[test]
    fn routes_unmentioned_messages_to_the_first_participant() {
        let room = room();
        let routed = room.route(&super::RoomMessage::user("hello")).unwrap();
        assert_eq!(routed[0].agent_name(), "explorer");
    }

    #[test]
    fn rejects_unknown_mentions() {
        let room = room();
        assert!(matches!(
            room.route(&super::RoomMessage::user("hello @writer")),
            Err(RoomError::UnknownMention { name, .. }) if name == "writer"
        ));
    }

    #[test]
    fn agent_authored_mentions_never_route() {
        let room = room();
        assert!(
            room.route(&super::RoomMessage::agent(
                "explorer",
                "@reviewer take over",
            ))
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn cleans_existing_self_attribution_for_display_and_model_context() {
        let message = super::RoomMessage::agent("explorer", "@EXPLORER: Hello");
        assert_eq!(message.visible_content(), "Hello");
        assert_eq!(message.as_provider_message().content(), "@explorer: Hello");

        let unrelated = super::RoomMessage::agent("explorer", "@exploration continues");
        assert_eq!(unrelated.visible_content(), "@exploration continues");
    }

    #[test]
    fn combines_agent_and_room_instructions() {
        let room = room();
        let instructions = super::room_instructions(&room, &room.participants()[0]);
        assert!(instructions.contains("Explore"));
        assert!(instructions.contains("Room instructions:\nCite evidence"));
    }

    #[test]
    fn requires_at_least_one_unique_participant() {
        let explorer = AgentSnapshot::fixture("Explore");
        assert!(validate_participants(std::slice::from_ref(&explorer)).is_ok());
        assert_eq!(
            validate_participants(&[]),
            Err(RoomError::InvalidParticipants)
        );
        assert_eq!(
            validate_participants(&[explorer.clone(), explorer]),
            Err(RoomError::InvalidParticipants)
        );
    }

    #[test]
    fn room_ids_round_trip() {
        let id = RoomId::new();
        assert_eq!(id.to_string().parse::<RoomId>().unwrap(), id);
    }
}
