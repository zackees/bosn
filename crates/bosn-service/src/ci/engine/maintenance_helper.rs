//! One durable, finite-lived maintenance helper; never a workflow engine.
use super::{
    ActEngineBackend, CACHE_VOLUME, CONTROL_DEADLINE, DockerActBackend, ENGINE_WORK,
    MaintenanceAttempt, act_archive, act_artifact,
    cache_usage::{helper, journal},
    engine_image, owned,
};
use crate::{RegistryActor, ci::cache_policy::CachePolicy};
use bosn_registry::cache_helper::{CacheHelperIntent, CacheHelperRole};

#[derive(Debug)]
pub struct MaintenanceHelperAttempt {
    pub nonce: String,
    pub container_id: String,
    /// Reclamation evidence survives a cleanup failure.
    pub outcome: Result<MaintenanceAttempt, String>,
    /// Success requires explicit Docker absence and a committed journal finish.
    pub cleanup: Result<(), String>,
}

impl DockerActBackend {
    /// The caller must supply the verified machine policy. This does not enroll
    /// repositories, admit legacy writers or authorize source-volume deletion.
    pub async fn maintain_cache_with_helper(
        &self,
        registry: &RegistryActor,
        owner: &str,
        policy: CachePolicy,
    ) -> Result<MaintenanceHelperAttempt, String> {
        self.verify_measured_volume(CACHE_VOLUME).await?;
        self.ensure_engine_image().await?;
        let intent = CacheHelperIntent {
            registry_id: owner.into(),
            nonce: crate::ci::new_uuid().await.map_err(|e| e.to_string())?,
            image: engine_image(),
            volume: CACHE_VOLUME.into(),
            created_at: crate::ci::lifecycle::now_seconds(),
            role: Some(CacheHelperRole::MaintenanceV1),
        };
        let identity = helper::Identity::from_intent(&intent)?;
        let _active = journal::ActiveHelper::claim(self, &intent.nonce);
        let tracker = journal::Tracker::new(registry, &intent.nonce);
        tracker.begin(intent.clone()).await?;
        let args = helper_create_args(&identity);
        let created = self
            .checked("maintenance helper create", args, CONTROL_DEADLINE)
            .await;
        let id = match created {
            Ok(id) if helper::valid_id(&id) => id,
            result => {
                let error = result
                    .err()
                    .unwrap_or_else(|| "maintenance create returned an invalid ID".into());
                let recovery = self
                    .recover_measurement(&identity, CACHE_VOLUME, Some(&tracker))
                    .await;
                return Err(format!(
                    "{error}; helper {} recovery: {recovery:?}",
                    identity.name
                ));
            }
        };
        let outcome = async {
            tracker.register(&id).await?;
            let document = self
                .checked(
                    "maintenance identity",
                    owned(&["container", "inspect", &id]),
                    CONTROL_DEADLINE,
                )
                .await?;
            if identity.verify(&document, CACHE_VOLUME)? != id {
                return Err("maintenance helper ID changed".into());
            }
            self.verify_measured_volume(CACHE_VOLUME).await?;
            self.checked(
                "maintenance start",
                owned(&["start", &id]),
                CONTROL_DEADLINE,
            )
            .await?;
            let arch = self
                .checked(
                    "maintenance architecture",
                    Self::exec(&id, "uname -m"),
                    CONTROL_DEADLINE,
                )
                .await?;
            let act = act_artifact(&arch)
                .ok_or_else(|| format!("unsupported maintenance architecture {arch}"))?;
            let script = offline_install_script(act);
            let version = self
                .checked(
                    "maintenance offline act",
                    Self::exec(&id, &script),
                    CONTROL_DEADLINE,
                )
                .await?;
            if !version.ends_with(super::ACT_VERSION) {
                return Err(format!("maintenance act version mismatch: {version}"));
            }
            self.agree_cache_policy(&id, policy).await?;
            self.maintain_cache_cohort(&id, policy).await
        }
        .await;
        // Re-inspect the complete ownership profile before removal. Recovery
        // also registers the exact ID if the first registry acknowledgement failed.
        let cleanup = if self.confirm_measurement_absent(&id).await.is_ok() {
            async {
                tracker.register(&id).await?;
                tracker.finish(&id).await
            }
            .await
        } else {
            self.recover_measurement(&identity, CACHE_VOLUME, Some(&tracker))
                .await
        };
        Ok(MaintenanceHelperAttempt {
            nonce: intent.nonce,
            container_id: id,
            outcome,
            cleanup,
        })
    }
}

