//! One-way import of Python bosn v4 registries.

use crate::common::*;

#[test]
fn v4_import_refuses_missing_cutover_marker_without_creating_destination() {
    let (directory, source) = database_path();
    let destination = directory.path().join("destination.sqlite3");
    let reserved = kernal_api::platform::fs::create_private_file(&source).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    connection
        .execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .unwrap();
    connection.execute("INSERT INTO meta VALUES ('schema_version','4'),('registry_id','11111111-2222-4333-8444-555555555555')", &[]).unwrap();
    assert!(import_python_v4(directory.path(), &source, &destination).is_err());
    assert!(!destination.exists());
}

#[test]
fn v4_import_refuses_a_source_outside_its_state_directory_after_taking_guard() {
    let (directory, source) = database_path();
    let wrong_source = directory.path().join("other.sqlite3");
    let destination = directory.path().join("destination.sqlite3");
    drop(kernal_api::platform::fs::create_private_file(&source).unwrap());
    drop(kernal_api::platform::fs::create_private_file(&wrong_source).unwrap());

    let guard = acquire_legacy_migration_guard(directory.path()).unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &wrong_source, &destination),
        Err(Error::MigrationGuardHeld(_))
    ));
    drop(guard);
    assert!(matches!(
        import_python_v4(directory.path(), &wrong_source, &destination),
        Err(Error::InvalidSchema)
    ));
    assert!(!destination.exists());
}

#[test]
fn v4_import_refuses_malformed_marker_before_touching_source_or_destination() {
    let (directory, source) = database_path();
    let destination = directory.path().join("destination.sqlite3");
    let marker = directory.path().join("rust-cutover-v1.json");
    let mut marker_file = kernal_api::platform::fs::create_private_file(&marker).unwrap();
    marker_file.write_all(b"not-json").unwrap();
    marker_file.sync_all().unwrap();
    drop(marker_file);
    let reserved = kernal_api::platform::fs::create_private_file(&source).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    connection
        .execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .unwrap();
    connection.execute("INSERT INTO meta VALUES ('schema_version','4'),('registry_id','11111111-2222-4333-8444-555555555555')", &[]).unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::InvalidCutoverMarker)
    ));
    assert!(!destination.exists());
    assert_eq!(
        connection
            .query(
                "SELECT value FROM meta WHERE key='schema_version'",
                &[],
                Default::default()
            )
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Text("4".into()))
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn v4_import_preserves_retired_rows_events_and_sets_reconciliation_gate() {
    let (directory, source) = database_path();
    let destination = directory.path().join("destination.sqlite3");
    let registry_id = "11111111-2222-4333-8444-555555555555";
    let marker = directory.path().join("rust-cutover-v1.json");
    let mut marker_file = kernal_api::platform::fs::create_private_file(&marker).unwrap();
    marker_file
        .write_all(format!(r#"{{"protocol":1,"registry_id":"{registry_id}"}}"#).as_bytes())
        .unwrap();
    marker_file.sync_all().unwrap();
    drop(marker_file);
    let reserved = kernal_api::platform::fs::create_private_file(&source).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    for statement in Registry::schema_sql()
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        connection.execute(statement, &[]).unwrap();
    }
    connection.execute("INSERT INTO meta VALUES ('schema_version','4'),('registry_id','11111111-2222-4333-8444-555555555555'),('legacy.extra','retained')", &[]).unwrap();
    connection.execute("INSERT INTO resources VALUES ('retired','container','old','stack','g','stack','/work',1,2,'retired','pinned')", &[]).unwrap();
    connection
        .execute(
            "INSERT INTO resource_uses VALUES ('retired','/work','stack','g',2,'retired')",
            &[],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO leases VALUES ('lease','retired',2147483647,NULL,1,2,30)",
            &[],
        )
        .unwrap();
    connection.execute("INSERT INTO execution_sessions VALUES ('session','container','docker',2147483647,NULL,'[\"lease\"]')", &[]).unwrap();
    connection.execute("INSERT INTO volume_creation_intents VALUES ('intent','{}','stack','g','stack','/work')", &[]).unwrap();
    connection
        .execute(
            "INSERT INTO generations VALUES ('/work','stack','g',1,NULL)",
            &[],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events(id,at,kind,detail) VALUES (42,2,'event','detail')",
            &[],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events(id,at,kind,detail) VALUES (99,3,'deleted','')",
            &[],
        )
        .unwrap();
    connection
        .execute("DELETE FROM events WHERE id=99", &[])
        .unwrap();
    let report = import_python_v4(directory.path(), &source, &destination).unwrap();
    assert!(report.reconciliation_required);
    assert_eq!(report.table_counts["resources"], 1);
    assert_eq!(report.table_counts["leases"], 1);
    assert_eq!(report.table_counts["execution_sessions"], 1);
    assert!(matches!(
        Registry::open_writer(&destination),
        Err(Error::ReconciliationRequired)
    ));
    let destination_connection =
        kernal_api::sqlite::Connection::open_read_only(&destination).unwrap();
    assert_eq!(
        destination_connection
            .query(
                "SELECT state,retention FROM resources",
                &[],
                Default::default()
            )
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Text("retired".into()))
    );
    assert_eq!(
        destination_connection
            .query("SELECT id FROM events", &[], Default::default())
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Integer(42))
    );
    assert_eq!(
        destination_connection
            .query(
                "SELECT value FROM meta WHERE key='legacy.extra'",
                &[],
                Default::default()
            )
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Text("retained".into()))
    );
    assert_eq!(
        destination_connection
            .query(
                "SELECT seq FROM sqlite_sequence WHERE name='events'",
                &[],
                Default::default()
            )
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Integer(99))
    );
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::ImportTargetExists(_))
    ));
    connection.execute("DELETE FROM events", &[]).unwrap();
    let empty_events_destination = directory.path().join("empty-events.sqlite3");
    import_python_v4(directory.path(), &source, &empty_events_destination).unwrap();
    let empty_events =
        kernal_api::sqlite::Connection::open_read_only(&empty_events_destination).unwrap();
    assert!(
        empty_events
            .query("SELECT id FROM events", &[], Default::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        empty_events
            .query(
                "SELECT seq FROM sqlite_sequence WHERE name='events'",
                &[],
                Default::default()
            )
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Integer(99))
    );
}

