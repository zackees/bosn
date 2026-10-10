use std::io::Write as _;

use super::*;

#[test]
fn reconciliation_preview_rechecks_gate_and_schema_after_writer_race() {
    for (mutation, gate_changed) in [
        (
            "DELETE FROM meta WHERE key='migration.reconciliation_required'",
            true,
        ),
        (
            "UPDATE meta SET value='7' WHERE key='schema_version'",
            false,
        ),
    ] {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let path = directory.path().join("registry.sqlite3");
        drop(Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap());
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO meta VALUES ('migration.reconciliation_required','true')",
                &[],
            )
            .unwrap();
        drop(connection);

        // A writer commits after the first read-only probe but before the
        // preview owns the writer fence. It must observe the later state.
        let result = Registry::open_reconciliation_preview_inner(
            &path,
            Some(&|| {
                let connection = Connection::open(&path).unwrap();
                connection.execute(mutation, &[]).unwrap();
            }),
        );
        if gate_changed {
            assert!(matches!(result, Err(Error::ReconciliationNotRequired)));
        } else {
            assert!(matches!(result, Err(Error::UnsupportedSchema(7))));
        }
        acquire_writer_lock(&path).unwrap();
    }
}

// This runs before the read-only SQLite open, so the rename is portable:
// Windows does not permit replacing a file with an already-open handle.
#[test]
fn v4_import_refuses_source_replacement_after_identity_capture() {
    let directory = fs::TemporaryDirectory::new().unwrap();
    let source = directory.path().join("registry.sqlite3");
    let replacement = directory.path().join("replacement.sqlite3");
    let destination = directory.path().join("destination.sqlite3");
    let marker = directory.path().join(CUTOVER_MARKER);
    let registry_id = "11111111-2222-4333-8444-555555555555";

    let mut marker_file = fs::create_private_file(&marker).unwrap();
    marker_file
        .write_all(format!(r#"{{"protocol":1,"registry_id":"{registry_id}"}}"#).as_bytes())
        .unwrap();
    marker_file.sync_all().unwrap();
    drop(marker_file);

    for path in [&source, &replacement] {
        drop(fs::create_private_file(path).unwrap());
        let connection = Connection::open(path).unwrap();
        connection
            .execute("CREATE TABLE marker(value TEXT)", &[])
            .unwrap();
    }

    let error = import_python_v4_inner(
        directory.path(),
        &source,
        &destination,
        Some(&|| std::fs::rename(&replacement, &source).unwrap()),
    )
    .unwrap_err();

    assert!(matches!(error, Error::ReplacedPath(path) if path == source));
    assert!(!destination.exists());
}

#[test]
fn preserving_upsert_never_removes_a_pin_but_plain_upsert_does() {
    let directory = fs::TemporaryDirectory::new().unwrap();
    let path = directory.path().join("registry.sqlite3");
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let row = |name: &str, retention| Resource {
        id: format!("setup-container:{name}"),
        kind: ResourceKind::Container,
        name: name.into(),
        stack: "setup".into(),
        generation: "sha256:a".into(),
        scope: Scope::Machine,
        workspace: "/w".into(),
        created_at: 1.0,
        last_used: 1.0,
        state: ResourceState::Active,
        retention,
    };
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&row("pinned", Retention::Pinned))
        .unwrap();
    transaction
        .put_resource(&row("warm", Retention::Warm))
        .unwrap();
    transaction
        .put_resource_preserving_pin(&row("pinned", Retention::Warm))
        .unwrap();
    transaction
        .put_resource_preserving_pin(&row("warm", Retention::Warm))
        .unwrap();
    transaction
        .put_resource_preserving_pin(&row("new", Retention::Warm))
        .unwrap();
    transaction.commit().unwrap();
    let retention = |registry: &Registry, name| {
        registry
            .resource_by_kind_name(ResourceKind::Container, name)
            .unwrap()
            .unwrap()
            .retention
    };
    assert_eq!(retention(&registry, "pinned"), Retention::Pinned);
    assert_eq!(retention(&registry, "warm"), Retention::Warm);
    assert_eq!(retention(&registry, "new"), Retention::Warm);
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&row("pinned", Retention::Warm))
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(retention(&registry, "pinned"), Retention::Warm);
}
