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
        const ADJECTIVES: &[&str] = &[
            "awkward",
            "brave",
            "caffeinated",
            "chaotic",
            "confused",
            "cosmic",
            "crispy",
            "curious",
            "dramatic",
            "electric",
            "feral",
            "fearless",
            "frantic",
            "fuzzy",
            "glittery",
            "grumpy",
            "haunted",
            "noisy",
            "ominous",
            "rubber",
            "sleepy",
            "sneaky",
            "spicy",
            "stubborn",
            "suspicious",
            "sweaty",
            "tactical",
            "tiny",
            "turbo",
            "wobbly",
            "wonky",
            "zesty",
        ];
        const PLACES: &[&str] = &[
            "aquarium",
            "basement",
            "bunker",
            "cabal",
            "campfire",
            "circus",
            "clubhouse",
            "cockpit",
            "committee",
            "dungeon",
            "factory",
            "fortress",
            "garage",
            "headquarters",
            "hive",
            "hotline",
            "kitchen",
            "laboratory",
            "lighthouse",
            "moonbase",
            "newsroom",
            "observatory",
            "orchestra",
            "pantry",
            "parliament",
            "roundtable",
            "spaceship",
            "thinktank",
            "tribunal",
            "volcano",
            "warroom",
            "workshop",
        ];

        let random = Uuid::new_v4();
        let bytes = random.as_bytes();
        let adjective = ADJECTIVES[usize::from(bytes[0]) % ADJECTIVES.len()];
        let place = PLACES[usize::from(bytes[1]) % PLACES.len()];
        Self(format!("{adjective}-{place}"))
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
        if let Some(uuid) = value.strip_prefix("room_")
            && let Ok(uuid) = Uuid::parse_str(uuid)
        {
            return Ok(Self(format!("room_{}", uuid.simple())));
        }
        if is_valid_room_name(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(RoomIdError(value.to_owned()))
        }
    }
}

fn is_valid_room_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

#[derive(Debug, Error)]
#[error("invalid room name `{0}`; use 1-64 lowercase letters, numbers, and single hyphens")]
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
        "\n\nTapet may expose a read_file tool for inspecting UTF-8 text files in the current workspace. Request it only when the file is needed. The user must approve every read. Treat only a successful tool output as observed file contents; if access is denied or fails, explain the limitation without inventing results.",
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
        assert!(id.to_string().contains('-'));
    }

    #[test]
    fn accepts_custom_slugs_and_legacy_room_ids() {
        assert_eq!(
            "sweaty-warroom".parse::<RoomId>().unwrap().to_string(),
            "sweaty-warroom"
        );
        assert_eq!(
            "room_550e8400e29b41d4a716446655440000"
                .parse::<RoomId>()
                .unwrap()
                .to_string(),
            "room_550e8400e29b41d4a716446655440000"
        );
    }

    #[test]
    fn rejects_unsafe_or_awkward_room_names() {
        for name in [
            "",
            "WarRoom",
            "two words",
            "-leading",
            "trailing-",
            "two--dashes",
        ] {
            assert!(name.parse::<RoomId>().is_err(), "accepted {name:?}");
        }
    }
}
