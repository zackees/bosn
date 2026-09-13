//! Versioned, inert setup-document parsing.
//!
//! A setup document describes one Docker Linux application without requiring a
//! separately-authored Dockerfile or build-context files.  This module only
//! parses and validates the document: fetching it, resolving a chosen local
//! workspace, writing companion files, and applying it are effectful work for
//! higher layers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;

/// The only setup-document schema accepted by this release.
pub const SETUP_DOCUMENT_VERSION: u64 = 1;
/// A deliberately bounded, inert document before it reaches a cache or parser.
pub const MAX_SETUP_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_INLINE_DOCKERFILE_BYTES: usize = 512 * 1024;
pub const MAX_COMPANION_FILE_BYTES: usize = 256 * 1024;
pub const MAX_COMPANION_FILES: usize = 128;
pub const MAX_MOUNTS: usize = 128;
pub const MAX_TASKS: usize = 128;
pub const MAX_ENVIRONMENT_ENTRIES: usize = 128;

/// A validated setup document.  All paths that name workspace or generated
/// assets use slash-separated relative spelling and have no escaping component.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SetupDocument {
    pub version: u64,
    pub app: SetupApp,
    pub tasks: BTreeMap<String, SetupTask>,
    pub files: Vec<CompanionFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SetupApp {
    /// Exactly one immutable image or an inline Dockerfile.
    pub source: SetupSource,
    /// Values are application-scoped; machine policy is intentionally absent.
    pub environment: BTreeMap<String, String>,
    /// A path below the caller-selected workspace.  It is not a host absolute
    /// path and is materialized by the apply layer beneath the workspace mount.
    pub workdir: Option<String>,
    /// Optional declared long-running application command.  When present, the
    /// setup ensure primitive runs it as `sh -lc` inside the managed app
    /// container.  This remains document data, never a caller-supplied argv.
    pub command: Option<String>,
    pub mounts: Vec<WorkspaceMount>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum SetupSource {
    /// A Docker image name pinned by a canonical sha256 manifest digest.
    PinnedImage(String),
    /// Dockerfile text carried by this document and later materialized safely.
    InlineDockerfile(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceMount {
    /// Relative to the caller-selected workspace, never a host absolute path.
    pub source: String,
    /// An absolute, normalized path inside the container.
    pub target: String,
    pub readonly: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SetupTask {
    pub command: String,
    /// An optional path below the caller-selected workspace.
    pub workdir: Option<String>,
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompanionFile {
    /// Relative to the generated build-asset root, never a host absolute path.
    pub path: String,
    pub content: String,
}

/// A local config path is opaque to this pure layer; an HTTPS URL is syntactically
/// checked here and fetched only by the resolver layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupConfigLocator {
    LocalPath(String),
    HttpsUrl(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupDocumentError(pub String);
impl fmt::Display for SetupDocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SetupDocumentError {}

fn err<T>(message: impl Into<String>) -> Result<T, SetupDocumentError> {
    Err(SetupDocumentError(message.into()))
}

/// Parse a complete, versioned setup TOML document without accessing the host.
///
/// The supported wire shape is:
///
/// ```toml
/// version = 1
/// [app]
/// image = "registry.example/app@sha256:<64 lowercase hex digits>"
/// workdir = "app" # workspace-relative
/// [app.environment]
/// RUST_LOG = "info"
/// [[app.mount]]
/// source = "src" # workspace-relative
/// target = "/workspace/src" # container path
/// readonly = true
/// [task.test]
/// command = "cargo test"
/// workdir = "."
/// [[file]]
/// path = "scripts/check.sh"
/// content = "#!/bin/sh\necho ok\n"
/// ```
///
/// Replace `image` with `dockerfile = """..."""` for an inline build.
/// They are deliberately mutually exclusive: no implicit precedence can make a
/// remote document silently pull a different application than it describes.
pub fn parse_setup_document_toml(source: &str) -> Result<SetupDocument, SetupDocumentError> {
    if source.len() > MAX_SETUP_DOCUMENT_BYTES {
        return err(format!(
            "setup document exceeds {MAX_SETUP_DOCUMENT_BYTES} byte limit"
        ));
    }
    let raw: toml::Value = source.parse().map_err(|e: toml::de::Error| {
        SetupDocumentError(format!("setup document is not valid TOML: {e}"))
    })?;
    let table = table(&raw, "setup document")?;
    reject_unknown(table, &["version", "app", "task", "file"], "setup document")?;
    let version = required_positive_integer(table, "version", "setup document")?;
    if version != SETUP_DOCUMENT_VERSION {
        return err(format!(
            "unsupported setup document version {version}; expected {SETUP_DOCUMENT_VERSION}"
        ));
    }
    let app = parse_app(required_table(table, "app", "setup document")?)?;
    let tasks = parse_tasks(optional_table(table, "task", "setup document")?)?;
    let files = parse_files(optional_array(table, "file", "setup document")?)?;
    Ok(SetupDocument {
        version,
        app,
        tasks,
        files,
    })
}

/// Classify a setup locator without fetching it.  Remote setup documents are
/// HTTPS-only.  This check rejects credentials and fragments because neither is
/// appropriate for a durable configuration provenance/cache key.
pub fn parse_setup_config_locator(locator: &str) -> Result<SetupConfigLocator, SetupDocumentError> {
    if locator.is_empty()
        || locator.len() > 8 * 1024
        || locator.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return err(
            "setup config locator is empty, oversized, or contains whitespace/control characters",
        );
    }
    if let Some(scheme_end) = locator.find("://") {
        let scheme = &locator[..scheme_end];
        if scheme != "https" {
            return err(format!(
                "unsupported remote setup config scheme {scheme:?}; only https is allowed"
            ));
        }
        let authority_and_rest = &locator[scheme_end + 3..];
        let authority = authority_and_rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.is_empty() || authority.contains('@') || locator.contains('#') {
            return err(
                "HTTPS setup config URL needs a host and must not contain credentials or a fragment",
            );
        }
        return Ok(SetupConfigLocator::HttpsUrl(locator.into()));
    }
    // A selected local config may be an explicit Windows path. It is not a
    // workspace-relative field and stays opaque until the filesystem layer.
    if is_windows_absolute(locator) {
        return Ok(SetupConfigLocator::LocalPath(locator.into()));
    }
    if looks_like_scheme(locator) {
        return err("unsupported remote setup config scheme; only https is allowed");
    }
    Ok(SetupConfigLocator::LocalPath(locator.into()))
}

fn parse_app(raw: &toml::map::Map<String, toml::Value>) -> Result<SetupApp, SetupDocumentError> {
    reject_unknown(
        raw,
        &[
            "image",
            "dockerfile",
            "environment",
            "workdir",
            "command",
            "mount",
        ],
        "app",
    )?;
    let image = optional_string(raw, "image", "app")?;
    let dockerfile = optional_string(raw, "dockerfile", "app")?;
    let source = match (image, dockerfile) {
        (Some(image), None) => SetupSource::PinnedImage(parse_pinned_image(&image)?),
        (None, Some(dockerfile)) => {
            if dockerfile.is_empty() {
                return err("app.dockerfile must not be empty");
            }
            if dockerfile.len() > MAX_INLINE_DOCKERFILE_BYTES {
                return err(format!(
                    "app.dockerfile exceeds {MAX_INLINE_DOCKERFILE_BYTES} byte limit"
                ));
            }
            SetupSource::InlineDockerfile(dockerfile)
        }
        (None, None) => return err("app must set exactly one of `image` or `dockerfile`"),
        (Some(_), Some(_)) => return err("app.image and app.dockerfile are mutually exclusive"),
    };
    let environment = parse_environment(
        optional_table(raw, "environment", "app")?,
        "app.environment",
    )?;
    let workdir = optional_string(raw, "workdir", "app")?
        .map(|path| workspace_relative_path(&path, "app.workdir"))
        .transpose()?;
    let command = optional_string(raw, "command", "app")?
        .map(|command| {
            bounded_string(&command, 16 * 1024, "app.command")?;
            if command.is_empty() || command.contains('\0') {
                return err("app.command must be a nonempty bounded string without NUL");
            }
            Ok(command)
        })
        .transpose()?;
    let mounts = parse_mounts(optional_array(raw, "mount", "app")?)?;
    Ok(SetupApp {
        source,
        environment,
        workdir,
        command,
        mounts,
    })
}

fn parse_tasks(
    raw: &toml::map::Map<String, toml::Value>,
) -> Result<BTreeMap<String, SetupTask>, SetupDocumentError> {
    if raw.len() > MAX_TASKS {
        return err(format!(
            "setup document declares more than {MAX_TASKS} tasks"
        ));
    }
    let mut tasks = BTreeMap::new();
    for (name, value) in raw {
        task_name(name)?;
        let body = table(value, &format!("task.{name}"))?;
        reject_unknown(
            body,
            &["command", "workdir", "environment"],
            &format!("task.{name}"),
        )?;
        let command = required_string(body, "command", &format!("task.{name}"))?;
        bounded_string(&command, 16 * 1024, &format!("task.{name}.command"))?;
        if command.is_empty() {
            return err(format!("task.{name}.command must not be empty"));
        }
        let workdir = optional_string(body, "workdir", &format!("task.{name}"))?
            .map(|path| workspace_relative_path(&path, &format!("task.{name}.workdir")))
            .transpose()?;
        let environment = parse_environment(
            optional_table(body, "environment", &format!("task.{name}"))?,
            &format!("task.{name}.environment"),
        )?;
        tasks.insert(
            name.clone(),
            SetupTask {
                command,
                workdir,
                environment,
            },
        );
    }
    Ok(tasks)
}

fn parse_files(raw: &[toml::Value]) -> Result<Vec<CompanionFile>, SetupDocumentError> {
    if raw.len() > MAX_COMPANION_FILES {
        return err(format!(
            "setup document declares more than {MAX_COMPANION_FILES} companion files"
        ));
    }
    let mut paths = BTreeSet::new();
    let mut files = Vec::with_capacity(raw.len());
    for (index, value) in raw.iter().enumerate() {
        let where_ = format!("file[{index}]");
        let body = table(value, &where_)?;
        reject_unknown(body, &["path", "content"], &where_)?;
        let path = generated_relative_path(
            &required_string(body, "path", &where_)?,
            &format!("{where_}.path"),
        )?;
        if !paths.insert(path.clone()) {
            return err(format!("duplicate companion file path {path:?}"));
        }
        let content = required_string(body, "content", &where_)?;
        bounded_string(
            &content,
            MAX_COMPANION_FILE_BYTES,
            &format!("{where_}.content"),
        )?;
        files.push(CompanionFile { path, content });
    }
    Ok(files)
}

fn parse_mounts(raw: &[toml::Value]) -> Result<Vec<WorkspaceMount>, SetupDocumentError> {
    if raw.len() > MAX_MOUNTS {
        return err(format!("app declares more than {MAX_MOUNTS} mounts"));
    }
    let mut mounts = Vec::with_capacity(raw.len());
    for (index, value) in raw.iter().enumerate() {
        let where_ = format!("app.mount[{index}]");
        let body = table(value, &where_)?;
        reject_unknown(body, &["source", "target", "readonly"], &where_)?;
        let source = workspace_relative_path(
            &required_string(body, "source", &where_)?,
            &format!("{where_}.source"),
        )?;
        let target = container_absolute_path(
            &required_string(body, "target", &where_)?,
            &format!("{where_}.target"),
        )?;
        let readonly = optional_bool(body, "readonly", &where_)?.unwrap_or(false);
        mounts.push(WorkspaceMount {
            source,
            target,
            readonly,
        });
    }
    Ok(mounts)
}

fn parse_environment(
    raw: &toml::map::Map<String, toml::Value>,
    where_: &str,
) -> Result<BTreeMap<String, String>, SetupDocumentError> {
    if raw.len() > MAX_ENVIRONMENT_ENTRIES {
        return err(format!(
            "{where_} declares more than {MAX_ENVIRONMENT_ENTRIES} entries"
        ));
    }
    let mut environment = BTreeMap::new();
    for (key, value) in raw {
        if !valid_environment_name(key) {
            return err(format!(
                "{where_} has invalid environment variable name {key:?}"
            ));
        }
        let value = value
            .as_str()
            .ok_or_else(|| SetupDocumentError(format!("{where_}.{key} must be a string")))?
            .to_owned();
        bounded_string(&value, 16 * 1024, &format!("{where_}.{key}"))?;
        if value.bytes().any(|b| b == 0) {
            return err(format!("{where_}.{key} must not contain NUL"));
        }
        environment.insert(key.clone(), value);
    }
    Ok(environment)
}

fn parse_pinned_image(image: &str) -> Result<String, SetupDocumentError> {
    bounded_string(image, 512, "app.image")?;
    if image
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return err("app.image must not contain whitespace or control characters");
    }
    let Some((name, digest)) = image.rsplit_once("@sha256:") else {
        return err("app.image must be pinned as name@sha256:<64 lowercase hex digits>");
    };
    if name.is_empty()
        || name.contains('@')
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return err("app.image must be pinned as name@sha256:<64 lowercase hex digits>");
    }
    Ok(image.into())
}

fn workspace_relative_path(path: &str, where_: &str) -> Result<String, SetupDocumentError> {
    checked_relative_path(path, where_, "workspace")
}

fn generated_relative_path(path: &str, where_: &str) -> Result<String, SetupDocumentError> {
    let path = checked_relative_path(path, where_, "generated asset")?;
    if path == "." {
        return err(format!("{where_} must name a generated file, not its root"));
    }
    Ok(path)
}

fn checked_relative_path(
    path: &str,
    where_: &str,
    kind: &str,
) -> Result<String, SetupDocumentError> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\0')
        || path.contains('\\')
        || path.starts_with('/')
        || is_windows_absolute(path)
    {
        return err(format!("{where_} must be a safe relative {kind} path"));
    }
    for component in path.split('/') {
        if component.is_empty() || component == ".." {
            return err(format!("{where_} must be a safe relative {kind} path"));
        }
    }
    Ok(path.into())
}

fn container_absolute_path(path: &str, where_: &str) -> Result<String, SetupDocumentError> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\0')
        || path.contains('\\')
        || !path.starts_with('/')
    {
        return err(format!(
            "{where_} must be a normalized absolute container path"
        ));
    }
    let suffix = &path[1..];
    if !suffix.is_empty()
        && suffix
            .split('/')
            .any(|component| component == ".." || component.is_empty())
    {
        return err(format!(
            "{where_} must be a normalized absolute container path"
        ));
    }
    Ok(path.into())
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn task_name(name: &str) -> Result<(), SetupDocumentError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .enumerate()
            .all(|(index, b)| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' && index > 0)
        || !name.as_bytes()[0].is_ascii_alphanumeric()
    {
        return err(format!("invalid task name {name:?}"));
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'_' | b'a'..=b'z' | b'A'..=b'Z'))
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

fn bounded_string(value: &str, max: usize, where_: &str) -> Result<(), SetupDocumentError> {
    if value.len() > max {
        return err(format!("{where_} exceeds {max} byte limit"));
    }
    Ok(())
}

fn looks_like_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, b)| {
            b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.' && index > 0
        })
}

