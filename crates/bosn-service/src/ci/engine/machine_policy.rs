//! Immutable policy agreement for participating Bosn cache users.
//! This is not repository enrollment or exclusion of older daemon versions.
use super::{CACHE_VOLUME, CONTROL_DEADLINE, DockerActBackend, ENGINE_CACHE};
use crate::ci::cache_policy::CachePolicy;

impl DockerActBackend {
    /// A maintenance helper must observe an existing agreement on its mounted
    /// volume. A replaced cache must not inherit a stale in-memory policy.
    pub(super) async fn require_cache_policy(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<(), String> {
        let output = self
            .checked(
                "existing maintenance policy",
                Self::exec(engine, &READ_POLICY.replace("/cache", ENGINE_CACHE)),
                CONTROL_DEADLINE,
            )
            .await?;
        if decode_record(&output)? != Some(policy) {
            return Err("existing machine cache policy is absent or differs".into());
        }
        Ok(())
    }

    /// Read participating policy without bootstrapping, enrolling or changing it.
    /// Absence is a sample; errors never authorize a default policy.
    pub async fn discover_cache_policy(
        &self,
        registry: &crate::RegistryActor,
        owner: &str,
    ) -> Result<Option<CachePolicy>, String> {
        if !self.volume_exists(CACHE_VOLUME).await? {
            return Ok(None);
        }
        let output = self
            .read_cache_tracked(CACHE_VOLUME, Some((registry, owner)), READ_POLICY)
            .await?;
        decode_record(&output)
    }

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
    let canonical = canonical_record(policy);
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

fn canonical_record(policy: CachePolicy) -> String {
    // Only validated numeric fields enter shell text. A fixed representation
    // makes equality independent of JSON ordering or caller formatting.
    format!(
        "schema=1\nrepository_max_bytes={}\naggregate_max_bytes={}\nmax_age_secs={}\nunused_age_secs={}\nmaintenance_interval_secs={}\n",
        policy.repository_max_bytes,
        policy.aggregate_max_bytes,
        policy.max_age_secs,
        policy.unused_age_secs,
        policy.maintenance_interval_secs,
    )
}

const READ_POLICY: &str = "set -eu; directory=/cache/actcache; \
    [ ! -L \"$directory\" ] || { echo 'machine policy directory is a symlink' >&2; exit 1; }; \
    record=\"$directory/.bosn-cohort-policy-v1\"; \
    [ ! -L \"$record\" ] || { echo 'machine policy record is a symlink' >&2; exit 1; }; \
    if [ ! -e \"$record\" ]; then printf 'absent\\n'; exit 0; fi; \
    [ -f \"$record\" ] && [ -f \"$record.lock\" ] && [ ! -L \"$record.lock\" ] || { echo 'machine policy record or lock is invalid' >&2; exit 1; }; \
    exec 7<\"$record.lock\"; flock -s -n 7 || { echo 'machine policy read busy' >&2; exit 75; }; \
    printf 'present\\n'; head -c 1025 \"$record\"; printf '\\nend'";

fn decode_record(output: &str) -> Result<Option<CachePolicy>, String> {
    if output == "absent" {
        return Ok(None);
    }
    let record = output
        .strip_prefix("present\n")
        .and_then(|value| value.strip_suffix("\nend"))
        .filter(|record| record.len() <= 1024)
        .ok_or("machine policy read is invalid or oversized")?;
    let policy_text = record
        .strip_prefix("schema=1\n")
        .ok_or("unsupported machine policy schema")?;
    let policy: CachePolicy =
        toml::from_str(policy_text).map_err(|_| "machine policy record is invalid")?;
    if canonical_record(policy) != record {
        return Err("machine policy record is not canonical".into());
    }
    Ok(Some(policy))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn discovery_requires_bounded_versioned_canonical_valid_policy() {
        let policy: CachePolicy = toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap();
        let document = format!("present\n{}\nend", canonical_record(policy));
        assert_eq!(decode_record(&document).unwrap(), Some(policy));
        assert_eq!(decode_record("absent").unwrap(), None);
        for invalid in [
            document.replace("schema=1", "schema=2"),
            document.replace("aggregate_max_bytes=200", "aggregate_max_bytes=99"),
            document.replace("max_age_secs=3600", "max_age_secs=0"),
            document.replace("\nend", "unknown=1\n\nend"),
            document.replace("\nend", " \nend"),
            document.replace("present\n", ""),
            format!("present\n{}\nend", " ".repeat(1025)),
        ] {
            assert!(decode_record(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    #[ignore = "requires the isolated bosn-456-live-v2 Docker engine with an agreed policy"]
    fn shared_policy_discovery_uses_a_retired_read_only_helper() {
        assert_eq!(
            std::env::var("DOCKER_HOST").unwrap(),
            "tcp://bosn-456-live-v2-engine:2375"
        );
        crate::ci::lifecycle::tests::with_registry(|registry, _state| async move {
            let backend = DockerActBackend::default();
            let policy = backend
                .discover_cache_policy(&registry, crate::ci::lifecycle::tests::OWNER)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(policy.repository_max_bytes, 104857600);
            assert_eq!(policy.aggregate_max_bytes, 209715200);
            assert_eq!(policy.maintenance_interval_secs, 60);
            let reply = registry
                .act_registry(crate::act_registry::ActRegistryCommand::HelperPending {
                    after_nonce: None,
                    limit: 64,
                })
                .await
                .unwrap();
            let crate::act_registry::ActRegistryReply::Helpers(page) = reply else {
                panic!("helper page required")
            };
            assert!(page.items.is_empty());
            backend.verify_measured_volume(CACHE_VOLUME).await.unwrap();
        });
    }
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
        let reader = READ_POLICY.replace("/cache", directory.path().to_str().unwrap());
        let absent = run(&reader);
        assert!(absent.status.success());
        assert_eq!(
            decode_record(String::from_utf8(absent.stdout).unwrap().trim()).unwrap(),
            None
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(run(&script).status.success());
        let record = directory.path().join("actcache/.bosn-cohort-policy-v1");
        let before = std::fs::read(&record).unwrap();
        let read = run(&reader);
        assert!(read.status.success());
        assert_eq!(
            decode_record(String::from_utf8(read.stdout).unwrap().trim()).unwrap(),
            Some(policy)
        );
        assert_eq!(std::fs::read(&record).unwrap(), before);
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
