//! Pure machine/app policy parsing; callers supply every external observation.

use std::collections::BTreeMap;
use std::fmt;

pub const POLICY_KEYS: [&str; 9] = [
    "container_idle_stop",
    "container_remove",
    "warm_volume_ttl",
    "superseded_cap",
    "shared_cache_ceiling",
    "run_max_duration",
    "idle_retire_seconds",
    "build_ttl_seconds",
    "max_builds",
];
const APP_KEYS: [&str; 2] = ["run_max_duration", "build_ttl_seconds"];
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyOrigin {
    Default,
    MachineFile,
    MachineEnvironment,
    MachineFlag,
    App,
}
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyValue {
    pub value: f64,
    pub origin: PolicyOrigin,
}
#[derive(Clone, Debug, PartialEq)]
pub struct MachinePolicy {
    values: BTreeMap<String, PolicyValue>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AppPolicy {
    machine: MachinePolicy,
    values: BTreeMap<String, PolicyValue>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyDefaults {
    values: BTreeMap<String, f64>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyError(pub String);
impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PolicyError {}
impl PolicyDefaults {
    pub fn for_cpu_count(cpu_count: Option<usize>) -> Self {
        Self {
            values: BTreeMap::from([
                ("container_idle_stop".into(), 3600.),
                ("container_remove".into(), 86400.),
                ("warm_volume_ttl".into(), 259200.),
                ("superseded_cap".into(), 86400.),
                (
                    "shared_cache_ceiling".into(),
                    (100_u64 * 1024_u64.pow(3)) as f64,
                ),
                ("run_max_duration".into(), 28800.),
                ("idle_retire_seconds".into(), 900.),
                ("build_ttl_seconds".into(), 3600.),
                (
                    "max_builds".into(),
                    cpu_count.map_or(2, |n| (n / 2).max(2)) as f64,
                ),
            ]),
        }
    }
}
impl MachinePolicy {
    pub fn get(&self, key: &str) -> Option<f64> {
        self.values.get(key).map(|v| v.value)
    }
    pub fn origin(&self, key: &str) -> Option<PolicyOrigin> {
        self.values.get(key).map(|v| v.origin)
    }
    pub fn values(&self) -> &BTreeMap<String, PolicyValue> {
        &self.values
    }
}
impl AppPolicy {
    pub fn get(&self, key: &str) -> Option<f64> {
        self.values.get(key).map(|v| v.value)
    }
    pub fn origin(&self, key: &str) -> Option<PolicyOrigin> {
        self.values.get(key).map(|v| v.origin)
    }
    pub fn machine(&self) -> &MachinePolicy {
        &self.machine
    }
}
pub fn resolve_machine_policy<MF, ME, MC, K, V>(
    defaults: PolicyDefaults,
    file: MF,
    environment: ME,
    cli: MC,
) -> Result<MachinePolicy, PolicyError>
where
    MF: IntoIterator<Item = (K, V)>,
    ME: IntoIterator<Item = (K, V)>,
    MC: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: ToString,
{
    let mut values = defaults
        .values
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                PolicyValue {
                    value: v,
                    origin: PolicyOrigin::Default,
                },
            )
        })
        .collect();
    apply(&mut values, file, PolicyOrigin::MachineFile)?;
    apply(&mut values, environment, PolicyOrigin::MachineEnvironment)?;
    apply(&mut values, cli, PolicyOrigin::MachineFlag)?;
    Ok(MachinePolicy { values })
}
pub fn resolve_app_policy<I, K, V>(
    machine: &MachinePolicy,
    app: I,
) -> Result<AppPolicy, PolicyError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: ToString,
{
    let mut values = machine.values.clone();
    for (key, raw) in app {
        let key = key.as_ref();
        if !APP_KEYS.contains(&key) {
            return Err(PolicyError(format!(
                "app policy {key:?} is not permitted to override machine policy"
            )));
        }
        let value = number(key, raw.to_string(), "app")?;
        let ceiling = machine.get(key).ok_or_else(|| {
            PolicyError(format!("missing machine ceiling for app policy {key:?}"))
        })?;
        if value > ceiling {
            return Err(PolicyError(format!(
                "app policy {key:?} ({value}) exceeds machine ceiling ({ceiling})"
            )));
        }
        values.insert(
            key.into(),
            PolicyValue {
                value,
                origin: PolicyOrigin::App,
            },
        );
    }
    Ok(AppPolicy {
        machine: machine.clone(),
        values,
    })
}
pub fn parse_machine_policy_toml<ME, MC, K, V>(
    source: &str,
    defaults: PolicyDefaults,
    environment: ME,
    cli: MC,
) -> Result<MachinePolicy, PolicyError>
where
    ME: IntoIterator<Item = (K, V)>,
    MC: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: ToString,
{
    let value: toml::Value = source
        .parse()
        .map_err(|e: toml::de::Error| PolicyError(format!("invalid config TOML: {e}")))?;
    let root = value
        .as_table()
        .ok_or_else(|| PolicyError("config must be a table".into()))?;
    if root.keys().any(|key| key != "policy") {
        return Err(PolicyError("config may contain only [policy]".into()));
    }
    let table = root
        .get("policy")
        .ok_or_else(|| PolicyError("config must contain [policy]".into()))?
        .as_table()
        .ok_or_else(|| PolicyError("[policy] must be a table".into()))?;
    let file = table
        .iter()
        .map(|(key, value)| match value {
            toml::Value::Integer(v) => Ok((key.clone(), v.to_string())),
            toml::Value::Float(v) => Ok((key.clone(), v.to_string())),
            _ => Err(PolicyError(format!(
                "invalid config key {key:?}: expected a positive finite number"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let environment = environment
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_owned(), value.to_string()))
        .collect::<Vec<_>>();
    let cli = cli
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_owned(), value.to_string()))
        .collect::<Vec<_>>();
    resolve_machine_policy(defaults, file, environment, cli)
}
fn apply<I, K, V>(
    target: &mut BTreeMap<String, PolicyValue>,
    values: I,
    origin: PolicyOrigin,
) -> Result<(), PolicyError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: ToString,
{
    for (key, raw) in values {
        let key = key.as_ref();
        if !target.contains_key(key) {
            return Err(PolicyError(format!("invalid config key {key:?}")));
        }
        target.insert(
            key.into(),
            PolicyValue {
                value: number(key, raw.to_string(), "machine")?,
                origin,
            },
        );
    }
    Ok(())
}
fn number(key: &str, raw: String, origin: &str) -> Result<f64, PolicyError> {
    let value = raw.parse::<f64>().map_err(|_| {
        PolicyError(format!(
            "invalid config key {key:?} from {origin}: expected a positive finite number"
        ))
    })?;
    if !value.is_finite() || value <= 0. {
        return Err(PolicyError(format!(
            "invalid config key {key:?} from {origin}: expected a positive finite number"
        )));
    }
    Ok(value)
}