#[test]
fn v4_import_rejects_duplicate_meta_keys_without_publishing_target() {
    let (directory, source) = database_path();
    let destination = directory.path().join("destination.sqlite3");
    let registry_id = "11111111-2222-4333-8444-555555555555";
    let marker = directory.path().join("rust-cutover-v1.json");
    let mut marker_file = kernal_api::platform::fs::create_private_file(&marker).unwrap();
    marker_file
        .write_all(format!(r#"{{"protocol":1,"registry_id":"{registry_id}"}}"#).as_bytes())
        .unwrap();
    marker_file.sync_all().unwrap();
    drop(marker_file);
    let reserved = kernal_api::platform::fs::create_private_file(&source).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    for statement in Registry::schema_sql()
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        connection.execute(statement, &[]).unwrap();
    }
    connection.execute("DROP TABLE meta", &[]).unwrap();
    connection
        .execute(
            "CREATE TABLE meta (key TEXT NOT NULL, value TEXT NOT NULL)",
            &[],
        )
        .unwrap();
    connection.execute("INSERT INTO meta VALUES ('schema_version','4'),('registry_id','11111111-2222-4333-8444-555555555555'),('registry_id','other')", &[]).unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::InvalidSchema)
    ));
    assert!(!destination.exists());
}

#[test]
fn bridge_migration_guard_excludes_a_second_rust_importer() {
    let (directory, _path) = database_path();
    let guard = acquire_legacy_migration_guard(directory.path()).unwrap();
    assert!(matches!(
        acquire_legacy_migration_guard(directory.path()),
        Err(Error::MigrationGuardHeld(_))
    ));
    drop(guard);
    acquire_legacy_migration_guard(directory.path()).unwrap();
}

