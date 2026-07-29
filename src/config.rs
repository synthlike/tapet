use serde::Deserialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SUPPORTED_VERSION: u32 = 1;
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug)]
pub struct Config {
    agents: BTreeMap<String, Agent>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenAi,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        match value {
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Agent {
    name: String,
    model_alias: String,
    provider_name: String,
    provider_kind: ProviderKind,
    base_url: String,
    api_key_env: String,
    model: String,
    prompt: String,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn model_alias(&self) -> &str {
        &self.model_alias
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
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

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    providers: BTreeMap<String, RawProvider>,
    models: BTreeMap<String, RawModel>,
    agents: BTreeMap<String, RawAgent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    #[serde(rename = "type")]
    kind: ProviderKind,
    #[serde(default = "default_openai_base_url")]
    base_url: String,
    api_key_env: String,
}

fn default_openai_base_url() -> String {
    DEFAULT_OPENAI_BASE_URL.to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    provider: String,
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    model: String,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
}

#[derive(Debug)]
pub enum ConfigError {
    ReadConfig {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    UnsupportedVersion(u32),
    NoProviders,
    NoModels,
    NoAgents,
    InvalidProvider {
        name: String,
        reason: &'static str,
    },
    InvalidModel {
        name: String,
        reason: &'static str,
    },
    InvalidAgent {
        name: String,
        reason: &'static str,
    },
    UnknownProvider {
        model: String,
        provider: String,
    },
    UnknownModel {
        agent: String,
        model: String,
    },
    ReadPrompt {
        agent: String,
        path: PathBuf,
        source: io::Error,
    },
    UnknownAgent {
        name: String,
        available: Vec<String>,
    },
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::ReadConfig {
            path: path.to_path_buf(),
            source,
        })?;
        let raw: RawConfig = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        if raw.version != SUPPORTED_VERSION {
            return Err(ConfigError::UnsupportedVersion(raw.version));
        }
        validate_definitions(&raw)?;

        let base_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut agents = BTreeMap::new();

        for (name, raw_agent) in raw.agents {
            validate_name("agent", &name).map_err(|reason| ConfigError::InvalidAgent {
                name: name.clone(),
                reason,
            })?;
            let model_alias = raw_agent.model.clone();
            let model = raw
                .models
                .get(&model_alias)
                .ok_or_else(|| ConfigError::UnknownModel {
                    agent: name.clone(),
                    model: model_alias.clone(),
                })?;
            let provider =
                raw.providers
                    .get(&model.provider)
                    .ok_or_else(|| ConfigError::UnknownProvider {
                        model: model_alias.clone(),
                        provider: model.provider.clone(),
                    })?;
            let prompt = resolve_prompt(&name, raw_agent, base_dir)?;
            agents.insert(
                name.clone(),
                Agent {
                    name,
                    model_alias,
                    provider_name: model.provider.clone(),
                    provider_kind: provider.kind,
                    base_url: provider.base_url.clone(),
                    api_key_env: provider.api_key_env.clone(),
                    model: model.model.clone(),
                    prompt,
                },
            );
        }

        Ok(Self { agents })
    }

    pub fn agent(&self, name: &str) -> Result<&Agent, ConfigError> {
        self.agents
            .get(name)
            .ok_or_else(|| ConfigError::UnknownAgent {
                name: name.to_owned(),
                available: self.agents.keys().cloned().collect(),
            })
    }

    pub fn agents(&self) -> impl Iterator<Item = &Agent> {
        self.agents.values()
    }
}

fn validate_definitions(raw: &RawConfig) -> Result<(), ConfigError> {
    if raw.providers.is_empty() {
        return Err(ConfigError::NoProviders);
    }
    if raw.models.is_empty() {
        return Err(ConfigError::NoModels);
    }
    if raw.agents.is_empty() {
        return Err(ConfigError::NoAgents);
    }

    for (name, provider) in &raw.providers {
        validate_name("provider", name).map_err(|reason| ConfigError::InvalidProvider {
            name: name.clone(),
            reason,
        })?;
        if provider.base_url.trim().is_empty() {
            return Err(ConfigError::InvalidProvider {
                name: name.clone(),
                reason: "`base_url` cannot be empty",
            });
        }
        if provider.api_key_env.trim().is_empty() {
            return Err(ConfigError::InvalidProvider {
                name: name.clone(),
                reason: "`api_key_env` cannot be empty",
            });
        }
    }

    for (name, model) in &raw.models {
        validate_name("model", name).map_err(|reason| ConfigError::InvalidModel {
            name: name.clone(),
            reason,
        })?;
        if model.model.trim().is_empty() {
            return Err(ConfigError::InvalidModel {
                name: name.clone(),
                reason: "`model` cannot be empty",
            });
        }
        if !raw.providers.contains_key(&model.provider) {
            return Err(ConfigError::UnknownProvider {
                model: name.clone(),
                provider: model.provider.clone(),
            });
        }
    }

    Ok(())
}

fn validate_name(kind: &'static str, name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() {
        return Err(match kind {
            "provider" => "provider names cannot be empty",
            "model" => "model names cannot be empty",
            _ => "agent names cannot be empty",
        });
    }
    Ok(())
}

fn resolve_prompt(name: &str, raw_agent: RawAgent, base_dir: &Path) -> Result<String, ConfigError> {
    let prompt = match (raw_agent.prompt, raw_agent.prompt_file) {
        (Some(prompt), None) => prompt,
        (None, Some(path)) => {
            let path = if path.is_absolute() {
                path
            } else {
                base_dir.join(path)
            };
            fs::read_to_string(&path).map_err(|source| ConfigError::ReadPrompt {
                agent: name.to_owned(),
                path,
                source,
            })?
        }
        (None, None) => {
            return Err(ConfigError::InvalidAgent {
                name: name.to_owned(),
                reason: "exactly one of `prompt` or `prompt_file` is required",
            });
        }
        (Some(_), Some(_)) => {
            return Err(ConfigError::InvalidAgent {
                name: name.to_owned(),
                reason: "`prompt` and `prompt_file` cannot be used together",
            });
        }
    };

    if prompt.trim().is_empty() {
        return Err(ConfigError::InvalidAgent {
            name: name.to_owned(),
            reason: "the prompt cannot be empty",
        });
    }

    Ok(prompt)
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadConfig { path, source } => write!(
                formatter,
                "cannot read configuration `{}`: {source}",
                path.display()
            ),
            Self::Parse { path, source } => write!(
                formatter,
                "invalid configuration `{}`: {source}",
                path.display()
            ),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "unsupported configuration version {version}; expected {SUPPORTED_VERSION}"
            ),
            Self::NoProviders => write!(formatter, "configuration must define a provider"),
            Self::NoModels => write!(formatter, "configuration must define a model"),
            Self::NoAgents => write!(formatter, "configuration must define at least one agent"),
            Self::InvalidProvider { name, reason } => {
                write!(formatter, "invalid provider `{name}`: {reason}")
            }
            Self::InvalidModel { name, reason } => {
                write!(formatter, "invalid model `{name}`: {reason}")
            }
            Self::InvalidAgent { name, reason } => {
                write!(formatter, "invalid agent `{name}`: {reason}")
            }
            Self::UnknownProvider { model, provider } => write!(
                formatter,
                "model `{model}` references unknown provider `{provider}`"
            ),
            Self::UnknownModel { agent, model } => {
                write!(
                    formatter,
                    "agent `{agent}` references unknown model `{model}`"
                )
            }
            Self::ReadPrompt {
                agent,
                path,
                source,
            } => write!(
                formatter,
                "cannot read prompt file `{}` for agent `{agent}`: {source}",
                path.display()
            ),
            Self::UnknownAgent { name, available } => write!(
                formatter,
                "unknown agent `{name}`; available agents: {}",
                available.join(", ")
            ),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadConfig { source, .. } | Self::ReadPrompt { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, DEFAULT_OPENAI_BASE_URL, ProviderKind};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "tapet-config-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn write(&self, relative_path: impl AsRef<Path>, contents: &str) -> PathBuf {
            let path = self.path.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }

    fn configuration(agent_tables: &str) -> String {
        format!(
            concat!(
                "version = 1\n",
                "[providers.openai]\n",
                "type = \"openai\"\n",
                "api_key_env = \"OPENAI_API_KEY\"\n",
                "[models.primary]\n",
                "provider = \"openai\"\n",
                "model = \"test-model\"\n",
                "{agent_tables}"
            ),
            agent_tables = agent_tables
        )
    }

    #[test]
    fn resolves_provider_model_and_inline_prompt() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            &configuration(
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore carefully\"\n",
            ),
        );

        let config = Config::load(path).unwrap();
        let agent = config.agent("explorer").unwrap();

        assert_eq!(agent.provider_kind(), ProviderKind::OpenAi);
        assert_eq!(agent.provider_name(), "openai");
        assert_eq!(agent.base_url(), DEFAULT_OPENAI_BASE_URL);
        assert_eq!(agent.api_key_env(), "OPENAI_API_KEY");
        assert_eq!(agent.model_alias(), "primary");
        assert_eq!(agent.model(), "test-model");
        assert_eq!(agent.prompt(), "Explore carefully");
    }

    #[test]
    fn loads_a_prompt_file_relative_to_the_configuration() {
        let directory = TestDirectory::new();
        directory.write("prompts/reviewer.md", "Review carefully\n");
        let path = directory.write(
            "tapet.toml",
            &configuration(
                "[agents.reviewer]\nmodel = \"primary\"\nprompt_file = \"prompts/reviewer.md\"\n",
            ),
        );

        let config = Config::load(path).unwrap();

        assert_eq!(
            config.agent("reviewer").unwrap().prompt(),
            "Review carefully\n"
        );
    }

    #[test]
    fn exposes_agents_in_deterministic_name_order() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            &configuration(concat!(
                "[agents.reviewer]\nmodel = \"primary\"\nprompt = \"Review\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n"
            )),
        );
        let config = Config::load(path).unwrap();

        assert_eq!(
            config
                .agents()
                .map(|agent| agent.name())
                .collect::<Vec<_>>(),
            ["explorer", "reviewer"]
        );
    }

    #[test]
    fn rejects_invalid_and_unknown_references() {
        let directory = TestDirectory::new();
        let unknown_provider = directory.write(
            "unknown-provider.toml",
            concat!(
                "version = 1\n",
                "[providers.openai]\ntype = \"openai\"\napi_key_env = \"KEY\"\n",
                "[models.primary]\nprovider = \"missing\"\nmodel = \"test\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n"
            ),
        );
        assert!(matches!(
            Config::load(unknown_provider),
            Err(ConfigError::UnknownProvider { model, provider })
                if model == "primary" && provider == "missing"
        ));

        let unknown_model = directory.write(
            "unknown-model.toml",
            &configuration("[agents.explorer]\nmodel = \"missing\"\nprompt = \"Explore\"\n"),
        );
        assert!(matches!(
            Config::load(unknown_model),
            Err(ConfigError::UnknownModel { agent, model })
                if agent == "explorer" && model == "missing"
        ));
    }

    #[test]
    fn rejects_invalid_toml_and_unknown_fields() {
        let directory = TestDirectory::new();
        let invalid = directory.write("invalid.toml", "version = [\n");
        assert!(matches!(
            Config::load(invalid),
            Err(ConfigError::Parse { .. })
        ));

        let unknown = directory.write(
            "unknown.toml",
            &configuration(
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\nunexpected = true\n",
            ),
        );
        assert!(matches!(
            Config::load(unknown),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn rejects_invalid_provider_and_agent_settings() {
        let directory = TestDirectory::new();
        let invalid_provider = directory.write(
            "provider.toml",
            concat!(
                "version = 1\n",
                "[providers.openai]\ntype = \"openai\"\napi_key_env = \"\"\n",
                "[models.primary]\nprovider = \"openai\"\nmodel = \"test\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n"
            ),
        );
        assert!(matches!(
            Config::load(invalid_provider),
            Err(ConfigError::InvalidProvider { reason, .. })
                if reason == "`api_key_env` cannot be empty"
        ));

        let no_prompt = directory.write(
            "agent.toml",
            &configuration("[agents.explorer]\nmodel = \"primary\"\n"),
        );
        assert!(matches!(
            Config::load(no_prompt),
            Err(ConfigError::InvalidAgent { reason, .. })
                if reason == "exactly one of `prompt` or `prompt_file` is required"
        ));
    }

    #[test]
    fn reports_unknown_agents_in_deterministic_order() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            &configuration(concat!(
                "[agents.reviewer]\nmodel = \"primary\"\nprompt = \"Review\"\n",
                "[agents.explorer]\nmodel = \"primary\"\nprompt = \"Explore\"\n"
            )),
        );
        let config = Config::load(path).unwrap();

        assert_eq!(
            config.agent("missing").unwrap_err().to_string(),
            "unknown agent `missing`; available agents: explorer, reviewer"
        );
    }
}