fn reject_unknown(
    table: &toml::map::Map<String, toml::Value>,
    allowed: &[&str],
    where_: &str,
) -> Result<(), SetupDocumentError> {
    if let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) {
        return err(format!("unknown key {key:?} in {where_}"));
    }
    Ok(())
}

fn table<'a>(
    value: &'a toml::Value,
    where_: &str,
) -> Result<&'a toml::map::Map<String, toml::Value>, SetupDocumentError> {
    value
        .as_table()
        .ok_or_else(|| SetupDocumentError(format!("{where_} must be a table")))
}

fn required_table<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<&'a toml::map::Map<String, toml::Value>, SetupDocumentError> {
    table
        .get(key)
        .ok_or_else(|| SetupDocumentError(format!("{where_} must set `{key}`")))
        .and_then(|value| self::table(value, &format!("{where_}.{key}")))
}

fn optional_table<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<&'a toml::map::Map<String, toml::Value>, SetupDocumentError> {
    match table.get(key) {
        Some(value) => self::table(value, &format!("{where_}.{key}")),
        None => Ok(empty_table()),
    }
}

fn optional_array<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<&'a [toml::Value], SetupDocumentError> {
    match table.get(key) {
        Some(value) => value
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| SetupDocumentError(format!("{where_}.{key} must be an array"))),
        None => Ok(&[]),
    }
}

