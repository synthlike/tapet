use crate::config::{Agent, ProviderKind};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSnapshot {
    agent_name: String,
    provider_kind: ProviderKind,
    base_url: String,
    api_key_env: String,
    model: String,
    system_prompt: String,
}

impl AgentSnapshot {
    pub fn resolve(agent: &Agent) -> Self {
        Self {
            agent_name: agent.name().to_owned(),
            provider_kind: agent.provider_kind(),
            base_url: agent.base_url().to_owned(),
            api_key_env: agent.api_key_env().to_owned(),
            model: agent.model().to_owned(),
            system_prompt: agent.prompt().to_owned(),
        }
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn provider_kind(&self) -> ProviderKind {
        self.provider_kind
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
        provider_kind: ProviderKind,
        base_url: String,
        api_key_env: String,
        model: String,
        system_prompt: String,
    ) -> Self {
        Self {
            agent_name,
            provider_kind,
            base_url,
            api_key_env,
            model,
            system_prompt,
        }
    }
}

#[cfg(test)]
impl AgentSnapshot {
    pub fn fixture(prompt: &str) -> Self {
        Self::fixture_for("explorer", "test-model", prompt)
    }

    pub fn fixture_for(agent_name: &str, model: &str, prompt: &str) -> Self {
        Self {
            agent_name: agent_name.to_owned(),
            provider_kind: ProviderKind::OpenAi,
            base_url: "https://example.test/v1".to_owned(),
            api_key_env: "TEST_OPENAI_API_KEY".to_owned(),
            model: model.to_owned(),
            system_prompt: prompt.to_owned(),
        }
    }
}
