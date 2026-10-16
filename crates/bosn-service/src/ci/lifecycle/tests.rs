use super::*;
use crate::{Registry, registry_actor};
use bosn_registry::act::{ActEngineObservation, ActEngineRemovalProof};
use kernal_api::async_engine::{CancellationSource, RuntimeBuilder};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

mod fake;
mod fake_machine;
mod shared;
pub use fake::*;

#[derive(Default)]
pub struct Collect {
    pub lines: Vec<EngineLine>,
    pub listing: Option<String>,
    pub notes: Vec<String>,
}
impl EngineObserver for Collect {
    fn note(&mut self, text: &str) {
        self.notes.push(text.into());
    }
    fn declared(&mut self, listing: &str) {
        self.listing = Some(listing.into());
    }
    fn line(&mut self, line: EngineLine) {
        self.lines.push(line);
    }
}

pub fn intent(run: &str) -> ActEngineIntent {
    ActEngineIntent {
        run_id: run.into(),
        workspace: "/private/source".into(),
        candidate_sha: "a".repeat(40),
        payload_sha256: "b".repeat(64),
        snapshot_sha256: "c".repeat(64),
        act_version: "0.2.88".into(),
        act_image_digest: format!("sha256:{}", "d".repeat(64)),
        engine_image_digest: crate::act_engine::ENGINE_MANIFEST.into(),
        runner_image_digest: format!("sha256:{}", "f".repeat(64)),
        created_at: 1.0,
        spare: false,
        creation_profile: Some(
            crate::act_engine::creation_profile_with_cache(
                super::super::limits::size_engine(FAKE_HOST, Default::default()).unwrap(),
                Some(test_cache().mount()),
            )
            .unwrap(),
        ),
    }
}
pub fn plan(run: &str, deadline: Duration) -> EnginePlan {
    EnginePlan {
        intent: intent(run),
        act: super::super::engine::act_artifact("x86_64").unwrap(),
        source: "/nonexistent".into(),
        event: "/nonexistent".into(),
        invocation: ActInvocation {
            event: "push".into(),
            workflow: ".github/workflows/ci.yml".into(),
            workflow_overlaid: false,
            job: None,
            cache_route: crate::ci::cache_cohort::CacheRoute::Legacy(
                crate::ci::cache_cohort::Namespace::parse(&"0".repeat(16)).unwrap(),
            ),
            secrets: Default::default(),
            params: Default::default(),
            scope: None,
        },
        cache: test_cache(),
        deadline: async_engine::Deadline::after(deadline),
        shared: shared_plan(run),
        github: None,
    }
}
/// The shared engine a plan for `run` would make: the run's engine shape,
/// bound to the daemon instead of the run.
pub fn shared_plan(run: &str) -> super::super::spare::SparePlan {
    let mut intent = intent(run);
    let unbound = bosn_registry::act::ActEngineBinding::spare(&intent.run_id, "/state");
    intent.workspace = unbound.workspace;
    intent.candidate_sha = unbound.candidate_sha;
    intent.payload_sha256 = unbound.payload_sha256;
    intent.snapshot_sha256 = unbound.snapshot_sha256;
    intent.spare = true;
    super::super::spare::SparePlan {
        intent,
        act: super::super::engine::act_artifact("x86_64").unwrap(),
        cache: test_cache(),
    }
}
pub fn test_cache() -> CacheVolume {
    CacheVolume {
        name: "bosn-ci-cache-test".into(),
        labels: BTreeMap::new(),
    }
}
pub fn run_id(n: u32) -> String {
    format!("aaaaaaaa-bbbb-4ccc-8ddd-{n:012x}")
}

pub const OWNER: &str = "11111111-2222-4333-8444-555555555555";

/// One daemon's sole registry writer over a registry file.
pub struct Daemon {
    pub registry: RegistryActor,
    task: async_engine::Task<()>,
}
impl Daemon {
    pub fn open(path: &std::path::Path) -> Self {
        let registry = if path.exists() {
            Registry::open_writer(path).unwrap()
        } else {
            Registry::create_writer(path, OWNER).unwrap()
        };
        let (sender, receiver) = async_engine::channel(16);
        let task = async_engine::launch(registry_actor(registry, receiver, None));
        Self {
            registry: RegistryActor { sender },
            task,
        }
    }
    pub async fn stop(self) {
        self.registry.stop().await;
        let _ = self.task.await;
    }
}

/// Run `body` with a live registry actor over a temporary registry.
pub fn with_registry<F, Fut>(body: F)
where
    F: FnOnce(RegistryActor, std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    RuntimeBuilder::multi_thread()
        // Each parallel fixture owns its pool; retain concurrent execution
        // without multiplying the container's CPU-sized pool per test.
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let daemon = Daemon::open(&path);
            body(daemon.registry.clone(), dir.path().to_path_buf()).await;
            daemon.stop().await;
        });
}

mod storage;
