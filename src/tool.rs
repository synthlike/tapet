use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const MAX_TOOL_CALLS_PER_TURN: usize = 4;
const MAX_FILE_BYTES: u64 = 128 * 1024;
const MAX_LIST_ENTRIES: usize = 500;

/// A parsed, validated request for one of the tools exposed to agents.
/// Dispatch lives here rather than in `main.rs` so approval copy and
/// execution stay next to the validation rules they depend on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRequest {
    ReadFile(ReadFileRequest),
    ListFiles(ListFilesRequest),
}

pub struct ToolOutcome {
    pub json: String,
    pub bytes: u64,
    pub lines: u64,
}

impl ToolRequest {
    pub fn parse(name: &str, arguments: &str) -> Result<Self, ToolError> {
        match name {
            "read_file" => Ok(Self::ReadFile(ReadFileRequest::parse(arguments)?)),
            "list_files" => Ok(Self::ListFiles(ListFilesRequest::parse(arguments)?)),
            other => Err(ToolError::UnknownTool(other.to_owned())),
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Self::ReadFile(_) => "read",
            Self::ListFiles(_) => "list",
        }
    }

    pub fn count_label(&self, count: u64) -> &'static str {
        match (self, count) {
            (Self::ReadFile(_), 1) => "line",
            (Self::ReadFile(_), _) => "lines",
            (Self::ListFiles(_), 1) => "entry",
            (Self::ListFiles(_), _) => "entries",
        }
    }

    pub fn display_path(&self) -> String {
        match self {
            Self::ReadFile(request) => request.display_path(),
            Self::ListFiles(request) => request.display_path(),
        }
    }

    pub fn execute(&self, workspace: &Path) -> Result<ToolOutcome, ToolError> {
        match self {
            Self::ReadFile(request) => request.execute(workspace).map(ToolOutcome::from),
            Self::ListFiles(request) => request.execute(workspace).map(ToolOutcome::from),
        }
    }
}

impl From<ReadFileOutput> for ToolOutcome {
    fn from(output: ReadFileOutput) -> Self {
        Self {
            bytes: output.bytes,
            lines: output.lines,
            json: serde_json::to_string(&output).expect("read_file output is serializable"),
        }
    }
}

