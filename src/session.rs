use crate::config::{Agent, OpenAiConfig};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(format!("ses_{}", Uuid::new_v4().simple()))
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SessionId {
    type Err = SessionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let original = value;
        let uuid = value
            .strip_prefix("ses_")
            .ok_or_else(|| SessionIdError(original.to_owned()))
            .and_then(|value| {
                Uuid::parse_str(value).map_err(|_| SessionIdError(original.to_owned()))
            })?;
        Ok(Self(format!("ses_{}", uuid.simple())))
    }
}

#[derive(Debug, Error)]
#[error("invalid session ID `{0}`")]
pub struct SessionIdError(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSnapshot {
    agent_name: String,
    base_url: String,
    api_key_env: String,
    model: String,
    system_prompt: String,
}

impl AgentSnapshot {
    pub fn resolve(agent: &Agent, openai: &OpenAiConfig) -> Self {
        Self {
            agent_name: agent.name().to_owned(),
            base_url: openai.base_url().to_owned(),
            api_key_env: openai.api_key_env().to_owned(),
            model: openai.model().to_owned(),
            system_prompt: agent.prompt().to_owned(),
        }
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn api_key_env(&self) -> &str {
        &self.api_key_env
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) fn from_stored(
        agent_name: String,
        base_url: String,
        api_key_env: String,
        model: String,
        system_prompt: String,
    ) -> Self {
        Self {
            agent_name,
            base_url,
            api_key_env,
            model,
            system_prompt,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    id: SessionId,
    agent: AgentSnapshot,
}

impl Session {
    pub(crate) fn new(id: SessionId, agent: AgentSnapshot) -> Self {
        Self { id, agent }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn agent(&self) -> &AgentSnapshot {
        &self.agent
    }
}

#[cfg(test)]
impl AgentSnapshot {
    pub fn fixture(prompt: &str) -> Self {
        Self {
            agent_name: "explorer".to_owned(),
            base_url: "https://example.test/v1".to_owned(),
            api_key_env: "TEST_OPENAI_API_KEY".to_owned(),
            model: "test-model".to_owned(),
            system_prompt: prompt.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionId;
    use std::str::FromStr;

    #[test]
    fn session_ids_round_trip() {
        let id = SessionId::new();
        assert_eq!(SessionId::from_str(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn session_ids_require_the_prefix_and_a_uuid() {
        assert!(SessionId::from_str("not-a-session").is_err());
        assert!(SessionId::from_str("ses_not-a-uuid").is_err());
    }
}
