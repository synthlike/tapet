use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const MAX_TOOL_CALLS_PER_TURN: usize = 4;
const MAX_FILE_BYTES: u64 = 128 * 1024;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_MATCHES: usize = 200;
const MAX_MATCH_LINE_CHARS: usize = 200;

/// A parsed, validated request for one of the tools exposed to agents.
/// Dispatch lives here rather than in `main.rs` so approval copy and
/// execution stay next to the validation rules they depend on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRequest {
    ReadFile(ReadFileRequest),
    ListFiles(ListFilesRequest),
    WriteFile(WriteFileRequest),
    SearchFiles(SearchFilesRequest),
}

pub struct ToolOutcome {
    pub json: String,
    pub bytes: u64,
    pub lines: u64,
}

/// What to show the user before a tool runs. Built without side effects for
/// read-only tools; `write_file` reads the current on-disk content to build
/// its diff, which is the one case where approval requires a disk read.
pub enum ToolApprovalPreview {
    Path {
        verb: &'static str,
        path: String,
    },
    Diff {
        verb: &'static str,
        path: String,
        is_new_file: bool,
        diff: Vec<DiffLine>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
}

impl ToolRequest {
    pub fn parse(name: &str, arguments: &str) -> Result<Self, ToolError> {
        match name {
            "read_file" => Ok(Self::ReadFile(ReadFileRequest::parse(arguments)?)),
            "list_files" => Ok(Self::ListFiles(ListFilesRequest::parse(arguments)?)),
            "write_file" => Ok(Self::WriteFile(WriteFileRequest::parse(arguments)?)),
            "search_files" => Ok(Self::SearchFiles(SearchFilesRequest::parse(arguments)?)),
            other => Err(ToolError::UnknownTool(other.to_owned())),
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Self::ReadFile(_) => "read",
            Self::ListFiles(_) => "list",
            Self::WriteFile(_) => "write",
            Self::SearchFiles(_) => "search",
        }
    }

    pub fn count_label(&self, count: u64) -> &'static str {
        match (self, count) {
            (Self::ReadFile(_) | Self::WriteFile(_), 1) => "line",
            (Self::ReadFile(_) | Self::WriteFile(_), _) => "lines",
            (Self::ListFiles(_), 1) => "entry",
            (Self::ListFiles(_), _) => "entries",
            (Self::SearchFiles(_), 1) => "match",
            (Self::SearchFiles(_), _) => "matches",
        }
    }

    pub fn display_path(&self) -> String {
        match self {
            Self::ReadFile(request) => request.display_path(),
            Self::ListFiles(request) => request.display_path(),
            Self::WriteFile(request) => request.display_path(),
            Self::SearchFiles(request) => request.display_path(),
        }
    }

    pub fn approval_preview(&self, workspace: &Path) -> Result<ToolApprovalPreview, ToolError> {
        match self {
            Self::WriteFile(request) => request.approval_preview(workspace),
            _ => Ok(ToolApprovalPreview::Path {
                verb: self.verb(),
                path: self.display_path(),
            }),
        }
    }