fn required_positive_integer(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<u64, SetupDocumentError> {
    let value = table
        .get(key)
        .ok_or_else(|| SetupDocumentError(format!("{where_} must set `{key}`")))?
        .as_integer()
        .ok_or_else(|| SetupDocumentError(format!("{where_}.{key} must be a positive integer")))?;
    u64::try_from(value)
        .map_err(|_| SetupDocumentError(format!("{where_}.{key} must be a positive integer")))
}

fn required_string(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<String, SetupDocumentError> {
    optional_string(table, key, where_)?
        .ok_or_else(|| SetupDocumentError(format!("{where_} must set `{key}`")))
}

fn optional_string(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<Option<String>, SetupDocumentError> {
    match table.get(key) {
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| SetupDocumentError(format!("{where_}.{key} must be a string"))),
        None => Ok(None),
    }
}

fn optional_bool(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<Option<bool>, SetupDocumentError> {
    match table.get(key) {
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| SetupDocumentError(format!("{where_}.{key} must be a boolean"))),
        None => Ok(None),
    }
}

fn empty_table() -> &'static toml::map::Map<String, toml::Value> {
    static EMPTY: std::sync::OnceLock<toml::map::Map<String, toml::Value>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(toml::map::Map::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: &str = "registry.example/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn app_command_is_optional_declared_document_data() {
        let document = parse_setup_document_toml(&format!(
            "version = 1\n[app]\nimage = \"{IMAGE}\"\ncommand = \"./serve --port 8080\"\n"
        ))
        .unwrap();
        assert_eq!(document.app.command.as_deref(), Some("./serve --port 8080"));

        let omitted =
            parse_setup_document_toml(&format!("version = 1\n[app]\nimage = \"{IMAGE}\"\n"))
                .unwrap();
        assert_eq!(omitted.app.command, None);
    }

    #[test]
    fn app_command_rejects_empty_nul_and_oversized_values() {
        for command in [
            "\"\"".to_owned(),
            "\"bad\\u0000value\"".to_owned(),
            format!("\"{}\"", "x".repeat(16 * 1024 + 1)),
        ] {
            let source = format!("version = 1\n[app]\nimage = \"{IMAGE}\"\ncommand = {command}\n");
            assert!(parse_setup_document_toml(&source).is_err(), "{command:?}");
        }
    }
}
