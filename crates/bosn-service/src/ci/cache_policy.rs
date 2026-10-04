//! Explicit archive budgets; these do not describe allocated filesystem blocks.
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "Input")]
pub struct CachePolicy {
    pub repository_max_bytes: i64,
    pub aggregate_max_bytes: i64,
    pub max_age_secs: u64,
    pub unused_age_secs: u64,
    pub maintenance_interval_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    repository_max_bytes: i64,
    aggregate_max_bytes: i64,
    max_age_secs: u64,
    unused_age_secs: u64,
    maintenance_interval_secs: u64,
}

impl TryFrom<Input> for CachePolicy {
    type Error = String;
    fn try_from(input: Input) -> Result<Self, Self::Error> {
        if input.repository_max_bytes <= 0 || input.aggregate_max_bytes < input.repository_max_bytes
        {
            return Err("cache requires positive repository bytes and an aggregate ceiling at least as large".into());
        }
        // act2 represents durations as signed nanoseconds. Reject overflow at
        // the boundary, before constructing argv or creating an engine.
        let max_seconds = (i64::MAX / 1_000_000_000) as u64;
        if [
            input.max_age_secs,
            input.unused_age_secs,
            input.maintenance_interval_secs,
        ]
        .iter()
        .any(|seconds| *seconds == 0 || *seconds > max_seconds)
        {
            return Err("cache durations must be positive seconds representable by act2".into());
        }
        Ok(Self {
            repository_max_bytes: input.repository_max_bytes,
            aggregate_max_bytes: input.aggregate_max_bytes,
            max_age_secs: input.max_age_secs,
            unused_age_secs: input.unused_age_secs,
            maintenance_interval_secs: input.maintenance_interval_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const VALID: &str = "repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=300\n";
    #[test]
    fn policy_refuses_disabled_ceilings_overflow_and_typos() {
        let policy: CachePolicy = toml::from_str(VALID).unwrap();
        assert_eq!(policy.repository_max_bytes, 100);
        for (from, to) in [
            ("repository_max_bytes=100", "repository_max_bytes=0"),
            ("repository_max_bytes=100", "repository_max_bytes=-1"),
            ("aggregate_max_bytes=200", "aggregate_max_bytes=99"),
            (
                "maintenance_interval_secs=300",
                "maintenance_interval_secs=0",
            ),
            ("max_age_secs=2592000", "max_age_secs=18446744073709551615"),
            ("unused_age_secs", "unused_age_seconds"),
        ] {
            assert!(
                toml::from_str::<CachePolicy>(&VALID.replace(from, to)).is_err(),
                "{to}"
            );
        }
    }
}
