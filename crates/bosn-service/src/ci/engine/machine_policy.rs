//! Immutable policy agreement for participating Bosn cache users.
//! This is not repository enrollment or exclusion of older daemon versions.
use super::{CONTROL_DEADLINE, DockerActBackend, ENGINE_CACHE};
use crate::ci::cache_policy::CachePolicy;

impl DockerActBackend {
    pub(super) async fn agree_cache_policy(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<(), String> {
        self.checked(
            "machine cache policy agreement",
            Self::exec(engine, &agreement_script(policy)),
            CONTROL_DEADLINE,
        )
        .await
        .map(|_| ())
    }
}

fn agreement_script(policy: CachePolicy) -> String {
    // Only validated numeric fields enter shell text. A fixed representation
    // makes equality independent of JSON ordering or caller formatting.
    let canonical = format!(
        "schema=1\nrepository_max_bytes={}\naggregate_max_bytes={}\nmax_age_secs={}\nunused_age_secs={}\nmaintenance_interval_secs={}\n",
        policy.repository_max_bytes,
        policy.aggregate_max_bytes,
        policy.max_age_secs,
        policy.unused_age_secs,
        policy.maintenance_interval_secs,
    );
    format!(
        "set -eu; directory={ENGINE_CACHE}/actcache; mkdir -p \"$directory\"; \
         record=\"$directory/.bosn-cohort-policy-v1\"; \
         exec 7>>\"$record.lock\"; flock -x -n 7 || {{ echo 'machine cache policy agreement busy' >&2; exit 75; }}; \
         umask 077; stage=$(mktemp \"$record.XXXXXXXX\"); trap 'rm -f \"$stage\"' EXIT; \
         printf '%s' '{canonical}' >\"$stage\"; \
         if [ ! -e \"$record\" ]; then ln \"$stage\" \"$record\"; fi; \
         cmp -s \"$stage\" \"$record\" || {{ echo 'machine cache policy conflict; existing policy preserved' >&2; exit 78; }}"
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn shared_policy_agrees_across_restart_and_refuses_conflicting_writers() {
        let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let policy: CachePolicy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap();
        let script =
            agreement_script(policy).replace(ENGINE_CACHE, directory.path().to_str().unwrap());
        let run = |script: &str| {
            std::process::Command::new("sh")
                .args(["-c", script])
                .output()
                .unwrap()
        };
        assert!(run(&script).status.success());
        let record = directory.path().join("actcache/.bosn-cohort-policy-v1");
        let before = std::fs::read(&record).unwrap();
        assert!(run(&script).status.success());
        let mut conflict = policy;
        conflict.aggregate_max_bytes = 300;
        let refusal =
            run(&agreement_script(conflict)
                .replace(ENGINE_CACHE, directory.path().to_str().unwrap()));
        assert_eq!(refusal.status.code(), Some(78));
        assert_eq!(std::fs::read(&record).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(record.parent().unwrap()).unwrap().count(),
            2
        );
    }
}