impl From<ListFilesOutput> for ToolOutcome {
    fn from(output: ListFilesOutput) -> Self {
        Self {
            bytes: output.bytes,
            lines: output.entries.len() as u64,
            json: serde_json::to_string(&output).expect("list_files output is serializable"),
        }
    }
}

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
        let path = parse_workspace_path(&arguments.path)?;

        Ok(Self { path })
    }

    pub fn display_path(&self) -> String {
        display_workspace_path(&self.path)
    }

    pub fn execute(&self, workspace: &Path) -> Result<ReadFileOutput, ToolError> {
        let resolved = resolve_within_workspace(workspace, &self.path, &self.display_path())?;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListFilesRequest {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFilesArguments {
    path: String,
}

#[derive(Debug, Serialize)]
pub struct ListEntry {
    pub name: String,
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ListFilesOutput {
    pub ok: bool,
    pub path: String,
    pub entries: Vec<ListEntry>,
    #[serde(skip)]
    pub bytes: u64,
}

impl ListFilesRequest {
    pub fn parse(arguments: &str) -> Result<Self, ToolError> {
        let arguments: ListFilesArguments = serde_json::from_str(arguments)?;
        let path = parse_workspace_path(&arguments.path)?;

        Ok(Self { path })
    }

    pub fn display_path(&self) -> String {
        display_workspace_path(&self.path)
    }

    pub fn execute(&self, workspace: &Path) -> Result<ListFilesOutput, ToolError> {
        let resolved = resolve_within_workspace(workspace, &self.path, &self.display_path())?;

        let metadata = fs::metadata(&resolved).map_err(|source| ToolError::Open {
            path: self.display_path(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::NotADirectory(self.display_path()));
        }

        let mut entries = Vec::new();
        for entry in fs::read_dir(&resolved).map_err(|source| ToolError::Open {
            path: self.display_path(),
            source,
        })? {
            let entry = entry.map_err(|source| ToolError::Read {
                path: self.display_path(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if is_sensitive_component(name) {
                continue;
            }
            let file_type = entry.file_type().map_err(|source| ToolError::Read {
                path: self.display_path(),
                source,
            })?;
            let kind = if file_type.is_dir() { "dir" } else { "file" };
            entries.push(ListEntry {
                name: name.to_owned(),
                kind,
            });
            if entries.len() > MAX_LIST_ENTRIES {
                return Err(ToolError::TooManyEntries {
                    path: self.display_path(),
                    limit: MAX_LIST_ENTRIES,
                });
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));

        let bytes = entries
            .iter()
            .map(|entry| entry.name.len() as u64)
            .sum::<u64>();

        Ok(ListFilesOutput {
            ok: true,
            path: self.display_path(),
            entries,
            bytes,
        })
    }
}

fn display_workspace_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn parse_workspace_path(raw: &str) -> Result<PathBuf, ToolError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ToolError::EmptyPath);
    }
    // "." names the workspace root; represent it as an empty relative path so
    // it doesn't trip the traversal check below (a lone `.` component isn't `Normal`).
    if trimmed == "." {
        return Ok(PathBuf::new());
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(ToolError::AbsolutePath);
    }
    if path_is_unsafe(path) {
        return Err(ToolError::UnsafePath(raw.to_owned()));
    }
    Ok(path.to_path_buf())
}

fn resolve_within_workspace(
    workspace: &Path,
    path: &Path,
    display: &str,
) -> Result<PathBuf, ToolError> {
    let workspace = workspace
        .canonicalize()
        .map_err(|source| ToolError::Workspace { source })?;
    let requested = workspace.join(path);
    let resolved = requested.canonicalize().map_err(|source| ToolError::Open {
        path: display.to_owned(),
        source,
    })?;
    if !resolved.starts_with(&workspace) {
        return Err(ToolError::OutsideWorkspace(display.to_owned()));
    }
    let relative = resolved
        .strip_prefix(&workspace)
        .expect("workspace prefix was checked");
    if path_is_unsafe(relative) {
        return Err(ToolError::UnsafePath(display.to_owned()));
    }
    Ok(resolved)
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
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("tool arguments are invalid: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("path must not be empty")]
    EmptyPath,
    #[error("only workspace-relative paths are accepted")]
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
    #[error("`{0}` is not a directory")]
    NotADirectory(String),
    #[error("`{path}` is larger than the {limit}-byte read limit")]
    TooLarge { path: String, limit: u64 },
    #[error("`{path}` has more than {limit} entries")]
    TooManyEntries { path: String, limit: usize },
    #[error("could not read `{path}`: {source}")]
    Read { path: String, source: io::Error },
    #[error("`{0}` is not UTF-8 text")]
    NotUtf8(String),
}

#[cfg(test)]
mod tests {
    use super::{ListFilesRequest, ReadFileRequest, ToolError, ToolRequest};
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

    #[test]
    fn lists_directory_entries_sorted_and_typed() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("src/lib.rs"), "").unwrap();
        fs::write(workspace.path().join("src/main.rs"), "").unwrap();
        fs::create_dir(workspace.path().join("src/nested")).unwrap();

        let request = ListFilesRequest::parse(r#"{"path":"src"}"#).unwrap();
        let output = request.execute(workspace.path()).unwrap();

        let names_and_kinds: Vec<_> = output
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind))
            .collect();
        assert_eq!(
            names_and_kinds,
            [("lib.rs", "file"), ("main.rs", "file"), ("nested", "dir")]
        );
    }

    #[test]
    fn list_files_hides_sensitive_entries_and_rejects_files() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join(".git")).unwrap();
        fs::write(workspace.path().join("visible.txt"), "").unwrap();

        let request = ListFilesRequest::parse(r#"{"path":"."}"#).unwrap();
        let output = request.execute(workspace.path()).unwrap();
        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.entries[0].name, "visible.txt");

        fs::write(workspace.path().join("visible.txt"), "content").unwrap();
        let not_a_directory = ListFilesRequest::parse(r#"{"path":"visible.txt"}"#).unwrap();
        assert!(matches!(
            not_a_directory.execute(workspace.path()),
            Err(ToolError::NotADirectory(_))
        ));
    }

    #[test]
    fn dispatches_unknown_tool_names_by_name() {
        assert!(matches!(
            ToolRequest::parse("delete_everything", "{}"),
            Err(ToolError::UnknownTool(name)) if name == "delete_everything"
        ));
        assert!(matches!(
            ToolRequest::parse("list_files", r#"{"path":"."}"#),
            Ok(ToolRequest::ListFiles(_))
        ));
    }
}
