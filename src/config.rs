use serde::Deserialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug)]
pub struct Config {
    agents: BTreeMap<String, Agent>,
}

#[derive(Debug)]
pub struct Agent {
    name: String,
    prompt: String,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    agents: BTreeMap<String, RawAgent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
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
    NoAgents,
    InvalidAgent {
        name: String,
        reason: &'static str,
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
        if raw.agents.is_empty() {
            return Err(ConfigError::NoAgents);
        }

        let base_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut agents = BTreeMap::new();

        for (name, raw_agent) in raw.agents {
            if name.trim().is_empty() {
                return Err(ConfigError::InvalidAgent {
                    name,
                    reason: "agent names cannot be empty",
                });
            }

            let prompt = resolve_prompt(&name, raw_agent, base_dir)?;
            agents.insert(name.clone(), Agent { name, prompt });
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
            Self::ReadConfig { path, source } => {
                write!(
                    formatter,
                    "cannot read configuration `{}`: {source}",
                    path.display()
                )
            }
            Self::Parse { path, source } => {
                write!(
                    formatter,
                    "invalid configuration `{}`: {source}",
                    path.display()
                )
            }
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "unsupported configuration version {version}; expected {SUPPORTED_VERSION}"
            ),
            Self::NoAgents => write!(formatter, "configuration must define at least one agent"),
            Self::InvalidAgent { name, reason } => {
                write!(formatter, "invalid agent `{name}`: {reason}")
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
            Self::UnsupportedVersion(_)
            | Self::NoAgents
            | Self::InvalidAgent { .. }
            | Self::UnknownAgent { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError};
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

    #[test]
    fn loads_an_inline_prompt() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            "version = 1\n[agents.explorer]\nprompt = \"Explore carefully\"\n",
        );

        let config = Config::load(path).unwrap();

        assert_eq!(
            config.agent("explorer").unwrap().prompt(),
            "Explore carefully"
        );
    }

    #[test]
    fn loads_a_prompt_file_relative_to_the_configuration() {
        let directory = TestDirectory::new();
        directory.write("prompts/reviewer.md", "Review carefully\n");
        let path = directory.write(
            "tapet.toml",
            "version = 1\n[agents.reviewer]\nprompt_file = \"prompts/reviewer.md\"\n",
        );

        let config = Config::load(path).unwrap();

        assert_eq!(
            config.agent("reviewer").unwrap().prompt(),
            "Review carefully\n"
        );
    }

    #[test]
    fn rejects_invalid_toml() {
        let directory = TestDirectory::new();
        let path = directory.write("tapet.toml", "version = [\n");

        assert!(matches!(Config::load(path), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn rejects_unknown_fields() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            "version = 1\nunexpected = true\n[agents.explorer]\nprompt = \"Explore\"\n",
        );

        assert!(matches!(Config::load(path), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn rejects_an_agent_without_a_prompt() {
        let directory = TestDirectory::new();
        let path = directory.write("tapet.toml", "version = 1\n[agents.explorer]\n");

        assert!(matches!(
            Config::load(path),
            Err(ConfigError::InvalidAgent {
                name,
                reason: "exactly one of `prompt` or `prompt_file` is required"
            }) if name == "explorer"
        ));
    }

    #[test]
    fn rejects_a_missing_prompt_file() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            "version = 1\n[agents.explorer]\nprompt_file = \"missing.md\"\n",
        );

        assert!(matches!(
            Config::load(path),
            Err(ConfigError::ReadPrompt { agent, .. }) if agent == "explorer"
        ));
    }

    #[test]
    fn rejects_both_prompt_sources() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            concat!(
                "version = 1\n",
                "[agents.explorer]\n",
                "prompt = \"Explore\"\n",
                "prompt_file = \"explorer.md\"\n"
            ),
        );

        assert!(matches!(
            Config::load(path),
            Err(ConfigError::InvalidAgent {
                reason: "`prompt` and `prompt_file` cannot be used together",
                ..
            })
        ));
    }

    #[test]
    fn reports_unknown_agents_in_deterministic_order() {
        let directory = TestDirectory::new();
        let path = directory.write(
            "tapet.toml",
            concat!(
                "version = 1\n",
                "[agents.reviewer]\n",
                "prompt = \"Review\"\n",
                "[agents.explorer]\n",
                "prompt = \"Explore\"\n"
            ),
        );
        let config = Config::load(path).unwrap();

        let error = config.agent("missing").unwrap_err();

        assert_eq!(
            error.to_string(),
            "unknown agent `missing`; available agents: explorer, reviewer"
        );
    }
}
