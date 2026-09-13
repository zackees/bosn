//! Inert manifest declarations.  Path spelling is preserved; resolving paths is kernel work.

use std::collections::BTreeMap;
use std::fmt;

use crate::{Retention, Scope};

const RESERVED_PREFIX: &str = "/bosn/";
const RESERVED_HEARTBEAT: &str = "/bosn-daemon/heartbeat";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestRoots {
    /// Source/provenance name for diagnostics; never opened by this module.
    pub source_provenance: String,
    /// Build-asset/materialization root spelling; never canonicalized by this module.
    pub materialization_root: String,
    /// Workspace bind root spelling; never canonicalized by this module.
    pub workspace_root: String,
}
impl ManifestRoots {
    pub fn new(
        source_provenance: impl Into<String>,
        materialization_root: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Self {
        Self {
            source_provenance: source_provenance.into(),
            materialization_root: materialization_root.into(),
            workspace_root: workspace_root.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Manifest {
    pub roots: ManifestRoots,
    pub stacks: BTreeMap<String, Stack>,
    pub tasks: BTreeMap<String, Task>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stack {
    pub name: String,
    pub dockerfile: Option<String>,
    pub image: Option<String>,
    pub family: Option<String>,
    pub default: bool,
    pub volumes: Vec<Volume>,
    pub mounts: Vec<Mount>,
    pub tmpfs: Vec<Tmpfs>,
    pub env: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub kind: Option<String>,
    pub guest: Option<Guest>,
    pub acknowledge_macos_license: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Volume {
    pub name: String,
    pub scope: Scope,
    pub destination: Option<String>,
    pub retention: Retention,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mount {
    pub name: String,
    pub source: String,
    pub destination: String,
    pub readonly: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tmpfs {
    pub value: String,
    pub destination: String,
    pub readwrite: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task {
    pub name: String,
    pub stack: String,
    pub cmd: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Guest {
    pub ssh_port: u64,
    pub ssh_user: String,
    pub ssh_host: String,
    pub web_port: u64,
    pub ready_timeout: u64,
    pub ready_poll_interval: u64,
    pub version: String,
    pub ram_size: String,
    pub disk_size: String,
    pub cpu_cores: Option<u64>,
    pub payload: Option<String>,
    pub payload_destination: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestError(pub String);
impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ManifestError {}
fn err<T>(message: impl Into<String>) -> Result<T, ManifestError> {
    Err(ManifestError(message.into()))
}

impl Manifest {
    pub fn stack(&self, name: &str) -> Result<&Stack, ManifestError> {
        self.stacks.get(name).ok_or_else(|| {
            ManifestError(format!(
                "no stack named {name:?}; known stacks: {:?}",
                self.stacks.keys().collect::<Vec<_>>()
            ))
        })
    }
    pub fn task(&self, name: &str) -> Result<&Task, ManifestError> {
        self.tasks.get(name).ok_or_else(|| {
            ManifestError(format!(
                "no task named {name:?}; known tasks: {:?}",
                self.tasks.keys().collect::<Vec<_>>()
            ))
        })
    }
    pub fn default_stack(&self) -> Result<&Stack, ManifestError> {
        let marked: Vec<_> = self.stacks.values().filter(|stack| stack.default).collect();
        match marked.as_slice() {
            [one] => Ok(one),
            [] if self.stacks.len() == 1 => self
                .stacks
                .values()
                .next()
                .ok_or_else(|| ManifestError("no default stack available".into())),
            [] => {
                err("no default stack; mark one with `default = true` or name a stack explicitly")
            }
            _ => err(format!(
                "more than one stack is marked default: {:?}",
                marked.iter().map(|s| &s.name).collect::<Vec<_>>()
            )),
        }
    }
}
impl Volume {
    pub fn mount_at(&self) -> String {
        self.destination
            .clone()
            .unwrap_or_else(|| format!("/bosn/{}", self.name))
    }
}

pub fn parse_manifest_toml(source: &str, roots: ManifestRoots) -> Result<Manifest, ManifestError> {
    let raw: toml::Value = source.parse().map_err(|e: toml::de::Error| {
        ManifestError(format!(
            "{} is not valid TOML: {e}",
            roots.source_provenance
        ))
    })?;
    let table = raw
        .as_table()
        .ok_or_else(|| ManifestError("manifest must be a table".into()))?;
    reject_unknown(table, &["stack", "task"], "manifest")?;
    let stacks_raw = optional_table(table, "stack", "manifest")?;
    let mut stacks = BTreeMap::new();
    for (name, body) in stacks_raw {
        stacks.insert(
            name.clone(),
            parse_stack(name, table_value(body, "stack")?)?,
        );
    }
    if stacks.is_empty() {
        return err("manifest declares no stacks");
    }
    let tasks_raw = optional_table(table, "task", "manifest")?;
    let mut tasks = BTreeMap::new();
    for (name, body) in tasks_raw {
        let body = table_value(body, "task")?;
        reject_unknown(body, &["stack", "cmd"], &format!("task.{name}"))?;
        let cmd = required_string(body, "cmd", &format!("task.{name}"))?;
        if cmd.is_empty() {
            return err(format!("[task.{name}] must set `cmd`"));
        }
        let stack = match optional_string(body, "stack", &format!("task.{name}"))? {
            Some(s) => s,
            None => Manifest {
                roots: roots.clone(),
                stacks: stacks.clone(),
                tasks: BTreeMap::new(),
            }
            .default_stack()?
            .name
            .clone(),
        };
        if !stacks.contains_key(&stack) {
            return err(format!(
                "[task.{name}] references unknown stack {stack:?}; known stacks: {:?}",
                stacks.keys().collect::<Vec<_>>()
            ));
        }
        tasks.insert(
            name.clone(),
            Task {
                name: name.clone(),
                stack,
                cmd,
            },
        );
    }
    Ok(Manifest {
        roots,
        stacks,
        tasks,
    })
}

fn parse_stack(
    name: &str,
    body: &toml::map::Map<String, toml::Value>,
) -> Result<Stack, ManifestError> {
    let where_ = format!("stack.{name}");
    reject_unknown(
        body,
        &[
            "dockerfile",
            "image",
            "family",
            "default",
            "volumes",
            "mounts",
            "tmpfs",
            "env",
            "workdir",
            "kind",
            "guest",
            "acknowledge_macos_license",
        ],
        &where_,
    )?;
    let dockerfile = optional_string(body, "dockerfile", &where_)?;
    let image = optional_string(body, "image", &where_)?;
    if dockerfile.is_none() && image.is_none() {
        return err(format!(
            "[stack.{name}] must set either `dockerfile` or `image`"
        ));
    }
    if dockerfile.as_deref().is_some_and(str::is_empty)
        || image.as_deref().is_some_and(str::is_empty)
    {
        return err(format!(
            "[stack.{name}] dockerfile and image must not be empty"
        ));
    }
    let volumes = parse_volumes(name, optional_table(body, "volumes", &where_)?)?;
    let mounts = parse_mounts(name, optional_table(body, "mounts", &where_)?)?;
    let tmpfs = parse_tmpfs(name, body.get("tmpfs"))?;
    duplicates(name, &volumes, &mounts, &tmpfs)?;
    let env = parse_env(name, optional_table(body, "env", &where_)?)?;
    let workdir = optional_string(body, "workdir", &where_)?
        .map(|v| workdir(&v, name))
        .transpose()?;
    let kind = optional_string(body, "kind", &where_)?;
    if kind.as_deref().is_some_and(|v| v != "macos-x64-guest") {
        return err(format!("[stack.{name}] has unknown kind {kind:?}"));
    }
    let acknowledge_macos_license =
        optional_bool(body, "acknowledge_macos_license", &where_)?.unwrap_or(false);
    let guest = parse_guest(name, body.get("guest"))?;
    if kind.is_none() && guest.is_some() {
        return err(format!(
            "[stack.{name}.guest] is only meaningful with kind = \"macos-x64-guest\""
        ));
    }
    if kind.is_none() && acknowledge_macos_license {
        return err(format!(
            "[stack.{name}] sets acknowledge_macos_license without guest kind"
        ));
    }
    if kind.is_some() && !acknowledge_macos_license {
        return err(format!(
            "[stack.{name}] guest requires acknowledge_macos_license = true"
        ));
    }
    if kind.is_some() && !mounts.is_empty() {
        return err(format!(
            "[stack.{name}.mounts] guest cannot see host bind mounts"
        ));
    }
    let is_guest = kind.is_some();
    Ok(Stack {
        name: name.into(),
        dockerfile,
        image,
        family: optional_string(body, "family", &where_)?,
        default: optional_bool(body, "default", &where_)?.unwrap_or(false),
        volumes,
        mounts,
        tmpfs,
        env,
        workdir,
        kind,
        guest: if is_guest {
            Some(guest.unwrap_or_default())
        } else {
            None
        },
        acknowledge_macos_license,
    })
}

fn parse_volumes(
    _stack: &str,
    raw: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<Volume>, ManifestError> {
    raw.iter()
        .map(|(name, v)| {
            let t = table_value(v, "volume")?;
            reject_unknown(t, &["scope", "destination", "retention"], "volume")?;
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
                return err(format!(
                    "volume {name:?} must be a single safe path component"
                ));
            }
            let scope = match optional_string(t, "scope", "volume")?
                .as_deref()
                .unwrap_or("spec")
            {
                "spec" => Scope::Spec,
                "stack" => Scope::Stack,
                "machine" => Scope::Machine,
                other => return err(format!("volume {name:?} has unknown scope {other:?}")),
            };
            let retention = match optional_string(t, "retention", "volume")?
                .as_deref()
                .unwrap_or("warm")
            {
                "warm" => Retention::Warm,
                "pinned" => Retention::Pinned,
                other => return err(format!("volume {name:?} has unknown retention {other:?}")),
            };
            let destination = optional_string(t, "destination", "volume")?
                .map(|d| destination(&d, "volume", name))
                .transpose()?;
            Ok(Volume {
                name: name.clone(),
                scope,
                destination,
                retention,
            })
        })
        .collect()
}
fn parse_mounts(
    stack: &str,
    raw: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<Mount>, ManifestError> {
    raw.iter()
        .map(|(name, v)| {
            let t = table_value(v, "mount")?;
            reject_unknown(t, &["source", "destination", "readonly"], "mount")?;
            let source = required_string(t, "source", &format!("stack.{stack}.mounts.{name}"))?;
            if source.is_empty() {
                return err(format!("mount {name:?} must set `source`"));
            }
            let dest = required_string(t, "destination", "mount")?;
            Ok(Mount {
                name: name.clone(),
                source,
                destination: destination(&dest, "mount", name)?,
                readonly: optional_bool(t, "readonly", "mount")?.unwrap_or(false),
            })
        })
        .collect()
}
fn parse_tmpfs(stack: &str, raw: Option<&toml::Value>) -> Result<Vec<Tmpfs>, ManifestError> {
    let Some(raw) = raw else { return Ok(vec![]) };
    let values = raw.as_array().ok_or_else(|| {
        ManifestError(format!("[stack.{stack}].tmpfs must be an array of strings"))
    })?;
    values
        .iter()
        .map(|v| {
            let value = v.as_str().ok_or_else(|| {
                ManifestError(format!("[stack.{stack}].tmpfs must be an array of strings"))
            })?;
            let (dest, options) = value
                .split_once(':')
                .map_or((value, None), |(a, b)| (a, Some(b)));
            if options.is_some_and(|o| o.is_empty() || o.split(',').any(str::is_empty)) {
                return err(format!("tmpfs {value:?} has an empty mount option"));
            }
            let destination = destination(dest, "tmpfs", dest)?;
            let readwrite = !options.is_some_and(|o| {
                o.split(',').rev().find(|x| *x == "ro" || *x == "rw") == Some("ro")
            });
            Ok(Tmpfs {
                value: options.map_or(destination.clone(), |o| format!("{destination}:{o}")),
                destination,
                readwrite,
            })
        })
        .collect()
}
fn parse_env(
    stack: &str,
    raw: &toml::map::Map<String, toml::Value>,
) -> Result<BTreeMap<String, String>, ManifestError> {
    raw.iter()
        .map(|(key, v)| {
            if key.is_empty() {
                return err(format!("[stack.{stack}.env] has an empty key"));
            }
            if key.contains('=') {
                return err(format!(
                    "[stack.{stack}.env] key {key:?} must not contain '='"
                ));
            }
            let value = match v {
                toml::Value::String(v) => v.clone(),
                toml::Value::Integer(v) => v.to_string(),
                toml::Value::Float(v) if v.is_finite() => v.to_string(),
                toml::Value::Boolean(v) => v.to_string(),
                _ => return err(format!("[stack.{stack}.env] key {key:?} must be a scalar")),
            };
            Ok((key.clone(), value))
        })
        .collect()
}
fn duplicates(
    stack: &str,
    volumes: &[Volume],
    mounts: &[Mount],
    tmpfs: &[Tmpfs],
) -> Result<(), ManifestError> {
    let mut seen = BTreeMap::new();
    for (destination, what) in volumes
        .iter()
        .map(|v| (v.mount_at(), format!("volume {:?}", v.name)))
        .chain(
            mounts
                .iter()
                .map(|m| (m.destination.clone(), format!("mount {:?}", m.name))),
        )
        .chain(
            tmpfs
                .iter()
                .map(|t| (t.destination.clone(), "tmpfs".into())),
        )
    {
        if let Some(previous) = seen.insert(destination.clone(), what.clone()) {
            return err(format!(
                "[stack.{stack}] mounts {destination:?} twice: {previous} and {what}"
            ));
        }
    }
    Ok(())
}
fn destination(value: &str, what: &str, name: &str) -> Result<String, ManifestError> {
    if !value.starts_with('/') {
        return err(format!(
            "{what} {name:?} has destination {value:?}; it must be an absolute path"
        ));
    }
    let mut components = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            part => components.push(part),
        }
    }
    let normalized = format!("/{}", components.join("/"));
    // `/` and any ancestor of the heartbeat could hide the daemon's liveness input.
    if normalized == "/"
        || normalized == "/bosn"
        || normalized.starts_with(RESERVED_PREFIX)
        || RESERVED_HEARTBEAT == normalized
        || RESERVED_HEARTBEAT.starts_with(&(normalized.clone() + "/"))
        || normalized.starts_with(&(RESERVED_HEARTBEAT.to_owned() + "/"))
    {
        return err(format!(
            "{what} {name:?} would mount inside bosn's reserved namespace"
        ));
    }
    Ok(normalized)
}
fn workdir(value: &str, name: &str) -> Result<String, ManifestError> {
    if !value.starts_with('/') {
        return err(format!(
            "workdir {name:?} has destination {value:?}; it must be an absolute path"
        ));
    }
    let normalized = lexical_absolute(value);
    if normalized == "/bosn"
        || normalized.starts_with(RESERVED_PREFIX)
        || normalized == RESERVED_HEARTBEAT
        || normalized.starts_with(&(RESERVED_HEARTBEAT.to_owned() + "/"))
    {
        return err(format!(
            "workdir {name:?} would use bosn's reserved namespace"
        ));
    }
    Ok(normalized)
}
fn lexical_absolute(value: &str) -> String {
    let mut components = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            part => components.push(part),
        }
    }
    format!("/{}", components.join("/"))
}

impl Default for Guest {
    fn default() -> Self {
        Self {
            ssh_port: 2222,
            ssh_user: "runner".into(),
            ssh_host: "127.0.0.1".into(),
            web_port: 8006,
            ready_timeout: 1800,
            ready_poll_interval: 10,
            version: "ventura".into(),
            ram_size: "8G".into(),
            disk_size: "128G".into(),
            cpu_cores: None,
            payload: None,
            payload_destination: "~/bosn-payload".into(),
        }
    }
}
fn parse_guest(stack: &str, raw: Option<&toml::Value>) -> Result<Option<Guest>, ManifestError> {
    let Some(raw) = raw else { return Ok(None) };
    let t = table_value(raw, "guest")?;
    let known = [
        "ssh_port",
        "ssh_user",
        "ssh_host",
        "web_port",
        "ready_timeout",
        "ready_poll_interval",
        "version",
        "ram_size",
        "disk_size",
        "cpu_cores",
        "payload",
        "payload_destination",
    ];
    if let Some(k) = t.keys().find(|k| !known.contains(&k.as_str())) {
        return err(format!("[stack.{stack}.guest] has unknown key {k:?}"));
    }
    let mut guest = Guest::default();
    for (key, target) in [
        ("ssh_port", &mut guest.ssh_port),
        ("web_port", &mut guest.web_port),
    ] {
        if let Some(v) = t.get(key) {
            *target = port(v, stack, key)?;
        }
    }
    for (key, target) in [
        ("ready_timeout", &mut guest.ready_timeout),
        ("ready_poll_interval", &mut guest.ready_poll_interval),
    ] {
        if let Some(v) = t.get(key) {
            *target = positive(v, stack, key)?;
        }
    }
    if let Some(v) = t.get("cpu_cores") {
        guest.cpu_cores = Some(positive(v, stack, "cpu_cores")?);
    }
    for (key, target) in [
        ("ssh_user", &mut guest.ssh_user),
        ("ssh_host", &mut guest.ssh_host),
        ("version", &mut guest.version),
        ("ram_size", &mut guest.ram_size),
        ("disk_size", &mut guest.disk_size),
        ("payload_destination", &mut guest.payload_destination),
    ] {
        if let Some(v) = t.get(key) {
            *target = string_value(v, &format!("stack.{stack}.guest.{key}"))?;
        }
    }
    if let Some(v) = t.get("payload") {
        guest.payload = Some(string_value(v, "guest.payload")?);
    }
    if guest.ssh_user.is_empty() || guest.ssh_host.is_empty() {
        return err(format!(
            "[stack.{stack}.guest] ssh_user and ssh_host must not be empty"
        ));
    }
    if guest.ssh_port == guest.web_port {
        return err(format!(
            "[stack.{stack}.guest] ssh_port and web_port must differ"
        ));
    }
    Ok(Some(guest))
}
fn positive(v: &toml::Value, stack: &str, key: &str) -> Result<u64, ManifestError> {
    v.as_integer()
        .and_then(|n| u64::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            ManifestError(format!(
                "[stack.{stack}.guest] {key} must be a positive integer"
            ))
        })
}
fn port(v: &toml::Value, stack: &str, key: &str) -> Result<u64, ManifestError> {
    let value = positive(v, stack, key)?;
    if value > 65535 {
        return err(format!(
            "[stack.{stack}.guest] {key} must be between 1 and 65535"
        ));
    }
    Ok(value)
}
fn optional_table<'a>(
    t: &'a toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<&'a toml::map::Map<String, toml::Value>, ManifestError> {
    match t.get(key) {
        None => Ok(empty_table()),
        Some(v) => v
            .as_table()
            .ok_or_else(|| ManifestError(format!("[{where_}.{key}] must be a table"))),
    }
}
fn empty_table() -> &'static toml::map::Map<String, toml::Value> {
    static EMPTY: std::sync::OnceLock<toml::map::Map<String, toml::Value>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(toml::map::Map::new)
}
fn table_value<'a>(
    v: &'a toml::Value,
    kind: &str,
) -> Result<&'a toml::map::Map<String, toml::Value>, ManifestError> {
    v.as_table()
        .ok_or_else(|| ManifestError(format!("{kind} must be a table")))
}
fn required_string(
    t: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<String, ManifestError> {
    t.get(key)
        .ok_or_else(|| ManifestError(format!("[{where_}] must set `{key}`")))
        .and_then(|v| string_value(v, where_))
}
fn optional_string(
    t: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<Option<String>, ManifestError> {
    t.get(key).map(|v| string_value(v, where_)).transpose()
}
fn string_value(v: &toml::Value, where_: &str) -> Result<String, ManifestError> {
    v.as_str()
        .map(Into::into)
        .ok_or_else(|| ManifestError(format!("[{where_}] must be a string")))
}
fn optional_bool(
    t: &toml::map::Map<String, toml::Value>,
    key: &str,
    where_: &str,
) -> Result<Option<bool>, ManifestError> {
    t.get(key)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| ManifestError(format!("[{where_}] must be a boolean")))
        })
        .transpose()
}
fn reject_unknown(
    t: &toml::map::Map<String, toml::Value>,
    allowed: &[&str],
    where_: &str,
) -> Result<(), ManifestError> {
    if let Some(key) = t.keys().find(|key| !allowed.contains(&key.as_str())) {
        return err(format!("[{where_}] has unknown key {key:?}"));
    }
    Ok(())
}