    pub fn execute(&self, workspace: &Path) -> Result<ToolOutcome, ToolError> {
        match self {
            Self::ReadFile(request) => request.execute(workspace).map(ToolOutcome::from),
            Self::ListFiles(request) => request.execute(workspace).map(ToolOutcome::from),
            Self::WriteFile(request) => request.execute(workspace).map(ToolOutcome::from),
            Self::SearchFiles(request) => request.execute(workspace).map(ToolOutcome::from),
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

impl From<WriteFileOutput> for ToolOutcome {
    fn from(output: WriteFileOutput) -> Self {
        Self {
            bytes: output.bytes,
            lines: output.lines,
            json: serde_json::to_string(&output).expect("write_file output is serializable"),
        }
    }
}

impl From<SearchFilesOutput> for ToolOutcome {
    fn from(output: SearchFilesOutput) -> Self {
        Self {
            bytes: output
                .matches
                .iter()
                .map(|entry| entry.content.len() as u64)
                .sum(),
            lines: output.matches.len() as u64,
            json: serde_json::to_string(&output).expect("search_files output is serializable"),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteFileRequest {
    path: PathBuf,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArguments {
    path: String,
    content: String,
}

#[derive(Debug, Serialize)]
pub struct WriteFileOutput {
    pub ok: bool,
    pub path: String,
    pub bytes: u64,
    pub lines: u64,
}

impl WriteFileRequest {
    pub fn parse(arguments: &str) -> Result<Self, ToolError> {
        let arguments: WriteFileArguments = serde_json::from_str(arguments)?;
        let path = parse_workspace_path(&arguments.path)?;

        Ok(Self {
            path,
            content: arguments.content,
        })
    }

    pub fn display_path(&self) -> String {
        display_workspace_path(&self.path)
    }

    pub fn approval_preview(&self, workspace: &Path) -> Result<ToolApprovalPreview, ToolError> {
        validate_write_size(&self.content, &self.display_path())?;
        let target = resolve_write_target(workspace, &self.path, &self.display_path())?;
        let existing = read_existing_utf8(&target, &self.display_path())?;
        let is_new_file = existing.is_none();
        let diff = compute_diff(existing.as_deref().unwrap_or(""), &self.content);

        Ok(ToolApprovalPreview::Diff {
            verb: "write",
            path: self.display_path(),
            is_new_file,
            diff,
        })
    }

    pub fn execute(&self, workspace: &Path) -> Result<WriteFileOutput, ToolError> {
        validate_write_size(&self.content, &self.display_path())?;
        let target = resolve_write_target(workspace, &self.path, &self.display_path())?;

        fs::write(&target, &self.content).map_err(|source| ToolError::Write {
            path: self.display_path(),
            source,
        })?;

        Ok(WriteFileOutput {
            ok: true,
            path: self.display_path(),
            bytes: self.content.len() as u64,
            lines: self.content.lines().count() as u64,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchFilesRequest {
    query: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchFilesArguments {
    query: String,
    path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: u64,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct SearchFilesOutput {
    pub ok: bool,
    pub query: String,
    pub matches: Vec<SearchMatch>,
}

impl SearchFilesRequest {
    pub fn parse(arguments: &str) -> Result<Self, ToolError> {
        let arguments: SearchFilesArguments = serde_json::from_str(arguments)?;
        if arguments.query.is_empty() {
            return Err(ToolError::EmptyQuery);
        }
        let path = parse_workspace_path(arguments.path.as_deref().unwrap_or("."))?;

        Ok(Self {
            query: arguments.query,
            path,
        })
    }

    pub fn display_path(&self) -> String {
        format!("{:?} in {}", self.query, display_workspace_path(&self.path))
    }

    pub fn execute(&self, workspace: &Path) -> Result<SearchFilesOutput, ToolError> {
        let workspace_root = workspace
            .canonicalize()
            .map_err(|source| ToolError::Workspace { source })?;
        let resolved_root = resolve_within_workspace(workspace, &self.path, &self.display_path())?;
        if !resolved_root.is_dir() {
            return Err(ToolError::NotADirectory(self.display_path()));
        }

        let mut matches = Vec::new();
        let mut directories = vec![resolved_root];
        while let Some(directory) = directories.pop() {
            let entries = fs::read_dir(&directory).map_err(|source| ToolError::Open {
                path: self.display_path(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| ToolError::Read {
                    path: self.display_path(),
                    source,
                })?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if is_sensitive_component(&name) {
                    continue;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    if !is_noisy_directory(&name) {
                        directories.push(entry.path());
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue; // symlinks and other special entries
                }
                self.search_file(&entry.path(), &workspace_root, &mut matches)?;
            }
        }

        Ok(SearchFilesOutput {
            ok: true,
            query: self.query.clone(),
            matches,
        })
    }

    fn search_file(
        &self,
        path: &Path,
        workspace_root: &Path,
        matches: &mut Vec<SearchMatch>,
    ) -> Result<(), ToolError> {
        let Ok(metadata) = fs::metadata(path) else {
            return Ok(());
        };
        if metadata.len() > MAX_FILE_BYTES {
            return Ok(());
        }
        let Ok(bytes) = fs::read(path) else {
            return Ok(());
        };
        let Ok(content) = String::from_utf8(bytes) else {
            return Ok(());
        };
        let relative = path
            .strip_prefix(workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        for (index, line) in content.lines().enumerate() {
            if !line.contains(&self.query) {
                continue;
            }
            matches.push(SearchMatch {
                path: relative.clone(),
                line: (index + 1) as u64,
                content: truncate_match_line(line),
            });
            if matches.len() > MAX_SEARCH_MATCHES {
                return Err(ToolError::TooManyMatches {
                    query: self.query.clone(),
                    limit: MAX_SEARCH_MATCHES,
                });
            }
        }
        Ok(())
    }
}

fn truncate_match_line(line: &str) -> String {
    if line.chars().count() <= MAX_MATCH_LINE_CHARS {
        line.to_owned()
    } else {
        let mut truncated: String = line.chars().take(MAX_MATCH_LINE_CHARS).collect();
        truncated.push('…');
        truncated
    }
}

fn is_noisy_directory(name: &str) -> bool {
    matches!(
        name,
        "target" | "node_modules" | ".venv" | "venv" | "dist" | "build" | "__pycache__"
    )
}

fn validate_write_size(content: &str, display: &str) -> Result<(), ToolError> {
    if content.len() as u64 > MAX_FILE_BYTES {
        return Err(ToolError::TooLarge {
            path: display.to_owned(),
            limit: MAX_FILE_BYTES,
        });
    }
    Ok(())
}

/// Resolves where a `write_file` call would land, tolerating a target that
/// doesn't exist yet (read/list resolution requires the path to already
/// exist). The parent directory must already exist; Tapet never creates
/// directories implicitly.
fn resolve_write_target(
    workspace: &Path,
    path: &Path,
    display: &str,
) -> Result<PathBuf, ToolError> {
    let workspace = workspace
        .canonicalize()
        .map_err(|source| ToolError::Workspace { source })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let resolved_parent = match parent {
        Some(parent) => workspace
            .join(parent)
            .canonicalize()
            .map_err(|_| ToolError::MissingParentDirectory(display.to_owned()))?,
        None => workspace.clone(),
    };
    if !resolved_parent.starts_with(&workspace) {
        return Err(ToolError::OutsideWorkspace(display.to_owned()));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| ToolError::UnsafePath(display.to_owned()))?;
    let target = resolved_parent.join(file_name);

    // If something is already there (possibly through a symlink), make sure
    // it still resolves inside the workspace and isn't a directory before
    // writing through it.
    if let Ok(resolved_target) = target.canonicalize() {
        if !resolved_target.starts_with(&workspace) {
            return Err(ToolError::OutsideWorkspace(display.to_owned()));
        }
        if resolved_target.is_dir() {
            return Err(ToolError::NotAFile(display.to_owned()));
        }
    }
    Ok(target)
}

fn read_existing_utf8(target: &Path, display: &str) -> Result<Option<String>, ToolError> {
    match fs::read(target) {
        Ok(bytes) => {
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(ToolError::TooLarge {
                    path: display.to_owned(),
                    limit: MAX_FILE_BYTES,
                });
            }
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| ToolError::NotUtf8(display.to_owned()))
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ToolError::Open {
            path: display.to_owned(),
            source,
        }),
    }
}

fn compute_diff(old: &str, new: &str) -> Vec<DiffLine> {
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|change| {
            let kind = match change.tag() {
                ChangeTag::Equal => DiffLineKind::Context,
                ChangeTag::Delete => DiffLineKind::Removed,
                ChangeTag::Insert => DiffLineKind::Added,
            };
            DiffLine {
                kind,
                content: change.value().trim_end_matches('\n').to_owned(),
            }
        })
        .collect()
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
    #[error("the parent directory of `{0}` does not exist")]
    MissingParentDirectory(String),
    #[error("`{path}` is larger than the {limit}-byte limit")]
    TooLarge { path: String, limit: u64 },
    #[error("`{path}` has more than {limit} entries")]
    TooManyEntries { path: String, limit: usize },
    #[error("search query must not be empty")]
    EmptyQuery,
    #[error("more than {limit} matches for `{query}`; narrow the query or path")]
    TooManyMatches { query: String, limit: usize },
    #[error("could not read `{path}`: {source}")]
    Read { path: String, source: io::Error },
    #[error("could not write `{path}`: {source}")]
    Write { path: String, source: io::Error },
    #[error("`{0}` is not UTF-8 text")]
    NotUtf8(String),
}

#[cfg(test)]
mod tests {
    use super::{
        DiffLineKind, ListFilesRequest, MAX_SEARCH_MATCHES, ReadFileRequest, SearchFilesRequest,
        ToolApprovalPreview, ToolError, ToolRequest, WriteFileRequest,
    };
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

    #[test]
    fn creates_a_new_file_and_diffs_it_as_all_additions() {
        let workspace = TempDir::new().unwrap();

        let request =
            WriteFileRequest::parse(r#"{"path":"notes.txt","content":"one\ntwo\n"}"#).unwrap();
        let preview = request.approval_preview(workspace.path()).unwrap();
        let ToolApprovalPreview::Diff {
            is_new_file, diff, ..
        } = preview
        else {
            panic!("expected a diff preview");
        };
        assert!(is_new_file);
        assert!(diff.iter().all(|line| line.kind == DiffLineKind::Added));
        assert_eq!(
            diff.iter()
                .map(|line| line.content.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );

        let output = request.execute(workspace.path()).unwrap();
        assert_eq!(output.path, "notes.txt");
        assert_eq!(output.bytes, 8);
        assert_eq!(output.lines, 2);
        assert_eq!(
            fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn diffs_an_overwrite_against_existing_content() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("notes.txt"), "one\ntwo\n").unwrap();

        let request =
            WriteFileRequest::parse(r#"{"path":"notes.txt","content":"one\nthree\n"}"#).unwrap();
        let preview = request.approval_preview(workspace.path()).unwrap();
        let ToolApprovalPreview::Diff {
            is_new_file, diff, ..
        } = preview
        else {
            panic!("expected a diff preview");
        };
        assert!(!is_new_file);
        assert_eq!(
            diff.iter()
                .map(|line| (line.kind.clone(), line.content.as_str()))
                .collect::<Vec<_>>(),
            [
                (DiffLineKind::Context, "one"),
                (DiffLineKind::Removed, "two"),
                (DiffLineKind::Added, "three"),
            ]
        );

        request.execute(workspace.path()).unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
            "one\nthree\n"
        );
    }

    #[test]
    fn rejects_writes_outside_the_workspace_and_to_missing_parents() {
        assert!(matches!(
            WriteFileRequest::parse(r#"{"path":"../escape.txt","content":""}"#),
            Err(ToolError::UnsafePath(_))
        ));

        let workspace = TempDir::new().unwrap();
        let request =
            WriteFileRequest::parse(r#"{"path":"missing/notes.txt","content":"hi"}"#).unwrap();
        assert!(matches!(
            request.execute(workspace.path()),
            Err(ToolError::MissingParentDirectory(_))
        ));
    }

    #[test]
    fn rejects_overwriting_a_directory_or_non_utf8_file() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        let as_directory = WriteFileRequest::parse(r#"{"path":"src","content":"hi"}"#).unwrap();
        assert!(matches!(
            as_directory.execute(workspace.path()),
            Err(ToolError::NotAFile(_))
        ));

        fs::write(workspace.path().join("binary"), [0xff, 0xfe]).unwrap();
        let over_binary = WriteFileRequest::parse(r#"{"path":"binary","content":"hi"}"#).unwrap();
        assert!(matches!(
            over_binary.approval_preview(workspace.path()),
            Err(ToolError::NotUtf8(_))
        ));
    }

    #[test]
    fn rejects_content_over_the_size_limit() {
        let workspace = TempDir::new().unwrap();
        let content = "x".repeat(128 * 1024 + 1);
        let arguments = serde_json::json!({"path": "big.txt", "content": content}).to_string();
        let request = WriteFileRequest::parse(&arguments).unwrap();
        assert!(matches!(
            request.execute(workspace.path()),
            Err(ToolError::TooLarge { .. })
        ));
    }

    #[test]
    fn finds_matches_across_nested_files_with_line_numbers() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(
            workspace.path().join("src/lib.rs"),
            "fn one() {}\nfn two() { todo!() }\n",
        )
        .unwrap();
        fs::write(workspace.path().join("README.md"), "no matches here\n").unwrap();

        let request = SearchFilesRequest::parse(r#"{"query":"todo!","path":null}"#).unwrap();
        let output = request.execute(workspace.path()).unwrap();

        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "src/lib.rs");
        assert_eq!(output.matches[0].line, 2);
        assert_eq!(output.matches[0].content, "fn two() { todo!() }");
    }

    #[test]
    fn search_skips_sensitive_and_noisy_directories_and_binary_files() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join(".git")).unwrap();
        fs::write(workspace.path().join(".git/config"), "secret todo").unwrap();
        fs::create_dir(workspace.path().join("target")).unwrap();
        fs::write(workspace.path().join("target/output"), "built todo").unwrap();
        fs::write(
            workspace.path().join("binary"),
            [0xff, 0xfe, b't', b'o', b'd', b'o'],
        )
        .unwrap();
        fs::write(workspace.path().join("visible.txt"), "a todo here\n").unwrap();

        let request = SearchFilesRequest::parse(r#"{"query":"todo","path":"."}"#).unwrap();
        let output = request.execute(workspace.path()).unwrap();

        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "visible.txt");
    }

    #[test]
    fn rejects_empty_queries_and_too_many_matches() {
        assert!(matches!(
            SearchFilesRequest::parse(r#"{"query":"","path":null}"#),
            Err(ToolError::EmptyQuery)
        ));

        let workspace = TempDir::new().unwrap();
        let content = "todo\n".repeat(MAX_SEARCH_MATCHES + 1);
        fs::write(workspace.path().join("many.txt"), content).unwrap();
        let request = SearchFilesRequest::parse(r#"{"query":"todo","path":null}"#).unwrap();
        assert!(matches!(
            request.execute(workspace.path()),
            Err(ToolError::TooManyMatches { .. })
        ));
    }
}
