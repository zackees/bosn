use std::io::Write as _;

use super::*;

#[test]
fn relocated_writer_commits_only_to_stable_database_and_preserves_identity() {
    let directory = fs::TemporaryDirectory::new().unwrap();
    let source = directory.path().join("source.sqlite3");
    let stable = directory.path().join("stable.sqlite3");
    let owner = "11111111-2222-4333-8444-555555555555";
    let mut registry = Registry::create_writer(&source, owner).unwrap();
    registry.relocate_writer(&stable).unwrap();
    registry.publish_authority(&source, &stable).unwrap();
    assert_eq!(Registry::resolve_authority(&source).unwrap(), stable);
    assert_eq!(registry.registry_id().unwrap(), owner);
    assert!(Registry::open_writer(&stable).is_err());
    assert!(
        Registry::open_writer(&source).is_err(),
        "historical writer fence lost"
    );
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .append_event(1.0, "relocation.fixture", "stable commit")
        .unwrap();
    transaction.commit().unwrap();
    let limits = QueryLimits {
        max_rows: 2,
        max_bytes: 1024,
    };
    assert_eq!(
        registry
            .connection
            .query(
                "SELECT detail FROM events WHERE kind='relocation.fixture'",
                &[],
                limits
            )
            .unwrap()
            .len(),
        1
    );
    let old =
        Connection::open_read_only_with_busy_timeout(&source, std::time::Duration::from_secs(5))
            .unwrap();
    assert!(
        old.query(
            "SELECT detail FROM events WHERE kind='relocation.fixture'",
            &[],
            limits
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        Registry::open_read_only(&source)
            .unwrap()
            .connection
            .query(
                "SELECT detail FROM events WHERE kind='relocation.fixture'",
                &[],
                limits
            )
            .unwrap()
            .len(),
        1
    );
    assert!(registry.relocate_writer(&source).is_err());
    drop(registry);
    let legacy_writer = acquire_writer_lock(&source).unwrap();
    assert!(Registry::open_retention_snapshot(&source).is_err());
    drop(legacy_writer);
    let snapshot = Registry::open_retention_snapshot(&source).unwrap();
    assert!(acquire_writer_lock(&source).is_err());
    assert!(Registry::open_writer(&stable).is_err());
    drop(snapshot);
    std::fs::rename(&source, directory.path().join("historical.sqlite3")).unwrap();
    assert_eq!(Registry::resolve_authority(&source).unwrap(), stable);
    assert_eq!(
        Registry::open_writer(&source)
            .unwrap()
            .registry_id()
            .unwrap(),
        owner
    );
    assert!(Registry::create_writer(&source, owner).is_err());
    std::fs::write(source.with_extension("authority.json"), b"{}").unwrap();
    assert!(Registry::resolve_authority(&source).is_err());
    assert_eq!(
        Registry::open_writer(&stable)
            .unwrap()
            .registry_id()
            .unwrap(),
        owner
    );
}

#[test]
fn ownership_export_marks_transactions_dirty_until_commits_are_published() {
    let directory = fs::TemporaryDirectory::new().unwrap();
    let backup = directory.path().join("backup");
    std::fs::create_dir(&backup).unwrap();
    let mut registry = Registry::create_writer(
        directory.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    registry.enable_ownership_backup(backup.clone()).unwrap();
    let marker = backup.join("ownership-status");
    assert_eq!(std::fs::read(&marker).unwrap(), b"clean\n");
    let mut transaction = registry.begin_immediate().unwrap();
    assert_eq!(std::fs::read(&marker).unwrap(), b"dirty\n");
    transaction
        .append_event(2.0, "ownership.marker.fixture", "new")
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(std::fs::read(&marker).unwrap(), b"dirty\n");
    registry.publish_ownership_backup().unwrap();
    assert_eq!(std::fs::read(&marker).unwrap(), b"clean\n");
    Registry::open_read_only(backup.join("registry.sqlite3")).unwrap();
}

#[test]
fn ownership_backup_includes_committed_wal_and_refuses_replacement() {
    let directory = fs::TemporaryDirectory::new().unwrap();
    let source = directory.path().join("registry.sqlite3");
    let snapshot = directory.path().join("ownership.sqlite3");
    let owner = "11111111-2222-4333-8444-555555555555";
    let mut registry = Registry::create_writer(&source, owner).unwrap();
    registry
        .connection
        .query(
            "PRAGMA journal_mode=WAL",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )
        .unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .append_event(1.0, "ownership.snapshot.fixture", "committed")
        .unwrap();
    transaction.commit().unwrap();
    assert!(source.with_extension("sqlite3-wal").exists());
    registry.backup_ownership(&snapshot).unwrap();
    let backup = Registry::open_read_only(&snapshot).unwrap();
    assert_eq!(backup.registry_id().unwrap(), owner);
    let rows = backup
        .connection
        .query(
            "SELECT detail FROM events WHERE kind='ownership.snapshot.fixture'",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 1024,
            },
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(registry.backup_ownership(&snapshot).is_err());
}

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
