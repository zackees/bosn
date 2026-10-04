//! Two independent backends against the same private machine volume.
use super::*;
use super::{
    cache_usage::{helper, journal},
    maintenance_helper::helper_create_args,
};
use crate::ci::lifecycle::tests::{OWNER, with_registry};
use bosn_registry::cache_helper::{CacheHelperIntent, CacheHelperRole};

#[test]
#[ignore = "requires isolated private Docker with verified shared act2.7 cohort"]
fn competing_maintenance_is_refused_and_container_death_releases_the_lease() {
    assert!(
        std::env::var("DOCKER_HOST")
            .unwrap()
            .contains("bosn-456-live-v2-engine")
    );
    with_registry(|registry, _| async move {
        let holder = DockerActBackend::default();
        let competitor = DockerActBackend::default();
        let intent = CacheHelperIntent {
            registry_id: OWNER.into(),
            nonce: crate::ci::new_uuid().await.unwrap(),
            image: engine_image(),
            volume: CACHE_VOLUME.into(),
            created_at: crate::ci::lifecycle::now_seconds(),
            role: Some(CacheHelperRole::MaintenanceV1),
        };
        let identity = helper::Identity::from_intent(&intent).unwrap();
        let tracker = journal::Tracker::new(&registry, &intent.nonce);
        let _active = journal::ActiveHelper::claim(&holder, &intent.nonce);
        tracker.begin(intent.clone()).await.unwrap();
        let mut args = helper_create_args(&identity);
        let entrypoint = args.iter().position(|v| v == "--entrypoint").unwrap();
        args[entrypoint + 1] = "sh".into();
        *args.last_mut().unwrap() = "-ec".into();
        args.push("mkdir -p /bosn/cache/actcache; exec 6>>/bosn/cache/actcache/.bosn-maintenance-v1.lock; flock -x -n 6; touch /var/lib/docker/ready; sleep 300".into());
        let id = holder
            .checked("lease holder create", args, CONTROL_DEADLINE)
            .await
            .unwrap();
        tracker.register(&id).await.unwrap();
        let result: Result<(), String> = async {
            holder.checked("lease holder start", owned(&["start", &id]), CONTROL_DEADLINE).await?;
            holder.checked("lease holder ready", DockerActBackend::exec(&id,
                "for i in 1 2 3 4 5 6 7 8 9 10; do test -f /var/lib/docker/ready && exit 0; sleep 0.1; done; exit 1"
            ), CONTROL_DEADLINE).await?;
            let policy: crate::ci::cache_policy::CachePolicy = toml::from_str("repository_max_bytes=104857600\naggregate_max_bytes=209715200\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=60\n").map_err(|e| e.to_string())?;
            let refused = competitor.maintain_cache_with_helper(&registry, OWNER, policy).await?;
            refused.cleanup?;
            let error = match refused.outcome {
                Err(error) => error,
                Ok(_) => return Err("competing maintenance unexpectedly ran".into()),
            };
            if !error.contains("machine cache maintenance busy") {
                return Err(format!("contention outcome missing: {error}"));
            }
            Ok(())
        }.await;
        let cleanup = holder
            .recover_measurement(&identity, CACHE_VOLUME, Some(&tracker))
            .await;
        cleanup.unwrap();
        result.unwrap();
        holder.confirm_measurement_absent(&id).await.unwrap();
        let policy = toml::from_str("repository_max_bytes=104857600\naggregate_max_bytes=209715200\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=60\n").unwrap();
        let resumed = competitor
            .maintain_cache_with_helper(&registry, OWNER, policy)
            .await
            .unwrap();
        resumed.cleanup.unwrap();
        resumed.outcome.unwrap().require_complete().unwrap();
        competitor
            .verify_measured_volume(CACHE_VOLUME)
            .await
            .unwrap();
    });
}
