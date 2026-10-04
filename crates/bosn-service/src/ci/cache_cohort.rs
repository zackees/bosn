//! act2 arguments for the coordinated root. Production admission remains gated.
use super::{cache_policy::CachePolicy, engine::ENGINE_CACHE};

/// Fixed root shared by fresh engines; legacy sources stay outside this root.
pub fn root() -> String {
    format!("{ENGINE_CACHE}/actcache/cohort-v1")
}

/// An execution route selected by the trusted planner. Choosing a cohort is
/// not enrollment: its caller must first establish shared routing, current
/// inventory and writer exclusion. Production planning currently uses Legacy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheRoute {
    Legacy(Namespace),
    Cohort {
        namespace: Namespace,
        policy: CachePolicy,
    },
}
impl CacheRoute {
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::Legacy(namespace) => vec!["--cache-server-path".into(), namespace.legacy_path()],
            Self::Cohort { namespace, policy } => policy.server_args(namespace),
        }
    }
}

/// Repository identity is a single direct-child hash, never an arbitrary path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Namespace(String);
impl Namespace {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.len() != 16
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("cache namespace must be 16 lowercase hexadecimal characters".into());
        }
        Ok(Self(value.into()))
    }
    pub fn legacy_path(&self) -> String {
        format!("{ENGINE_CACHE}/actcache/{}", self.0)
    }
    pub fn path(&self) -> String {
        format!("{}/{}", root(), self.0)
    }
}

impl CachePolicy {
    fn namespace_policy_args(self) -> Vec<String> {
        vec![
            "--cache-server-max-bytes".into(),
            self.repository_max_bytes.to_string(),
            "--cache-server-max-age".into(),
            format!("{}s", self.max_age_secs),
            "--cache-server-unused-age".into(),
            format!("{}s", self.unused_age_secs),
            "--cache-server-gc-interval".into(),
            format!("{}s", self.maintenance_interval_secs),
        ]
    }

    /// Call only after warm migration establishes the enrolled destination.
    pub fn server_args(self, namespace: &Namespace) -> Vec<String> {
        let mut args = vec![
            "--cache-server-path".into(),
            namespace.path(),
            "--cache-server-cohort-root".into(),
            root(),
            "--cache-server-cohort-max-bytes".into(),
            self.aggregate_max_bytes.to_string(),
        ];
        args.extend(self.namespace_policy_args());
        args
    }

    /// Caller must exclude legacy writers before asserting source quiescence.
    pub fn import_args_for_quiescent_source(self, namespace: &Namespace) -> Vec<String> {
        vec![
            "cache".into(),
            "import".into(),
            "--apply".into(),
            "--source-quiescent".into(),
            "--from".into(),
            namespace.legacy_path(),
            "--namespace".into(),
            namespace.0.clone(),
            "--max-bytes".into(),
            self.repository_max_bytes.to_string(),
            "--cache-server-path".into(),
            root(),
        ]
    }

    /// Independent maintenance covers idle namespaces after all servers close.
    pub fn maintenance_args(self) -> Vec<String> {
        let mut args = vec![
            "cache".into(),
            "prune-cohort".into(),
            "--apply".into(),
            "--cache-server-path".into(),
            root(),
            "--max-bytes".into(),
            self.aggregate_max_bytes.to_string(),
            "--watch".into(),
            format!("{}s", self.maintenance_interval_secs),
        ];
        args.extend(self.namespace_policy_args());
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn namespace_cannot_escape_or_alias_its_repository() {
        let first = Namespace::parse("0123456789abcdef").unwrap();
        let second = Namespace::parse("0123456789abcdee").unwrap();
        assert_ne!(first.path(), second.path());
        for value in [
            "",
            "../0123456789abc",
            "/123456789abcdef",
            "0123456789abcdeF",
            "0123456789abcdef/x",
            "0123456789abcdeg",
        ] {
            assert!(Namespace::parse(value).is_err(), "{value}");
        }
    }
}
