//! Shared live-Docker helpers for the managed-retention proofs (#545): foreign decoys the pass
//! must never touch, labelled throwaway objects, and zero-age passes through the production entry.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bosn_core::retention::RetentionPolicy;
use bosn_engine::DockerEngine;
use bosn_service::managed_retention::{MaintenanceOutcome, maintenance_pass_with};

use super::setup_docker::{PINNED_ALPINE, docker_capture};

/// Every age gate at zero, so a test need not wait out the production gates. Ownership,
/// liveness, pin and lease gates are unchanged.
pub(crate) const ZERO_AGE: RetentionPolicy = RetentionPolicy {
    container_ttl: Duration::ZERO,
    volume_ttl: Duration::ZERO,
    image_ttl: Duration::ZERO,
    max_bytes: None,
};

/// The production maintenance pass (idle stop, then retention) with every age gate at zero.
pub(crate) fn zero_age_pass(engine: &DockerEngine, state: &Path) -> MaintenanceOutcome {
    let outcome = maintenance_pass_with(engine, state, ZERO_AGE);
    eprintln!(
        "pass: stopped_idle={:?} {:?}",
        outcome.stopped_idle, outcome.retention.summary
    );
    assert_eq!(
        outcome.retention.summary.refused, None,
        "the engine read was complete"
    );
    outcome
}

/// The complete canonical label set naming `registry`, as Bosn's own objects carry it.
pub(crate) fn canonical_labels(registry: &str, kind: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (bosn_core::LABEL_REGISTRY, registry.to_owned()),
        (bosn_core::LABEL_KIND, kind.to_owned()),
        (bosn_core::LABEL_STACK, "bosn545-e2e".to_owned()),
        (bosn_core::LABEL_GENERATION, "1".to_owned()),
        (bosn_core::LABEL_SCOPE, "stack".to_owned()),
        (bosn_core::LABEL_WORKSPACE, "/bosn545-e2e".to_owned()),
        (bosn_core::LABEL_CREATED, "1".to_owned()),
    ])
}

/// Create a volume named `name` with `labels`; removed by the caller's [`Throwaway`].
pub(crate) fn labelled_volume(
    engine: &DockerEngine,
    name: &str,
    labels: &BTreeMap<&'static str, String>,
) {
    let mut args = vec!["volume".to_owned(), "create".to_owned()];
    for (key, value) in labels {
        args.push("--label".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push(name.to_owned());
    assert!(docker_capture(engine, args).ok(), "create volume {name}");
}

pub(crate) fn volume_exists(engine: &DockerEngine, name: &str) -> bool {
    docker_capture(engine, ["volume", "inspect", name]).ok()
}

pub(crate) fn container_exists(engine: &DockerEngine, name: &str) -> bool {
    docker_capture(engine, ["container", "inspect", name]).ok()
}

/// Names Docker lists for `kind` (`container` or `volume`) under a label filter.
pub(crate) fn listed(engine: &DockerEngine, kind: &str, filter: &str) -> Vec<String> {
    let format = if kind == "container" {
        "{{.Names}}"
    } else {
        "{{.Name}}"
    };
    let mut args = vec![kind, "ls", "--filter", filter, "--format", format];
    if kind == "container" {
        args.insert(2, "--all");
    }
    let result = docker_capture(engine, args);
    assert!(result.ok(), "docker {kind} ls failed");
    String::from_utf8_lossy(&result.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Exact objects this test created, removed on drop whatever happened.
pub(crate) struct Throwaway {
    pub(crate) engine: DockerEngine,
    pub(crate) containers: Vec<String>,
    pub(crate) volumes: Vec<String>,
    pub(crate) images: Vec<String>,
}

impl Throwaway {
    pub(crate) fn new(engine: &DockerEngine) -> Self {
        Self {
            engine: engine.clone(),
            containers: Vec::new(),
            volumes: Vec::new(),
            images: Vec::new(),
        }
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        for name in &self.containers {
            docker_capture(&self.engine, ["container", "rm", "--force", name.as_str()]);
        }
        for name in &self.volumes {
            docker_capture(&self.engine, ["volume", "rm", name.as_str()]);
        }
        for name in &self.images {
            docker_capture(&self.engine, ["image", "rm", name.as_str()]);
        }
    }
}

/// Objects that are not this registry's: a complete-label volume and stopped container naming
/// another registry, and an unlabelled volume. Every pass must leave all three.
pub(crate) struct Decoys {
    volume_foreign: String,
    volume_unlabelled: String,
    container_foreign: String,
    _cleanup: Throwaway,
}

impl Decoys {
    pub(crate) fn plant(engine: &DockerEngine, unique: &str) -> Self {
        let foreign = "00000000-0000-4000-8000-0000000b0545";
        let volume_foreign = format!("bosn545-decoy-foreign-{unique}");
        let volume_unlabelled = format!("bosn545-decoy-unlabelled-{unique}");
        let container_foreign = format!("bosn545-decoy-container-{unique}");
        let mut cleanup = Throwaway::new(engine);
        cleanup.volumes.push(volume_foreign.clone());
        cleanup.volumes.push(volume_unlabelled.clone());
        cleanup.containers.push(container_foreign.clone());
        labelled_volume(
            engine,
            &volume_foreign,
            &canonical_labels(foreign, "volume"),
        );
        labelled_volume(engine, &volume_unlabelled, &BTreeMap::new());
        let mut args = vec!["container".to_owned(), "create".to_owned()];
        for (key, value) in canonical_labels(foreign, "container") {
            args.push("--label".to_owned());
            args.push(format!("{key}={value}"));
        }
        args.extend([
            "--name".to_owned(),
            container_foreign.clone(),
            PINNED_ALPINE.to_owned(),
            "true".to_owned(),
        ]);
        assert!(docker_capture(engine, args).ok(), "create decoy container");
        Self {
            volume_foreign,
            volume_unlabelled,
            container_foreign,
            _cleanup: cleanup,
        }
    }

    pub(crate) fn assert_intact(&self, engine: &DockerEngine) {
        assert!(
            volume_exists(engine, &self.volume_foreign),
            "foreign volume kept"
        );
        assert!(
            volume_exists(engine, &self.volume_unlabelled),
            "unlabelled volume kept"
        );
        assert!(
            container_exists(engine, &self.container_foreign),
            "foreign container kept"
        );
    }
}

/// The machine state root must be throwaway: these tests enroll in, and reclaim through, the
/// machine catalog under it.
pub(crate) fn throwaway_machine_root() -> std::path::PathBuf {
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .expect("set XDG_STATE_HOME to a temporary directory");
    let real = std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".local/state"))
        .expect("HOME");
    assert_ne!(
        root, real,
        "XDG_STATE_HOME is the real machine state root; use a throwaway one"
    );
    root.join("bosn")
}
