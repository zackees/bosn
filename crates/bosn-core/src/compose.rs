//! Typed, inert planning for Bosn's deliberately small Compose subset.
//!
//! This is not a Docker Compose runner.  It reads YAML into Bosn-owned types, resolves
//! YAML merge keys, rejects fields whose meaning is not represented below, checks the
//! cross-resource references that a future daemon will need, and produces a stable plan
//! digest.  It does not inspect paths, contact an engine, interpolate an environment, or
//! start a process.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

pub const COMPOSE_PLAN_VERSION: u32 = 1;

const TOP_LEVEL_KEYS: &[&str] = &["name", "version", "services", "volumes", "networks"];
const SERVICE_KEYS: &[&str] = &[
    "image",
    "build",
    "profiles",
    "volumes",
    "networks",
    "environment",
    "ports",
    "depends_on",
    "healthcheck",
    "labels",
    "command",
    "entrypoint",
    "restart",
    "container_name",
    "tmpfs",
];
const VOLUME_KEYS: &[&str] = &["driver", "driver_opts", "labels", "external", "name"];
const NETWORK_KEYS: &[&str] = &[
    "driver",
    "driver_opts",
    "labels",
    "external",
    "internal",
    "name",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeErrorCode {
    MalformedYaml,
    InvalidShape,
    Unsupported,
    InvalidValue,
    UnsafePath,
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeError {
    pub code: ComposeErrorCode,
    pub path: String,
    pub message: String,
    pub remedy: String,
}
impl ComposeError {
    fn new(
        code: ComposeErrorCode,
        path: impl Into<String>,
        message: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
            remedy: remedy.into(),
        }
    }
    fn unsupported(path: impl Into<String>, remedy: impl Into<String>) -> Self {
        let path = path.into();
        Self::new(
            ComposeErrorCode::Unsupported,
            path.clone(),
            format!("unsupported Compose field {path:?}"),
            remedy,
        )
    }
}
impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}: {}; remedy: {}",
            match self.code {
                ComposeErrorCode::MalformedYaml => "malformed_yaml",
                ComposeErrorCode::InvalidShape => "invalid_shape",
                ComposeErrorCode::Unsupported => "unsupported",
                ComposeErrorCode::InvalidValue => "invalid_value",
                ComposeErrorCode::UnsafePath => "unsafe_path",
                ComposeErrorCode::Ambiguous => "ambiguous",
            },
            self.path,
            self.message,
            self.remedy
        )
    }
}
impl std::error::Error for ComposeError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RelativePath(String);
impl RelativePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BuildSpec {
    pub context: RelativePath,
    pub dockerfile: RelativePath,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MountSpec {
    Bind {
        source: RelativePath,
        target: String,
        read_only: bool,
    },
    Volume {
        source: String,
        target: String,
        read_only: bool,
    },
    Tmpfs {
        target: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DependencySpec {
    pub condition: String,
    pub restart: bool,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HealthcheckSpec {
    pub test: Vec<String>,
    pub interval: Option<String>,
    pub timeout: Option<String>,
    pub retries: Option<u64>,
    pub start_period: Option<String>,
    pub start_interval: Option<String>,
    pub disable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ServiceSpec {
    pub image: Option<String>,
    pub build: Option<BuildSpec>,
    pub profiles: Vec<String>,
    pub mounts: Vec<MountSpec>,
    pub networks: BTreeSet<String>,
    pub environment: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub depends_on: BTreeMap<String, DependencySpec>,
    pub healthcheck: Option<HealthcheckSpec>,
    pub labels: BTreeMap<String, String>,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub restart: Option<String>,
    pub container_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResourceSpec {
    pub driver: Option<String>,
    pub driver_opts: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub external: bool,
    pub name: Option<String>,
    pub internal: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ComposeDocument {
    pub name: Option<String>,
    /// Retained as declared for plan identity; it has no execution meaning in this slice.
    pub version: Option<String>,
    pub services: BTreeMap<String, ServiceSpec>,
    pub volumes: BTreeMap<String, ResourceSpec>,
    pub networks: BTreeMap<String, ResourceSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ComposePlan {
    pub version: u32,
    pub document: ComposeDocument,
    /// Canonical JSON of `document`; useful for review and stable test fixtures only.
    pub normalized_json: String,
    /// `sha256:` digest over `normalized_json`, not a filesystem/build-context digest.
    pub digest: String,
}

/// Parse the represented Compose subset.  This function is pure: source is YAML text,
/// and all paths remain lexical strings rooted by a future caller rather than this module.
pub fn parse_compose_yaml(source: &str) -> Result<ComposeDocument, ComposeError> {
    let raw: Value = serde_yaml::from_str(source).map_err(|error| {
        ComposeError::new(
            ComposeErrorCode::MalformedYaml,
            "compose",
            error.to_string(),
            "supply one YAML mapping using Bosn's documented Compose subset",
        )
    })?;
    let raw = resolve_merges(raw, "compose")?;
    let top = mapping(&raw, "compose")?;
    check_keys(top, TOP_LEVEL_KEYS, "compose")?;
    let name = optional_string(top, "name", "name")?;
    if let Some(name) = &name {
        identifier(name, "name")?;
    }
    let version = get(top, "version")
        .map(|value| scalar_string(value, "version"))
        .transpose()?;

    let services_raw = required_mapping(top, "services", "services")?;
    if services_raw.is_empty() {
        return Err(invalid("services", "services must not be empty"));
    }
    let mut services = BTreeMap::new();
    for (name, raw) in services_raw {
        let name = key_string(name, "services")?;
        identifier(&name, &format!("services.{name}"))?;
        services.insert(name.clone(), parse_service(&name, raw)?);
    }
    let volumes = parse_resources(get(top, "volumes"), "volumes", VOLUME_KEYS, false)?;
    let networks = parse_resources(get(top, "networks"), "networks", NETWORK_KEYS, true)?;
    validate_references(&services, &volumes, &networks)?;
    Ok(ComposeDocument {
        name,
        version,
        services,
        volumes,
        networks,
    })
}

/// Build deterministic review data from a parsed document.  It has no path or system effects.
pub fn plan_compose(document: ComposeDocument) -> Result<ComposePlan, ComposeError> {
    let normalized_json = serde_json::to_string(&document).map_err(|error| {
        ComposeError::new(
            ComposeErrorCode::InvalidValue,
            "compose",
            error.to_string(),
            "use only serializable Bosn Compose values",
        )
    })?;
    let digest = format!("sha256:{:x}", Sha256::digest(normalized_json.as_bytes()));
    Ok(ComposePlan {
        version: COMPOSE_PLAN_VERSION,
        document,
        normalized_json,
        digest,
    })
}

pub fn parse_and_plan_compose_yaml(source: &str) -> Result<ComposePlan, ComposeError> {
    plan_compose(parse_compose_yaml(source)?)
}

fn parse_service(name: &str, raw: &Value) -> Result<ServiceSpec, ComposeError> {
    let path = format!("services.{name}");
    let body = mapping(raw, &path)?;
    check_keys(body, SERVICE_KEYS, &path)?;
    let image = optional_string(body, "image", &format!("{path}.image"))?;
    if image.as_deref().is_some_and(str::is_empty) {
        return Err(invalid(format!("{path}.image"), "image must not be empty"));
    }
    let build = match get(body, "build") {
        Some(value) => Some(parse_build(value, &format!("{path}.build"))?),
        None => None,
    };
    if image.is_none() && build.is_none() {
        return Err(invalid(
            &path,
            "a planned service must declare image or build",
        ));
    }
    let profiles =
        optional_string_list(body, "profiles", &format!("{path}.profiles"))?.unwrap_or_default();
    let mounts = match get(body, "volumes") {
        Some(value) => parse_mounts(value, &format!("{path}.volumes"))?,
        None => Vec::new(),
    };
    let networks = match get(body, "networks") {
        Some(value) => parse_network_refs(value, &format!("{path}.networks"))?,
        None => BTreeSet::new(),
    };
    let environment = match get(body, "environment") {
        Some(value) => parse_string_map_or_list(value, &format!("{path}.environment"))?,
        None => BTreeMap::new(),
    };
    let ports = match get(body, "ports") {
        Some(value) => string_list(value, &format!("{path}.ports"))?,
        None => Vec::new(),
    };
    let depends_on = match get(body, "depends_on") {
        Some(value) => parse_dependencies(value, &format!("{path}.depends_on"))?,
        None => BTreeMap::new(),
    };
    let healthcheck = match get(body, "healthcheck") {
        Some(Value::Null) => None,
        Some(value) => Some(parse_healthcheck(value, &format!("{path}.healthcheck"))?),
        None => None,
    };
    let labels = match get(body, "labels") {
        Some(value) => parse_string_map_or_list(value, &format!("{path}.labels"))?,
        None => BTreeMap::new(),
    };
    let command = optional_command(body, "command", &format!("{path}.command"))?;
    let entrypoint = optional_command(body, "entrypoint", &format!("{path}.entrypoint"))?;
    let restart = optional_string(body, "restart", &format!("{path}.restart"))?;
    if let Some(restart) = &restart
        && !["no", "always", "on-failure", "unless-stopped"].contains(&restart.as_str())
    {
        return Err(ComposeError::unsupported(
            format!("{path}.restart"),
            "use no, always, on-failure, or unless-stopped",
        ));
    }
    let container_name =
        optional_string(body, "container_name", &format!("{path}.container_name"))?;
    if container_name.as_deref().is_some_and(str::is_empty) {
        return Err(invalid(
            format!("{path}.container_name"),
            "container_name must not be empty",
        ));
    }
    let mut mounts = mounts;
    if let Some(value) = get(body, "tmpfs") {
        for target in string_or_list(value, &format!("{path}.tmpfs"))? {
            mounts.push(MountSpec::Tmpfs {
                target: normalize_container_path(&target, &format!("{path}.tmpfs"))?,
            });
        }
    }
    Ok(ServiceSpec {
        image,
        build,
        profiles,
        mounts,
        networks,
        environment,
        ports,
        depends_on,
        healthcheck,
        labels,
        command,
        entrypoint,
        restart,
        container_name,
    })
}

fn parse_build(value: &Value, path: &str) -> Result<BuildSpec, ComposeError> {
    match value {
        Value::String(context) => Ok(BuildSpec {
            context: normalize_relative_path(context, path)?,
            dockerfile: RelativePath("Dockerfile".into()),
        }),
        Value::Mapping(body) => {
            check_keys(body, &["context", "dockerfile"], path)?;
            let context = required_string(body, "context", &format!("{path}.context"))?;
            let dockerfile = optional_string(body, "dockerfile", &format!("{path}.dockerfile"))?
                .unwrap_or_else(|| "Dockerfile".into());
            Ok(BuildSpec {
                context: normalize_relative_path(&context, &format!("{path}.context"))?,
                dockerfile: normalize_relative_path(&dockerfile, &format!("{path}.dockerfile"))?,
            })
        }
        _ => Err(shape(
            path,
            "build must be a relative context string or mapping",
        )),
    }
}

fn parse_mounts(value: &Value, path: &str) -> Result<Vec<MountSpec>, ComposeError> {
    let list = sequence(value, path)?;
    list.iter()
        .enumerate()
        .map(|(index, mount)| parse_mount(mount, &format!("{path}[{index}]")))
        .collect()
}

fn parse_mount(value: &Value, path: &str) -> Result<MountSpec, ComposeError> {
    match value {
        Value::String(value) => parse_short_mount(value, path),
        Value::Mapping(body) => {
            check_keys(
                body,
                &[
                    "type",
                    "source",
                    "target",
                    "read_only",
                    "bind",
                    "volume",
                    "tmpfs",
                ],
                path,
            )?;
            let kind = required_string(body, "type", &format!("{path}.type"))?;
            let target = required_string(body, "target", &format!("{path}.target"))?;
            let target = normalize_container_path(&target, &format!("{path}.target"))?;
            let read_only =
                optional_bool(body, "read_only", &format!("{path}.read_only"))?.unwrap_or(false);
            match kind.as_str() {
                "bind" => {
                    reject_present(body, "bind", &format!("{path}.bind"))?;
                    let source = required_string(body, "source", &format!("{path}.source"))?;
                    Ok(MountSpec::Bind {
                        source: normalize_relative_path(&source, &format!("{path}.source"))?,
                        target,
                        read_only,
                    })
                }
                "volume" => {
                    reject_present(body, "volume", &format!("{path}.volume"))?;
                    let source = required_string(body, "source", &format!("{path}.source"))?;
                    identifier(&source, &format!("{path}.source"))?;
                    Ok(MountSpec::Volume {
                        source,
                        target,
                        read_only,
                    })
                }
                "tmpfs" => {
                    reject_present(body, "tmpfs", &format!("{path}.tmpfs"))?;
                    if get(body, "source").is_some() || read_only {
                        return Err(ComposeError::unsupported(
                            path,
                            "tmpfs mounts may set only type and target in this planning foundation",
                        ));
                    }
                    Ok(MountSpec::Tmpfs { target })
                }
                _ => Err(ComposeError::unsupported(
                    format!("{path}.type"),
                    "use bind, volume, or tmpfs",
                )),
            }
        }
        _ => Err(shape(path, "volume entry must be a string or mapping")),
    }
}

fn parse_short_mount(value: &str, path: &str) -> Result<MountSpec, ComposeError> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() < 2 || parts.len() > 3 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(ComposeError::new(
            ComposeErrorCode::Ambiguous,
            path,
            "anonymous or ambiguous short-form mounts are not representable",
            "use an explicit bind/volume mapping with source and target",
        ));
    }
    let read_only = match parts.get(2).copied() {
        None | Some("rw") => false,
        Some("ro") => true,
        Some(_) => {
            return Err(ComposeError::unsupported(
                path,
                "short-form mounts support only :ro or :rw; use an explicit mapping for other modes",
            ));
        }
    };
    let target = normalize_container_path(parts[1], path)?;
    if parts[0].starts_with('.') {
        Ok(MountSpec::Bind {
            source: normalize_relative_path(parts[0], path)?,
            target,
            read_only,
        })
    } else {
        identifier(parts[0], path)?;
        Ok(MountSpec::Volume {
            source: parts[0].into(),
            target,
            read_only,
        })
    }
}

fn parse_network_refs(value: &Value, path: &str) -> Result<BTreeSet<String>, ComposeError> {
    let mut names = BTreeSet::new();
    match value {
        Value::Sequence(entries) => {
            for (index, entry) in entries.iter().enumerate() {
                let name = scalar_string(entry, &format!("{path}[{index}]"))?;
                identifier(&name, &format!("{path}[{index}]"))?;
                names.insert(name);
            }
        }
        Value::Mapping(entries) => {
            for (key, config) in entries {
                let name = key_string(key, path)?;
                identifier(&name, &format!("{path}.{name}"))?;
                if !matches!(config, Value::Null | Value::Mapping(_)) {
                    return Err(shape(
                        format!("{path}.{name}"),
                        "network configuration must be null or a mapping",
                    ));
                }
                if let Value::Mapping(config) = config {
                    check_keys(
                        config,
                        &[
                            "aliases",
                            "ipv4_address",
                            "ipv6_address",
                            "link_local_ips",
                            "priority",
                            "gw_priority",
                            "mac_address",
                        ],
                        &format!("{path}.{name}"),
                    )?;
                    if !config.is_empty() {
                        return Err(ComposeError::unsupported(
                            format!("{path}.{name}"),
                            "per-service network options are not yet represented; use a name-only network reference",
                        ));
                    }
                }
                names.insert(name);
            }
        }
        _ => return Err(shape(path, "networks must be a list or mapping")),
    }
    Ok(names)
}

fn parse_dependencies(
    value: &Value,
    path: &str,
) -> Result<BTreeMap<String, DependencySpec>, ComposeError> {
    let mut result = BTreeMap::new();
    match value {
        Value::Sequence(entries) => {
            for (index, entry) in entries.iter().enumerate() {
                let name = scalar_string(entry, &format!("{path}[{index}]"))?;
                identifier(&name, &format!("{path}[{index}]"))?;
                result.insert(
                    name,
                    DependencySpec {
                        condition: "service_started".into(),
                        restart: false,
                        required: true,
                    },
                );
            }
        }
        Value::Mapping(entries) => {
            for (key, value) in entries {
                let name = key_string(key, path)?;
                identifier(&name, &format!("{path}.{name}"))?;
                let config = mapping(value, &format!("{path}.{name}"))?;
                check_keys(
                    config,
                    &["condition", "restart", "required"],
                    &format!("{path}.{name}"),
                )?;
                let condition =
                    optional_string(config, "condition", &format!("{path}.{name}.condition"))?
                        .unwrap_or_else(|| "service_started".into());
                if ![
                    "service_started",
                    "service_healthy",
                    "service_completed_successfully",
                ]
                .contains(&condition.as_str())
                {
                    return Err(ComposeError::unsupported(
                        format!("{path}.{name}.condition"),
                        "use service_started, service_healthy, or service_completed_successfully",
                    ));
                }
                let restart = optional_bool(config, "restart", &format!("{path}.{name}.restart"))?
                    .unwrap_or(false);
                let required =
                    optional_bool(config, "required", &format!("{path}.{name}.required"))?
                        .unwrap_or(true);
                result.insert(
                    name,
                    DependencySpec {
                        condition,
                        restart,
                        required,
                    },
                );
            }
        }
        _ => return Err(shape(path, "depends_on must be a list or mapping")),
    }
    Ok(result)
}

fn parse_healthcheck(value: &Value, path: &str) -> Result<HealthcheckSpec, ComposeError> {
    let body = mapping(value, path)?;
    check_keys(
        body,
        &[
            "test",
            "interval",
            "timeout",
            "retries",
            "start_period",
            "start_interval",
            "disable",
        ],
        path,
    )?;
    let test = match get(body, "test") {
        Some(value) => string_or_list(value, &format!("{path}.test"))?,
        None => Vec::new(),
    };
    if test.is_empty() {
        return Err(invalid(
            format!("{path}.test"),
            "healthcheck.test must not be empty",
        ));
    }
    Ok(HealthcheckSpec {
        test,
        interval: optional_string(body, "interval", &format!("{path}.interval"))?,
        timeout: optional_string(body, "timeout", &format!("{path}.timeout"))?,
        retries: optional_u64(body, "retries", &format!("{path}.retries"))?,
        start_period: optional_string(body, "start_period", &format!("{path}.start_period"))?,
        start_interval: optional_string(body, "start_interval", &format!("{path}.start_interval"))?,
        disable: optional_bool(body, "disable", &format!("{path}.disable"))?.unwrap_or(false),
    })
}

fn parse_resources(
    raw: Option<&Value>,
    section: &str,
    allowed: &[&str],
    is_network: bool,
) -> Result<BTreeMap<String, ResourceSpec>, ComposeError> {
    let Some(raw) = raw else {
        return Ok(BTreeMap::new());
    };
    let entries = mapping(raw, section)?;
    let mut resources = BTreeMap::new();
    for (key, config) in entries {
        let name = key_string(key, section)?;
        identifier(&name, &format!("{section}.{name}"))?;
        let body = match config {
            Value::Null => {
                resources.insert(name, empty_resource(is_network));
                continue;
            }
            Value::Mapping(body) => body,
            _ => {
                return Err(shape(
                    format!("{section}.{name}"),
                    "resource must be null or a mapping",
                ));
            }
        };
        check_keys(body, allowed, &format!("{section}.{name}"))?;
        resources.insert(
            name.clone(),
            ResourceSpec {
                driver: optional_string(body, "driver", &format!("{section}.{name}.driver"))?,
                driver_opts: match get(body, "driver_opts") {
                    Some(value) => {
                        parse_string_map(value, &format!("{section}.{name}.driver_opts"))?
                    }
                    None => BTreeMap::new(),
                },
                labels: match get(body, "labels") {
                    Some(value) => {
                        parse_string_map_or_list(value, &format!("{section}.{name}.labels"))?
                    }
                    None => BTreeMap::new(),
                },
                external: optional_bool(body, "external", &format!("{section}.{name}.external"))?
                    .unwrap_or(false),
                name: optional_string(body, "name", &format!("{section}.{name}.name"))?,
                internal: if is_network {
                    Some(
                        optional_bool(body, "internal", &format!("{section}.{name}.internal"))?
                            .unwrap_or(false),
                    )
                } else {
                    None
                },
            },
        );
    }
    Ok(resources)
}

fn empty_resource(is_network: bool) -> ResourceSpec {
    ResourceSpec {
        driver: None,
        driver_opts: BTreeMap::new(),
        labels: BTreeMap::new(),
        external: false,
        name: None,
        internal: is_network.then_some(false),
    }
}

fn validate_references(
    services: &BTreeMap<String, ServiceSpec>,
    volumes: &BTreeMap<String, ResourceSpec>,
    networks: &BTreeMap<String, ResourceSpec>,
) -> Result<(), ComposeError> {
    for (service, spec) in services {
        for mount in &spec.mounts {
            if let MountSpec::Volume { source, .. } = mount
                && !volumes.contains_key(source)
            {
                return Err(ComposeError::new(
                    ComposeErrorCode::Ambiguous,
                    format!("services.{service}.volumes"),
                    format!("references undeclared top-level volume {source:?}"),
                    "declare it under top-level volumes before planning",
                ));
            }
        }
        for network in &spec.networks {
            if !networks.contains_key(network) {
                return Err(ComposeError::new(
                    ComposeErrorCode::Ambiguous,
                    format!("services.{service}.networks"),
                    format!("references undeclared top-level network {network:?}"),
                    "declare it under top-level networks before planning",
                ));
            }
        }
        for dependency in spec.depends_on.keys() {
            if !services.contains_key(dependency) {
                return Err(ComposeError::new(
                    ComposeErrorCode::Ambiguous,
                    format!("services.{service}.depends_on"),
                    format!("references missing service {dependency:?}"),
                    "declare the dependency under services before planning",
                ));
            }
        }
    }
    Ok(())
}

fn resolve_merges(value: Value, path: &str) -> Result<Value, ComposeError> {
    match value {
        Value::Sequence(values) => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| resolve_merges(value, &format!("{path}[{index}]")))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Sequence),
        Value::Mapping(values) => {
            let mut output = Mapping::new();
            let mut merge_values = Vec::new();
            for (key, value) in values {
                if key.as_str() == Some("<<") {
                    merge_values.push(value);
                } else {
                    output.insert(key, resolve_merges(value, path)?);
                }
            }
            for merge in merge_values.into_iter().rev() {
                let sources = match merge {
                    Value::Mapping(map) => vec![map],
                    Value::Sequence(items) => items
                        .into_iter()
                        .map(|item| match item {
                            Value::Mapping(map) => Ok(map),
                            _ => Err(shape(path, "YAML merge values must be mappings")),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err(shape(path, "YAML merge values must be mappings")),
                };
                for source in sources.into_iter().rev() {
                    for (key, value) in source {
                        output.entry(key).or_insert(resolve_merges(value, path)?);
                    }
                }
            }
            Ok(Value::Mapping(output))
        }
        other => Ok(other),
    }
}

fn check_keys(map: &Mapping, allowed: &[&str], path: &str) -> Result<(), ComposeError> {
    for key in map.keys() {
        let key = key_string(key, path)?;
        if key.starts_with("x-") || allowed.contains(&key.as_str()) {
            continue;
        }
        return Err(ComposeError::unsupported(
            join(path, &key),
            format!("supported fields are {}", allowed.join(", ")),
        ));
    }
    Ok(())
}

fn mapping<'a>(value: &'a Value, path: &str) -> Result<&'a Mapping, ComposeError> {
    value
        .as_mapping()
        .ok_or_else(|| shape(path, "must be a YAML mapping"))
}
fn sequence<'a>(value: &'a Value, path: &str) -> Result<&'a Vec<Value>, ComposeError> {
    value
        .as_sequence()
        .ok_or_else(|| shape(path, "must be a YAML list"))
}
fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.into()))
}
fn key_string(value: &Value, path: &str) -> Result<String, ComposeError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| shape(path, "mapping keys must be strings"))
}
fn required_mapping<'a>(
    map: &'a Mapping,
    key: &str,
    path: &str,
) -> Result<&'a Mapping, ComposeError> {
    map.get(Value::String(key.into()))
        .ok_or_else(|| invalid(path, format!("missing required {key:?} mapping")))
        .and_then(|value| mapping(value, path))
}
fn required_string(map: &Mapping, key: &str, path: &str) -> Result<String, ComposeError> {
    get(map, key)
        .ok_or_else(|| invalid(path, format!("missing required {key:?} string")))
        .and_then(|value| scalar_string(value, path))
}
fn optional_string(map: &Mapping, key: &str, path: &str) -> Result<Option<String>, ComposeError> {
    get(map, key)
        .map(|value| scalar_string(value, path))
        .transpose()
}
fn optional_bool(map: &Mapping, key: &str, path: &str) -> Result<Option<bool>, ComposeError> {
    get(map, key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| shape(path, "must be a boolean"))
        })
        .transpose()
}
fn optional_u64(map: &Mapping, key: &str, path: &str) -> Result<Option<u64>, ComposeError> {
    get(map, key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| shape(path, "must be a non-negative integer"))
        })
        .transpose()
}
fn optional_string_list(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ComposeError> {
    get(map, key)
        .map(|value| string_list(value, path))
        .transpose()
}
fn optional_command(
    map: &Mapping,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ComposeError> {
    get(map, key)
        .map(|value| string_or_list(value, path))
        .transpose()
}
fn scalar_string(value: &Value, path: &str) -> Result<String, ComposeError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        _ => Err(shape(path, "must be a scalar string, number, or boolean")),
    }
}
fn string_list(value: &Value, path: &str) -> Result<Vec<String>, ComposeError> {
    sequence(value, path)?
        .iter()
        .enumerate()
        .map(|(index, value)| scalar_string(value, &format!("{path}[{index}]")))
        .collect()
}
fn string_or_list(value: &Value, path: &str) -> Result<Vec<String>, ComposeError> {
    match value {
        Value::String(value) => Ok(vec![value.clone()]),
        Value::Sequence(_) => string_list(value, path),
        _ => Err(shape(path, "must be a string or list of strings")),
    }
}
fn parse_string_map(value: &Value, path: &str) -> Result<BTreeMap<String, String>, ComposeError> {
    let map = mapping(value, path)?;
    map.iter()
        .map(|(key, value)| {
            Ok((
                key_string(key, path)?,
                scalar_string(value, &join(path, &key_string(key, path)?))?,
            ))
        })
        .collect()
}
fn parse_string_map_or_list(
    value: &Value,
    path: &str,
) -> Result<BTreeMap<String, String>, ComposeError> {
    match value {
        Value::Mapping(_) => parse_string_map(value, path),
        Value::Sequence(values) => {
            let mut result = BTreeMap::new();
            for (index, value) in values.iter().enumerate() {
                let entry = scalar_string(value, &format!("{path}[{index}]"))?;
                let Some((key, value)) = entry.split_once('=') else {
                    return Err(ComposeError::new(
                        ComposeErrorCode::Ambiguous,
                        format!("{path}[{index}]"),
                        "list entries must contain an explicit key=value pair",
                        "use mapping syntax for inherited environment values",
                    ));
                };
                if key.is_empty() {
                    return Err(invalid(format!("{path}[{index}]"), "key must not be empty"));
                }
                result.insert(key.into(), value.into());
            }
            Ok(result)
        }
        _ => Err(shape(path, "must be a mapping or key=value list")),
    }
}
fn reject_present(map: &Mapping, key: &str, path: &str) -> Result<(), ComposeError> {
    if get(map, key).is_some() {
        return Err(ComposeError::unsupported(
            path,
            "nested mount options are not yet represented in Bosn's plan",
        ));
    }
    Ok(())
}
fn identifier(value: &str, path: &str) -> Result<(), ComposeError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ComposeError::new(
            ComposeErrorCode::InvalidValue,
            path,
            format!("{value:?} is not a safe Compose identifier"),
            "use a non-empty ASCII name containing only letters, digits, _, -, or .",
        ));
    }
    Ok(())
}
fn normalize_relative_path(value: &str, path: &str) -> Result<RelativePath, ComposeError> {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') {
        return Err(unsafe_path(
            path,
            "path must be a non-empty slash-separated relative path",
        ));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(unsafe_path(path, "path traversal is not permitted")),
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Ok(RelativePath(".".into()));
    }
    Ok(RelativePath(parts.join("/")))
}
fn normalize_container_path(value: &str, path: &str) -> Result<String, ComposeError> {
    if !value.starts_with('/') || value.contains('\\') {
        return Err(unsafe_path(
            path,
            "container target must be an absolute slash-separated path",
        ));
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(unsafe_path(
                    path,
                    "container target must not traverse with ..",
                ));
            }
            _ => parts.push(part),
        }
    }
    Ok(if parts.is_empty() {
        "/".into()
    } else {
        format!("/{}", parts.join("/"))
    })
}
fn join(prefix: &str, field: &str) -> String {
    if prefix == "compose" {
        field.into()
    } else {
        format!("{prefix}.{field}")
    }
}
fn shape(path: impl Into<String>, message: impl Into<String>) -> ComposeError {
    ComposeError::new(
        ComposeErrorCode::InvalidShape,
        path,
        message,
        "use the documented typed Compose shape",
    )
}
fn invalid(path: impl Into<String>, message: impl Into<String>) -> ComposeError {
    ComposeError::new(
        ComposeErrorCode::InvalidValue,
        path,
        message,
        "correct the value before planning",
    )
}
fn unsafe_path(path: impl Into<String>, message: impl Into<String>) -> ComposeError {
    ComposeError::new(
        ComposeErrorCode::UnsafePath,
        path,
        message,
        "use a relative path contained by the selected root",
    )
}
