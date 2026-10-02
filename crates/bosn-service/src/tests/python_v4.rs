//! Offline Python v4 registry reconciliation.

use super::*;

struct FakePythonV4Engine {
    observed: Option<PythonV4ObservedResource>,
}
impl PythonV4ReconcileExecutor for FakePythonV4Engine {
    fn inspect(
        &self,
        _kind: ResourceKind,
        _name: &str,
    ) -> Result<Option<PythonV4ObservedResource>, String> {
        Ok(self.observed.clone())
    }
}

fn gated_v4_reconciliation_registry(
    with_session: bool,
) -> (
    kernal_api::platform::fs::TemporaryDirectory,
    PathBuf,
    PythonV4ObservedResource,
) {
    let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = directory.path().join("registry.sqlite3");
    let registry_id = "11111111-2222-4333-8444-555555555555";
    let resource = Resource {
        id: "legacy-volume".into(),
        kind: ResourceKind::Volume,
        name: "legacy-volume-name".into(),
        stack: "legacy".into(),
        generation: "sha256:legacy".into(),
        scope: Scope::Stack,
        workspace: "/legacy/work".into(),
        created_at: 1.0,
        last_used: 1.0,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    };
    let mut registry = Registry::create_writer(&path, registry_id).unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    transaction.put_resource(&resource).unwrap();
    transaction
        .put_resource_use(&ResourceUse {
            resource_id: resource.id.clone(),
            workspace: resource.workspace.clone(),
            stack: resource.stack.clone(),
            generation: resource.generation.clone(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    transaction.commit().unwrap();
    drop(registry);
    let connection = kernal_api::sqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO meta(key,value) VALUES('migration.reconciliation_required','true')",
            &[],
        )
        .unwrap();
    if with_session {
        connection.execute("INSERT INTO execution_sessions VALUES ('old','legacy-volume','docker',2147483647,NULL,'[]')", &[]).unwrap();
    }
    let labels = ResourceLabels::new(
        registry_id,
        resource.kind,
        &resource.stack,
        &resource.generation,
        resource.scope,
        &resource.workspace,
        "legacy-created",
        Some(resource.retention),
    )
    .unwrap()
    .to_map()
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect();
    (
        directory,
        path,
        PythonV4ObservedResource {
            engine_id: "legacy-volume-name".into(),
            name: resource.name,
            labels,
        },
    )
}

#[test]
fn offline_python_v4_reconciliation_repeats_fake_engine_proof_then_clears_gate_atomically() {
    let (_directory, path, observed) = gated_v4_reconciliation_registry(false);
    let before_preview = std::fs::read(&path).unwrap();
    let preview = preview_python_v4_reconciliation(
        &path,
        &FakePythonV4Engine {
            observed: Some(observed.clone()),
        },
    )
    .unwrap();
    assert_eq!(preview.verified, ["legacy-volume"]);
    assert!(preview.ready());
    assert_eq!(std::fs::read(&path).unwrap(), before_preview);
    let applied = apply_python_v4_reconciliation(
        &path,
        &FakePythonV4Engine {
            observed: Some(observed),
        },
        42.0,
    )
    .unwrap();
    assert!(applied.ready());
    let registry = Registry::open_writer(&path).unwrap();
    assert_eq!(
        registry.meta("migration.reconciliation_required").unwrap(),
        None
    );
    assert!(
        registry
            .events(0, 16)
            .unwrap()
            .items
            .iter()
            .any(|event| event.kind == "migration.reconcile.verified")
    );
}

#[test]
fn offline_python_v4_reconciliation_refuses_missing_engine_and_legacy_session_without_clearing_gate()
 {
    let (_directory, path, _observed) = gated_v4_reconciliation_registry(true);
    let report =
        apply_python_v4_reconciliation(&path, &FakePythonV4Engine { observed: None }, 42.0)
            .unwrap();
    assert!(
        report
            .refusals
            .iter()
            .any(|reason| reason == "legacy_sessions_present")
    );
    assert!(
        report
            .refusals
            .iter()
            .any(|reason| reason == "missing:legacy-volume")
    );
    assert!(matches!(
        Registry::open_writer(&path),
        Err(bosn_registry::Error::ReconciliationRequired)
    ));
}

#[test]
fn offline_python_v4_reconciliation_refuses_foreign_labels_without_name_adoption() {
    let (_directory, path, mut observed) = gated_v4_reconciliation_registry(false);
    observed
        .labels
        .insert("com.zackees.bosn.registry".into(), "foreign".into());
    let report = preview_python_v4_reconciliation(
        &path,
        &FakePythonV4Engine {
            observed: Some(observed),
        },
    )
    .unwrap();
    assert_eq!(
        report.refusals,
        ["foreign_or_ambiguous_labels:legacy-volume"]
    );
    assert!(matches!(
        Registry::open_writer(&path),
        Err(bosn_registry::Error::ReconciliationRequired)
    ));
}