#[test]
fn v4_import_refuses_unknown_or_newer_schema_without_publishing_destination() {
    let (directory, source, registry_id) = valid_v4_source();
    let destination = directory.path().join("destination.sqlite3");
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    connection
        .execute("CREATE TABLE future_python_state (key TEXT)", &[])
        .unwrap();
    drop(connection);
    let source_before_refusal = std::fs::read(&source).unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::InvalidSchema)
    ));
    assert!(!destination.exists());
    assert_eq!(std::fs::read(&source).unwrap(), source_before_refusal);
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    connection
        .execute("DROP TABLE future_python_state", &[])
        .unwrap();
    connection
        .execute("UPDATE meta SET value='5' WHERE key='schema_version'", &[])
        .unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::UnsupportedSchema(5))
    ));
    assert!(!destination.exists());
    assert_eq!(registry_id, "11111111-2222-4333-8444-555555555555");
}

#[test]
fn v4_import_refuses_live_ownership_and_preserves_pinned_volume_identity() {
    let (directory, source, _registry_id) = valid_v4_source();
    let destination = directory.path().join("destination.sqlite3");
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    connection.execute("INSERT INTO resources VALUES ('pinned-volume','volume','bosn-v4-data','stack','sha256:fixture','machine','/work',1,2,'active','pinned')", &[]).unwrap();
    connection.execute("INSERT INTO resource_uses VALUES ('pinned-volume','/work','stack','sha256:fixture',2,'active')", &[]).unwrap();
    connection
        .execute(
            "INSERT INTO leases VALUES ('live-lease','pinned-volume',?,NULL,1,2,30)",
            &[kernal_api::sqlite::Value::Integer(i64::from(
                std::process::id(),
            ))],
        )
        .unwrap();
    connection.execute("INSERT INTO execution_sessions VALUES ('live-session','container','docker',?,NULL,'[\"live-lease\"]')", &[kernal_api::sqlite::Value::Integer(i64::from(std::process::id()))]).unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::SourceOwnershipLive(pid)) if pid == std::process::id()
    ));
    assert!(!destination.exists());
    connection
        .execute("DELETE FROM execution_sessions", &[])
        .unwrap();
    connection.execute("DELETE FROM leases", &[]).unwrap();
    drop(connection);
    import_python_v4(directory.path(), &source, &destination).unwrap();
    let imported = kernal_api::sqlite::Connection::open_read_only(&destination).unwrap();
    assert_eq!(
        imported
            .query(
                "SELECT id,name,retention FROM resources WHERE id='pinned-volume'",
                &[],
                Default::default(),
            )
            .unwrap()[0]
            .get(0),
        Some(&kernal_api::sqlite::Value::Text("pinned-volume".into()))
    );
    assert_eq!(
        imported
            .query(
                "SELECT id,name,retention FROM resources WHERE id='pinned-volume'",
                &[],
                Default::default(),
            )
            .unwrap()[0]
            .get(1),
        Some(&kernal_api::sqlite::Value::Text("bosn-v4-data".into()))
    );
    assert_eq!(
        imported
            .query(
                "SELECT id,name,retention FROM resources WHERE id='pinned-volume'",
                &[],
                Default::default(),
            )
            .unwrap()[0]
            .get(2),
        Some(&kernal_api::sqlite::Value::Text("pinned".into()))
    );
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::ImportTargetExists(_))
    ));
}

#[test]
fn v4_import_refuses_orphaned_session_leases_and_in_place_destination() {
    let (directory, source, _registry_id) = valid_v4_source();
    let destination = directory.path().join("destination.sqlite3");
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    connection.execute("INSERT INTO execution_sessions VALUES ('session','container','docker',2147483647,NULL,'[\"missing-lease\"]')", &[]).unwrap();
    assert!(matches!(
        import_python_v4(directory.path(), &source, &destination),
        Err(Error::InvalidSchema)
    ));
    assert!(!destination.exists());
    connection
        .execute("DELETE FROM execution_sessions", &[])
        .unwrap();
    drop(connection);
    assert!(matches!(
        import_python_v4(directory.path(), &source, &source),
        Err(Error::SourceDestinationAliased(path)) if path == source
    ));
}