fn helper_create_args(identity: &helper::Identity) -> Vec<String> {
    let mount = format!("type=volume,source={CACHE_VOLUME},target=/bosn/cache");
    let mut args = owned(&[
        "create",
        "--rm",
        "--name",
        &identity.name,
        "--label",
        &identity.label(),
        "--pull",
        "never",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--memory",
        "128m",
        "--cpus",
        "1",
        "--tmpfs",
        "/var/lib/docker:exec,size=64m",
        "--mount",
        &mount,
        "--entrypoint",
        "sleep",
        &engine_image(),
        "300",
    ]);
    args.splice(2..2, identity.ownership_args());
    args
}

fn offline_install_script(act: super::ActArtifact) -> String {
    format!(
        "mkdir -p {ENGINE_WORK}/bin; tgz={archive}; exec 9>>\"$tgz.lock\"; flock -s 9; \
         echo \"{sum}  $tgz\" | sha256sum -c - >/dev/null && \
         tar -xzf \"$tgz\" -C {ENGINE_WORK}/bin act && \
         echo \"{binary}  {ENGINE_WORK}/bin/act\" | sha256sum -c - >/dev/null && \
         {ENGINE_WORK}/bin/act --version",
        archive = act_archive(act),
        sum = act.sha256,
        binary = act.binary_sha256,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::lifecycle::tests::{OWNER, with_registry};
    use bosn_registry::{Registry, cache_helper::CacheHelperState};

    #[test]
    #[ignore = "requires isolated private Docker with verified act archive and cohort cache"]
    fn durable_maintenance_helper_retires_and_preserves_shared_volume() {
        assert!(
            std::env::var("DOCKER_HOST")
                .unwrap()
                .contains("bosn-456-live-v2-engine")
        );
        with_registry(|registry, directory| async move {
            let backend = DockerActBackend::default();
            let policy: CachePolicy = toml::from_str(
                "repository_max_bytes=104857600\naggregate_max_bytes=209715200\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=60\n"
            ).unwrap();
            let result = backend
                .maintain_cache_with_helper(&registry, OWNER, policy)
                .await
                .unwrap();
            assert!(result.cleanup.is_ok(), "{:?}", result.cleanup);
            let record = Registry::open_read_only(directory.join("registry.sqlite3"))
                .unwrap()
                .cache_helper(&result.nonce)
                .unwrap()
                .unwrap();
            assert_eq!(record.state, CacheHelperState::Removed);
            assert_eq!(record.intent.role, Some(CacheHelperRole::MaintenanceV1));
            assert_eq!(
                record.container_id.as_deref(),
                Some(result.container_id.as_str())
            );
            backend
                .confirm_measurement_absent(&result.container_id)
                .await
                .unwrap();
            backend.verify_measured_volume(CACHE_VOLUME).await.unwrap();
            let outcome = result.outcome.unwrap();
            outcome.require_complete().unwrap();
            let mut conflict = policy;
            conflict.aggregate_max_bytes += 1;
            let refused = backend
                .maintain_cache_with_helper(&registry, OWNER, conflict)
                .await
                .unwrap();
            assert!(refused.cleanup.is_ok(), "{:?}", refused.cleanup);
            assert!(refused.outcome.unwrap_err().contains("policy conflict"));
            backend
                .confirm_measurement_absent(&refused.container_id)
                .await
                .unwrap();
            backend.verify_measured_volume(CACHE_VOLUME).await.unwrap();
            assert!(
                outcome
                    .report
                    .namespaces
                    .as_ref()
                    .unwrap()
                    .iter()
                    .all(|ns| ns
                        .retention
                        .as_ref()
                        .is_some_and(|r| r.reclaimed_archive_bytes == 0))
            );
        });
    }
}
