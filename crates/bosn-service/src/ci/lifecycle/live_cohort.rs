//! Explicit cohort route through the real owned-engine lifecycle.
use super::*;
use crate::ci::{
    cache_cohort::{CacheRoute, Namespace},
    cache_policy::CachePolicy,
    engine::{DockerActBackend, act_artifact},
    limits::{EngineConfig, size_engine},
};

fn workflow(key: &str, restore_only: bool) -> String {
    let command = if restore_only {
        "test \"$(cat cached/x)\" = kept && echo FRESH_ENGINE_RESTORED"
    } else {
        "mkdir -p cached && echo kept > cached/x"
    };
    format!(
        "on: [push]\njobs:\n  save:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/cache@v4\n        with:\n          path: cached\n          key: {key}\n      - run: {command}\n  restore:\n    needs: save\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/cache@v4\n        with:\n          path: cached\n          key: {key}\n      - run: test \"$(cat cached/x)\" = kept && echo COHORT_JOB_RESTORED\n"
    )
}

fn isolated_workflow(key: &str) -> String {
    r#"on: [push]
jobs:
  isolate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/cache@v4
        id: cache
        with:
          path: cached
          key: CACHE_KEY
      - run: test "$CACHE_HIT" != true && test ! -e cached/x && echo COHORT_REPOSITORY_ISOLATED
        env:
          CACHE_HIT: ${{ steps.cache.outputs.cache-hit }}
"#
    .replace("CACHE_KEY", key)
}

#[test]
#[ignore = "requires isolated bosn-456-live-v2 Docker engine and network access"]
fn cohort_route_restores_across_jobs_and_successive_owned_engines() {
    assert!(
        std::env::var("DOCKER_HOST")
            .unwrap()
            .contains("bosn-456-live-v2-engine")
    );
    with_registry(|registry, directory| async move {
        let backend = DockerActBackend::default();
        backend.ensure_engine_image().await.unwrap();
        let act = act_artifact("x86_64").unwrap();
        let cache = CacheVolume::machine(OWNER, now_seconds()).unwrap();
        let limits = size_engine(
            backend.host_resources().await.unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let profile =
            crate::act_engine::creation_profile_with_cache(limits, Some(cache.mount())).unwrap();
        let nonce = crate::ci::new_uuid().await.unwrap();
        let namespace = Namespace::parse(&nonce.replace('-', "")[..16]).unwrap();
        let other = crate::ci::new_uuid().await.unwrap();
        let other = Namespace::parse(&other.replace('-', "")[..16]).unwrap();
        let key = format!("cohort-proof-{nonce}");
        let policy: CachePolicy = toml::from_str("repository_max_bytes=104857600\naggregate_max_bytes=209715200\nmax_age_secs=2592000\nunused_age_secs=604800\nmaintenance_interval_secs=60\n").unwrap();
        let source = directory.join("source");
        std::fs::create_dir(&source).unwrap();
        let event = directory.join("event.json");
        std::fs::write(&event, "{}").unwrap();
        let mut ids = Vec::new();
        for mode in [0, 1, 2] {
            let contents = if mode == 2 {
                isolated_workflow(&key)
            } else {
                workflow(&key, mode == 1)
            };
            std::fs::write(source.join("workflow.yaml"), contents).unwrap();
            let mut intent = intent(&crate::ci::new_uuid().await.unwrap());
            intent.workspace = source.to_str().unwrap().into();
            intent.act_version = crate::ci::engine::ACT_VERSION.into();
            intent.act_image_digest = format!("sha256:{}", act.sha256);
            intent.runner_image_digest = crate::ci::pins::runner_manifest().into();
            intent.creation_profile = Some(profile.clone());
            intent.created_at = now_seconds();
            let plan = EnginePlan {
                intent,
                act,
                source: source.clone(),
                event: event.clone(),
                cache: cache.clone(),
                deadline: async_engine::Deadline::after(Duration::from_secs(600)),
                spare: None,
                invocation: ActInvocation {
                    event: "push".into(),
                    workflow: "workflow.yaml".into(),
                    workflow_overlaid: false,
                    job: None,
                    cache_route: CacheRoute::Cohort {
                        namespace: if mode == 2 {
                            other.clone()
                        } else {
                            namespace.clone()
                        },
                        policy,
                    },
                    secrets: Default::default(),
                    params: Default::default(),
                    scope: None,
                },
            };
            let mut observer = Collect::default();
            let cancel = CancellationSource::new();
            let report =
                run_on_engine(&registry, &backend, &plan, &cancel.token(), &mut observer).await;
            let logs = format!("{:?}\n{:?}", observer.notes, observer.lines);
            assert_eq!(report.cleanup, CleanupEnd::Removed, "{report:?}\n{logs}");
            assert_eq!(
                report.execution,
                ExecutionEnd::Exited(0),
                "{report:?}\n{logs}"
            );
            if mode == 2 {
                assert!(logs.contains("COHORT_REPOSITORY_ISOLATED"), "{logs}");
            } else {
                assert!(logs.contains("COHORT_JOB_RESTORED"), "{logs}");
                if mode == 1 {
                    assert!(logs.contains("FRESH_ENGINE_RESTORED"), "{logs}");
                }
            }
            ids.push(report.engine_id.unwrap());
        }
        assert_ne!(
            ids[0], ids[1],
            "warm restore must use distinct fresh engines"
        );
        assert_ne!(ids[2], ids[0]);
        assert_ne!(ids[2], ids[1]);
    });
}
