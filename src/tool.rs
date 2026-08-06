use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const MAX_TOOL_CALLS_PER_TURN: usize = 4;
const MAX_FILE_BYTES: u64 = 128 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadFileRequest {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArguments {
    path: String,
}

#[derive(Debug, Serialize)]
pub struct ReadFileOutput {
    pub ok: bool,
    pub path: String,
    pub content: String,
    pub bytes: u64,
    pub lines: u64,
}

impl ReadFileRequest {
    pub fn parse(arguments: &str) -> Result<Self, ToolError> {
        let arguments: ReadFileArguments = serde_json::from_str(arguments)?;
        let path = Path::new(&arguments.path);
        if arguments.path.trim().is_empty() {
            return Err(ToolError::EmptyPath);
        }
        if path.is_absolute() {
            return Err(ToolError::AbsolutePath);
        }
        if path_is_unsafe(path) {
            return Err(ToolError::UnsafePath(arguments.path));
        }

        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn display_path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    pub fn execute(&self, workspace: &Path) -> Result<ReadFileOutput, ToolError> {
        let workspace = workspace
            .canonicalize()
            .map_err(|source| ToolError::Workspace { source })?;
        let requested = workspace.join(&self.path);
        let resolved = requested.canonicalize().map_err(|source| ToolError::Open {
            path: self.display_path(),
            source,
        })?;
        if !resolved.starts_with(&workspace) {
            return Err(ToolError::OutsideWorkspace(self.display_path()));
        }
        let relative = resolved
            .strip_prefix(&workspace)
            .expect("workspace prefix was checked");
        if path_is_unsafe(relative) {
            return Err(ToolError::UnsafePath(self.display_path()));
        }

        let mut file = File::open(&resolved).map_err(|source| ToolError::Open {
            path: self.display_path(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| ToolError::Open {
            path: self.display_path(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(ToolError::NotAFile(self.display_path()));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(ToolError::TooLarge {
                path: self.display_path(),
                limit: MAX_FILE_BYTES,
            });
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| ToolError::Read {
                path: self.display_path(),
                source,
            })?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(ToolError::TooLarge {
                path: self.display_path(),
                limit: MAX_FILE_BYTES,
            });
        }
        let byte_count = bytes.len() as u64;
        let content =
            String::from_utf8(bytes).map_err(|_| ToolError::NotUtf8(self.display_path()))?;
        let lines = content.lines().count() as u64;

        Ok(ReadFileOutput {
            ok: true,
            path: self.display_path(),
            content,
            bytes: byte_count,
            lines,
        })
    }
}

pub fn error_output(error: &impl std::fmt::Display) -> String {
    serde_json::json!({ "ok": false, "error": error.to_string() }).to_string()
}

fn is_sensitive_component(component: &str) -> bool {
    let lowercase = component.to_ascii_lowercase();
    matches!(lowercase.as_str(), ".git" | ".tapet" | ".env")
        || lowercase.starts_with(".env.")
        || matches!(
            lowercase.as_str(),
            "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519"
        )
        || matches!(
            Path::new(&lowercase)
                .extension()
                .and_then(|value| value.to_str()),
            Some("pem" | "key" | "p12" | "pfx")
        )
}

fn path_is_unsafe(path: &Path) -> bool {
    path.components().any(|component| {
        !matches!(component, Component::Normal(_))
            || component
                .as_os_str()
                .to_str()
                .is_none_or(is_sensitive_component)
    })
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("read_file arguments are invalid: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("read_file requires a non-empty path")]
    EmptyPath,
    #[error("read_file only accepts workspace-relative paths")]
    AbsolutePath,
    #[error("path `{0}` contains traversal or a protected component")]
    UnsafePath(String),
    #[error("could not resolve the current workspace: {source}")]
    Workspace { source: io::Error },
    #[error("path `{0}` resolves outside the current workspace")]
    OutsideWorkspace(String),
    #[error("could not open `{path}`: {source}")]
    Open { path: String, source: io::Error },
    #[error("`{0}` is not a regular file")]
    NotAFile(String),
    #[error("`{path}` is larger than the {limit}-byte read limit")]
    TooLarge { path: String, limit: u64 },
    #[error("could not read `{path}`: {source}")]
    Read { path: String, source: io::Error },
    #[error("`{0}` is not UTF-8 text")]
    NotUtf8(String),
}

#[cfg(test)]
mod tests {
    use super::{ReadFileRequest, ToolError};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn reads_bounded_utf8_workspace_files() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("src/lib.rs"), "one\ntwo\n").unwrap();

        let request = ReadFileRequest::parse(r#"{"path":"src/lib.rs"}"#).unwrap();
        let output = request.execute(workspace.path()).unwrap();

        assert_eq!(output.path, "src/lib.rs");
        assert_eq!(output.content, "one\ntwo\n");
        assert_eq!(output.bytes, 8);
        assert_eq!(output.lines, 2);
    }

    #[test]
    fn rejects_traversal_absolute_and_sensitive_paths() {
        assert!(matches!(
            ReadFileRequest::parse(r#"{"path":"../secret"}"#),
            Err(ToolError::UnsafePath(_))
        ));
        assert!(matches!(
            ReadFileRequest::parse(r#"{"path":"/etc/passwd"}"#),
            Err(ToolError::AbsolutePath)
        ));
        assert!(matches!(
            ReadFileRequest::parse(r#"{"path":".env.local"}"#),
            Err(ToolError::UnsafePath(_))
        ));
        assert!(matches!(
            ReadFileRequest::parse(r#"{"path":"keys/server.pem"}"#),
            Err(ToolError::UnsafePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_escape_the_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            workspace.path().join("link.txt"),
        )
        .unwrap();

        let request = ReadFileRequest::parse(r#"{"path":"link.txt"}"#).unwrap();
        assert!(matches!(
            request.execute(workspace.path()),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_aliases_for_protected_paths() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join(".git")).unwrap();
        fs::write(workspace.path().join(".git/config"), "secret").unwrap();
        symlink(
            workspace.path().join(".git"),
            workspace.path().join("metadata"),
        )
        .unwrap();

        let request = ReadFileRequest::parse(r#"{"path":"metadata/config"}"#).unwrap();
        assert!(matches!(
            request.execute(workspace.path()),
            Err(ToolError::UnsafePath(_))
        ));
    }

    #[test]
    fn rejects_non_utf8_and_oversized_files() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("binary"), [0xff, 0xfe]).unwrap();
        fs::write(workspace.path().join("large"), vec![b'x'; 128 * 1024 + 1]).unwrap();

        let binary = ReadFileRequest::parse(r#"{"path":"binary"}"#).unwrap();
        assert!(matches!(
            binary.execute(workspace.path()),
            Err(ToolError::NotUtf8(_))
        ));
        let large = ReadFileRequest::parse(r#"{"path":"large"}"#).unwrap();
        assert!(matches!(
            large.execute(workspace.path()),
            Err(ToolError::TooLarge { .. })
        ));
    }
}
