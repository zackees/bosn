use std::collections::BTreeMap;
use std::io::Write as _;

use bosn_core::{ResourceKind, ResourceState, Retention, Scope};
use bosn_registry::{
    Error, ExecutionSession, Generation, Lease, Registry, Resource, ResourceUse,
    SetupMissingRepair, VolumeCreationIntent, acquire_legacy_migration_guard, import_python_v4,
};

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

fn valid_v4_source() -> (
    kernal_api::platform::fs::TemporaryDirectory,
    std::path::PathBuf,
    &'static str,
) {
    let (directory, source) = database_path();
    let registry_id = "11111111-2222-4333-8444-555555555555";
    let marker = directory.path().join("rust-cutover-v1.json");
    let mut marker_file = kernal_api::platform::fs::create_private_file(&marker).unwrap();
    marker_file
        .write_all(format!(r#"{{"protocol":1,"registry_id":"{registry_id}"}}"#).as_bytes())
        .unwrap();
    marker_file.sync_all().unwrap();
    drop(marker_file);
    drop(kernal_api::platform::fs::create_private_file(&source).unwrap());
    let connection = kernal_api::sqlite::Connection::open(&source).unwrap();
    for statement in Registry::schema_sql()
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        connection.execute(statement, &[]).unwrap();
    }
    connection
        .execute(
            "INSERT INTO meta VALUES ('schema_version','4'),('registry_id',?)",
            &[kernal_api::sqlite::Value::Text(registry_id.into())],
        )
        .unwrap();
    drop(connection);
    (directory, source, registry_id)
}

fn database_path() -> (
    kernal_api::platform::fs::TemporaryDirectory,
    std::path::PathBuf,
) {
    let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = directory.path().join("registry.sqlite3");
    (directory, path)
}

fn resource(id: &str, name: &str) -> Resource {
    Resource {
        id: id.into(),
        kind: ResourceKind::Volume,
        name: name.into(),
        stack: "stack".into(),
        generation: "sha256:g".into(),
        scope: Scope::Stack,
        workspace: "/work".into(),
        created_at: 1.0,
        last_used: 2.0,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    }
}

fn active_setup_container(id: &str, name: &str, workspace: &str, generation: &str) -> Resource {
    Resource {
        id: id.into(),
        kind: ResourceKind::Container,
        name: name.into(),
        stack: "setup".into(),
        generation: generation.into(),
        scope: Scope::Machine,
        workspace: workspace.into(),
        created_at: 1.0,
        last_used: 1.0,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    }
}

fn active_setup_use(id: &str, workspace: &str, generation: &str) -> ResourceUse {
    ResourceUse {
        resource_id: id.into(),
        workspace: workspace.into(),
        stack: "setup".into(),
        generation: generation.into(),
        last_used: 1.0,
        state: ResourceState::Active,
    }
}

#[test]
fn missing_setup_repair_is_exact_atomic_idempotent_and_protects_ambiguous_state() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let workspace = "/canonical/work";
    let id = "setup-container:missing";
    let name = "bosn-setup-missing";
    let generation = "sha256:missing";
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&active_setup_container(id, name, workspace, generation))
        .unwrap();
    tx.put_resource_use(&active_setup_use(id, workspace, generation))
        .unwrap();
    tx.commit().unwrap();

    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(workspace, id, name, generation, 2.0)
            .unwrap(),
        Some(SetupMissingRepair::Repaired)
    );
    tx.commit().unwrap();
    assert!(
        registry
            .resources(0, 10)
            .unwrap()
            .items
            .iter()
            .any(|r| r.id == id && r.state == ResourceState::Retired)
    );
    assert!(
        registry
            .resource_uses(0, 10)
            .unwrap()
            .items
            .iter()
            .any(|u| u.resource_id == id && u.state == ResourceState::Retired)
    );
    assert_eq!(
        registry
            .events(0, 10)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "setup.reconcile.missing_repaired")
            .count(),
        1
    );
    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(workspace, id, name, generation, 3.0)
            .unwrap(),
        Some(SetupMissingRepair::AlreadyRepaired)
    );
    drop(tx);
    assert_eq!(
        registry.events(0, 10).unwrap().items.len(),
        1,
        "repeat does not write"
    );

    // Foreign uses, leases, sessions, and identity/generation mismatch all
    // fail closed and dropping the transaction proves no partial state/event.
    for (suffix, foreign_use, lease, session) in [
        ("foreign", true, false, false),
        ("lease", false, true, false),
        ("session", false, false, true),
    ] {
        let candidate_id = format!("setup-container:{suffix}");
        let candidate_name = format!("bosn-setup-{suffix}");
        let mut tx = registry.begin_immediate().unwrap();
        tx.put_resource(&active_setup_container(
            &candidate_id,
            &candidate_name,
            workspace,
            generation,
        ))
        .unwrap();
        tx.put_resource_use(&active_setup_use(&candidate_id, workspace, generation))
            .unwrap();
        if foreign_use {
            tx.put_resource_use(&active_setup_use(&candidate_id, "/other/work", generation))
                .unwrap();
        }
        if lease {
            tx.put_lease(&Lease {
                id: format!("lease-{suffix}"),
                resource_id: candidate_id.clone(),
                pid: 1,
                proc_start: None,
                acquired_at: 1.0,
                heartbeat_at: 1.0,
                ttl_seconds: 1.0,
            })
            .unwrap();
        }
        if session {
            tx.put_execution_session(&ExecutionSession {
                id: format!("session-{suffix}"),
                container_id: candidate_name.clone(),
                engine_binary: "docker".into(),
                client_pid: 1,
                client_start: None,
                lease_ids: vec![],
            })
            .unwrap();
        }
        tx.commit().unwrap();
        let mut tx = registry.begin_immediate().unwrap();
        assert_eq!(
            tx.repair_missing_setup_container(
                workspace,
                &candidate_id,
                &candidate_name,
                generation,
                4.0
            )
            .unwrap(),
            None
        );
        drop(tx);
        assert!(
            registry
                .resources(0, 20)
                .unwrap()
                .items
                .iter()
                .any(|r| r.id == candidate_id && r.state == ResourceState::Active)
        );
    }
    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(
            workspace,
            "setup-container:missing",
            name,
            "sha256:wrong",
            5.0
        )
        .unwrap(),
        None
    );
    drop(tx);

    let rollback_id = "setup-container:rollback";
    let rollback_name = "bosn-setup-rollback";
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&active_setup_container(
        rollback_id,
        rollback_name,
        workspace,
        generation,
    ))
    .unwrap();
    tx.put_resource_use(&active_setup_use(rollback_id, workspace, generation))
        .unwrap();
    tx.commit().unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(workspace, rollback_id, rollback_name, generation, 6.0)
            .unwrap(),
        Some(SetupMissingRepair::Repaired)
    );
    drop(tx);
    assert!(
        registry
            .resources(0, 32)
            .unwrap()
            .items
            .iter()
            .any(|r| r.id == rollback_id && r.state == ResourceState::Active)
    );
    assert_eq!(
        registry
            .events(0, 32)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "setup.reconcile.missing_repaired")
            .count(),
        1,
        "dropped repair rolls back state and event"
    );
}

#[test]
fn setup_gc_preview_only_returns_unambiguously_retired_managed_containers() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, state) in [
        (
            "setup-container:eligible",
            "bosn-setup-eligible",
            ResourceState::Retired,
        ),
        (
            "setup-container:active",
            "bosn-setup-active",
            ResourceState::Active,
        ),
        ("foreign", "foreign", ResourceState::Retired),
        (
            "setup-container:leased",
            "bosn-setup-leased",
            ResourceState::Retired,
        ),
        (
            "setup-container:session",
            "bosn-setup-session",
            ResourceState::Retired,
        ),
        (
            "setup-container:ambiguous",
            "bosn-setup-ambiguous",
            ResourceState::Retired,
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Container,
            name: name.into(),
            stack: "setup".into(),
            generation: "g".into(),
            scope: Scope::Machine,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "setup".into(),
            generation: "g".into(),
            last_used: 1.0,
            state: if id == "setup-container:active" {
                ResourceState::Active
            } else {
                ResourceState::Retired
            },
        })
        .unwrap();
    }
    tx.put_resource_use(&ResourceUse {
        resource_id: "setup-container:ambiguous".into(),
        workspace: "/other".into(),
        stack: "setup".into(),
        generation: "g".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.put_lease(&Lease {
        id: "lease".into(),
        resource_id: "setup-container:leased".into(),
        pid: 1,
        proc_start: None,
        acquired_at: 1.0,
        heartbeat_at: 1.0,
        ttl_seconds: 1.0,
    })
    .unwrap();
    tx.put_execution_session(&ExecutionSession {
        id: "session".into(),
        container_id: "bosn-setup-session".into(),
        engine_binary: "docker".into(),
        client_pid: 1,
        client_start: None,
        lease_ids: vec![],
    })
    .unwrap();
    tx.commit().unwrap();
    let preview = registry.setup_gc_preview("/work", 0, 1).unwrap();
    assert_eq!(preview.candidates.items.len(), 1);
    assert_eq!(preview.candidates.items[0].id, "setup-container:eligible");
    assert_eq!(preview.counts.protected_not_retired, 1);
    assert_eq!(preview.counts.protected_ambiguous_use, 1);
    assert_eq!(preview.counts.protected_lease, 1);
    assert_eq!(preview.counts.protected_session, 1);
    assert_eq!(preview.counts.excluded_unmanaged, 1);
}

#[test]
fn manifest_volume_gc_only_allows_retired_warm_spec_native_volumes() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, scope, retention) in [
        (
            "manifest-volume:eligible",
            "bosn-v-spec-eligible",
            Scope::Spec,
            Retention::Warm,
        ),
        (
            "manifest-volume:pinned",
            "bosn-v-spec-pinned",
            Scope::Spec,
            Retention::Pinned,
        ),
        (
            "manifest-volume:machine",
            "bosn-v-machine-machine",
            Scope::Machine,
            Retention::Warm,
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Volume,
            name: name.into(),
            stack: "app".into(),
            generation: "sha256:old".into(),
            scope,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Retired,
            retention,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "app".into(),
            generation: "sha256:old".into(),
            last_used: 1.0,
            state: ResourceState::Retired,
        })
        .unwrap();
    }
    tx.commit().unwrap();
    let preview = registry.manifest_volume_gc_preview("/work", 0, 16).unwrap();
    assert_eq!(
        preview
            .candidates
            .items
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        vec!["manifest-volume:eligible"]
    );
    assert_eq!(preview.counts.protected_policy, 2);
    let candidate = preview.candidates.items[0].clone();
    let mut tx = registry.begin_immediate().unwrap();
    assert!(
        tx.finalize_manifest_volume_gc_candidate(
            "/work",
            &candidate.id,
            &candidate.name,
            &candidate.generation,
            2.0,
            "manifest.volume_gc.removed"
        )
        .unwrap()
    );
    tx.commit().unwrap();
    assert!(
        registry
            .resource_by_kind_name(ResourceKind::Volume, &candidate.name)
            .unwrap()
            .is_none()
    );
}

#[test]
fn manifest_volume_rollover_retires_only_warm_spec_data() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, scope, retention) in [
        ("manifest-volume:spec", Scope::Spec, Retention::Warm),
        ("manifest-volume:pinned", Scope::Spec, Retention::Pinned),
        ("manifest-volume:stack", Scope::Stack, Retention::Warm),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Volume,
            name: format!("bosn-v-{}-{id}", scope.as_str()),
            stack: "app".into(),
            generation: "sha256:old".into(),
            scope,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "app".into(),
            generation: "sha256:old".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    tx.retire_prior_manifest_warm_spec_volume_generations("/work", "app", &[])
        .unwrap();
    tx.commit().unwrap();
    let rows = registry.resources(0, 16).unwrap().items;
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "manifest-volume:spec")
            .unwrap()
            .state,
        ResourceState::Retired
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "manifest-volume:pinned")
            .unwrap()
            .state,
        ResourceState::Active
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "manifest-volume:stack")
            .unwrap()
            .state,
        ResourceState::Active
    );
}

#[test]
fn manifest_generation_rollover_is_workspace_stack_scoped_and_keeps_sessions_protected() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let workspace_a = "/canonical/manifest-a";
    let workspace_b = "/canonical/manifest-b";
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, workspace, stack, generation) in [
        (
            "manifest-container:app:old",
            "bosn-setup-old",
            workspace_a,
            "app",
            "sha256:old",
        ),
        (
            "manifest-container:app:new",
            "bosn-setup-new",
            workspace_a,
            "app",
            "sha256:new",
        ),
        (
            "manifest-guest:app:old",
            "bosn-setup-guest-old",
            workspace_a,
            "app",
            "sha256:old-guest",
        ),
        (
            "manifest-container:app:other-workspace",
            "bosn-setup-other-workspace",
            workspace_b,
            "app",
            "sha256:other-workspace",
        ),
        (
            "manifest-container:other:other-stack",
            "bosn-setup-other-stack",
            workspace_a,
            "other",
            "sha256:other-stack",
        ),
        (
            "setup-container:setup",
            "bosn-setup-setup",
            workspace_a,
            "setup",
            "sha256:setup",
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Container,
            name: name.into(),
            stack: stack.into(),
            generation: generation.into(),
            scope: Scope::Machine,
            workspace: workspace.into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: workspace.into(),
            stack: stack.into(),
            generation: generation.into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    tx.put_execution_session(&ExecutionSession {
        id: "uncertain-manifest-task".into(),
        container_id: "bosn-setup-old".into(),
        engine_binary: "docker".into(),
        client_pid: 1,
        client_start: None,
        lease_ids: vec![],
    })
    .unwrap();
    tx.put_execution_session(&ExecutionSession {
        id: "uncertain-manifest-guest".into(),
        container_id: "bosn-setup-guest-old".into(),
        engine_binary: "docker".into(),
        client_pid: 2,
        client_start: None,
        lease_ids: vec![],
    })
    .unwrap();
    tx.retire_prior_manifest_container_generations(workspace_a, "app", "sha256:new")
        .unwrap();
    tx.commit().unwrap();

    let resources = registry.resources(0, 16).unwrap().items;
    let state = |id: &str| {
        resources
            .iter()
            .find(|resource| resource.id == id)
            .unwrap()
            .state
    };
    assert_eq!(state("manifest-container:app:old"), ResourceState::Retired);
    assert_eq!(state("manifest-guest:app:old"), ResourceState::Retired);
    assert_eq!(state("manifest-container:app:new"), ResourceState::Active);
    assert_eq!(
        state("manifest-container:app:other-workspace"),
        ResourceState::Active
    );
    assert_eq!(
        state("manifest-container:other:other-stack"),
        ResourceState::Active
    );
    assert_eq!(state("setup-container:setup"), ResourceState::Active);
    let uses = registry.resource_uses(0, 16).unwrap().items;
    assert_eq!(
        uses.iter()
            .find(|use_| use_.resource_id == "manifest-container:app:old")
            .unwrap()
            .state,
        ResourceState::Retired
    );
    // A rollover is registry-only. The uncertain task session survives and
    // keeps the retired generation out of conservative GC until cleared.
    assert_eq!(registry.status().unwrap().sessions, 2);
    let protected = registry.setup_gc_preview(workspace_a, 0, 16).unwrap();
    assert!(protected.candidates.items.is_empty());
    assert_eq!(protected.counts.protected_session, 2);
}

#[test]
fn setup_done_is_workspace_isolated_idempotent_and_preserves_shared_resources() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, workspace) in [
        ("shared", "/canonical/a"),
        ("only-a", "/canonical/a"),
        ("only-b", "/canonical/b"),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Image,
            name: format!("image-{id}"),
            stack: "setup".into(),
            generation: "g".into(),
            scope: Scope::Machine,
            workspace: workspace.into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: workspace.into(),
            stack: "setup".into(),
            generation: "g".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    // This foreign active use protects the machine-global resource state.
    tx.put_resource_use(&ResourceUse {
        resource_id: "shared".into(),
        workspace: "/canonical/b".into(),
        stack: "setup".into(),
        generation: "g2".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    // A non-setup use is never selected.
    tx.put_resource_use(&ResourceUse {
        resource_id: "only-a".into(),
        workspace: "/canonical/a".into(),
        stack: "other".into(),
        generation: "g3".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.commit().unwrap();

    let mut rollback = registry.begin_immediate().unwrap();
    assert_eq!(
        rollback
            .complete_setup_workspace("/canonical/a", 2.0)
            .unwrap()
            .uses_completed,
        2
    );
    drop(rollback);
    assert!(
        registry
            .resource_uses(0, 20)
            .unwrap()
            .items
            .iter()
            .filter(|use_row| use_row.workspace == "/canonical/a" && use_row.stack == "setup")
            .all(|use_row| use_row.state == ResourceState::Active)
    );
    assert!(registry.events(0, 20).unwrap().items.is_empty());

    let mut tx = registry.begin_immediate().unwrap();
    let result = tx.complete_setup_workspace("/canonical/a", 2.0).unwrap();
    assert_eq!(result.uses_completed, 2);
    assert_eq!(result.resources_completed, 0); // shared + other-stack active uses protect both
    tx.commit().unwrap();
    let uses = registry.resource_uses(0, 20).unwrap().items;
    assert!(
        uses.iter()
            .any(|u| u.resource_id == "only-b" && u.state == ResourceState::Active)
    );
    assert!(uses.iter().any(|u| u.resource_id == "only-a"
        && u.stack == "setup"
        && u.state == ResourceState::Done));
    assert!(uses.iter().any(|u| u.resource_id == "only-a"
        && u.stack == "other"
        && u.state == ResourceState::Active));
    assert!(
        registry
            .resources(0, 20)
            .unwrap()
            .items
            .iter()
            .all(|r| r.state == ResourceState::Active)
    );
    // Completion is not a GC retirement transition, so it cannot make a
    // candidate collectible merely by setting a use to done.
    assert!(
        registry
            .setup_gc_preview("/canonical/a", 0, 10)
            .unwrap()
            .candidates
            .items
            .is_empty()
    );
    let events = registry.events(0, 20).unwrap().items;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "setup.done")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .all(|event| !event.detail.contains("/canonical"))
    );

    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.complete_setup_workspace("/canonical/a", 3.0)
            .unwrap()
            .uses_completed,
        0
    );
    // Do not commit an idempotent no-op: no second event or timestamp write.
    drop(tx);
    assert_eq!(
        registry
            .events(0, 20)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "setup.done")
            .count(),
        1
    );

    // A following ensure upsert can reactivate the previously done use/resource.
    let mut reactivated = registry
        .resources(0, 20)
        .unwrap()
        .items
        .into_iter()
        .find(|r| r.id == "shared")
        .unwrap();
    reactivated.state = ResourceState::Active;
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&reactivated).unwrap();
    tx.put_resource_use(&ResourceUse {
        resource_id: "shared".into(),
        workspace: "/canonical/a".into(),
        stack: "setup".into(),
        generation: "g".into(),
        last_used: 4.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.commit().unwrap();
    assert!(
        registry
            .resource_uses(0, 20)
            .unwrap()
            .items
            .iter()
            .any(|u| u.resource_id == "shared"
                && u.workspace == "/canonical/a"
                && u.state == ResourceState::Active)
    );
}

#[test]
fn typed_transaction_writes_round_trip_all_tables_and_roll_back() {
    let (_missing_dir, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.put_resource(&resource("rollback", "rollback")).unwrap();
    }
    assert!(registry.resources(0, 10).unwrap().items.is_empty());
    let resource = resource("resource", "volume");
    let mut tx = registry.begin_immediate().unwrap();
    tx.set_meta("purpose", "roundtrip").unwrap();
    tx.put_resource(&resource).unwrap();
    tx.put_resource_use(&ResourceUse {
        resource_id: "resource".into(),
        workspace: "/other".into(),
        stack: "stack".into(),
        generation: "sha256:g".into(),
        last_used: 3.0,
        state: ResourceState::Done,
    })
    .unwrap();
    tx.put_lease(&Lease {
        id: "lease".into(),
        resource_id: "resource".into(),
        pid: 42,
        proc_start: None,
        acquired_at: 4.0,
        heartbeat_at: 5.0,
        ttl_seconds: 900.0,
    })
    .unwrap();
    tx.put_execution_session(&ExecutionSession {
        id: "session".into(),
        container_id: "container".into(),
        engine_binary: "docker".into(),
        client_pid: 42,
        client_start: Some(6.0),
        lease_ids: vec!["lease".into()],
    })
    .unwrap();
    tx.put_volume_creation_intent(&VolumeCreationIntent {
        name: "intent".into(),
        labels: BTreeMap::from([("label".into(), "value".into())]),
        stack: "stack".into(),
        generation: "sha256:g".into(),
        scope: Scope::Stack,
        workspace: "/work".into(),
    })
    .unwrap();
    tx.put_generation(&Generation {
        workspace: "/work".into(),
        stack: "stack".into(),
        digest: "sha256:g".into(),
        created_at: 7.0,
        superseded_at: Some(8.0),
    })
    .unwrap();
    tx.append_event(9.0, "event", "detail").unwrap();
    tx.commit().unwrap();
    assert_eq!(registry.resources(0, 10).unwrap().items, vec![resource]);
    assert_eq!(
        registry.resource_uses(0, 10).unwrap().items[0].last_used,
        3.0
    );
    assert_eq!(registry.leases(0, 10).unwrap().items[0].acquired_at, 4.0);
    assert_eq!(
        registry.execution_sessions(0, 10).unwrap().items[0].lease_ids,
        vec!["lease"]
    );
    assert_eq!(
        registry.volume_creation_intents(0, 10).unwrap().items[0].labels,
        BTreeMap::from([("label".into(), "value".into())])
    );
    assert_eq!(
        registry.generations(0, 10).unwrap().items[0].superseded_at,
        Some(8.0)
    );
    assert_eq!(registry.events(0, 10).unwrap().items[0].detail, "detail");
}

#[test]
fn pagination_has_an_explicit_next_offset_after_more_than_maximum() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for number in 0..1_001 {
        tx.put_resource(&resource(
            &format!("id-{number:04}"),
            &format!("name-{number:04}"),
        ))
        .unwrap();
    }
    tx.commit().unwrap();
    let page = registry.resources(0, 10_000).unwrap();
    assert_eq!(page.items.len(), 1_000);
    assert_eq!(page.next_offset, Some(1_000));
    assert_eq!(registry.resources(1_000, 1_000).unwrap().items.len(), 1);
}

#[test]
fn read_only_setup_ensure_events_are_filtered_newest_first_and_do_not_write() {
    let (_directory, path) = database_path();
    let mut writer =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = writer.begin_immediate().unwrap();
    tx.append_event(
        1.0,
        "setup.ensure.submitted",
        "job_id=1 policy=refresh source=https",
    )
    .unwrap();
    tx.append_event(2.0, "unrelated", "must not appear")
        .unwrap();
    tx.append_event(3.0, "setup.ensure.succeeded", "job_id=1 outcome=succeeded")
        .unwrap();
    tx.commit().unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    let readonly = Registry::open_read_only(&path).unwrap();
    let page = readonly.setup_ensure_events(0, 1).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].kind, "setup.ensure.succeeded");
    assert_eq!(page.next_offset, Some(1));
    let next = readonly
        .setup_ensure_events(page.next_offset.unwrap(), 64)
        .unwrap();
    assert_eq!(next.items.len(), 1);
    assert_eq!(next.items[0].kind, "setup.ensure.submitted");
    assert!(next.next_offset.is_none());
    drop(readonly);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
}

#[test]
fn read_only_missing_does_not_create_and_legacy_or_newer_schemas_are_refused() {
    let (_directory, path) = database_path();
    assert!(Registry::open_read_only(&path).is_err());
    // If the failed read-only open created anything, create_new would fail.
    Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let (_v4_dir, v4) = database_path();
    let reserved = kernal_api::platform::fs::create_private_file(&v4).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&v4).unwrap();
    connection
        .execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .unwrap();
    connection.execute("INSERT INTO meta VALUES ('schema_version','4'),('registry_id','11111111-2222-4333-8444-555555555555')", &[]).unwrap();
    assert!(matches!(
        Registry::open_writer(&v4),
        Err(Error::LegacyImportRequired(4))
    ));
    let (_v6_dir, v6) = database_path();
    let reserved = kernal_api::platform::fs::create_private_file(&v6).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&v6).unwrap();
    connection
        .execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .unwrap();
    connection.execute("INSERT INTO meta VALUES ('schema_version','6'),('registry_id','11111111-2222-4333-8444-555555555555')", &[]).unwrap();
    assert!(matches!(
        Registry::open_writer(&v6),
        Err(Error::UnsupportedSchema(6))
    ));
    let (_bad_dir, malformed) = database_path();
    let reserved = kernal_api::platform::fs::create_private_file(&malformed).unwrap();
    drop(reserved);
    let connection = kernal_api::sqlite::Connection::open(&malformed).unwrap();
    connection
        .execute(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO meta VALUES ('schema_version','5'),('registry_id','not-a-uuid')",
            &[],
        )
        .unwrap();
    assert!(matches!(
        Registry::open_writer(&malformed),
        Err(Error::BadRow("registry_id"))
    ));
}

#[test]
fn malformed_persisted_json_is_a_typed_read_error() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_execution_session(&ExecutionSession {
        id: "bad".into(),
        container_id: "container".into(),
        engine_binary: "docker".into(),
        client_pid: 1,
        client_start: None,
        lease_ids: vec!["lease".into()],
    })
    .unwrap();
    tx.put_volume_creation_intent(&VolumeCreationIntent {
        name: "bad".into(),
        labels: BTreeMap::new(),
        stack: "stack".into(),
        generation: "g".into(),
        scope: Scope::Stack,
        workspace: "/work".into(),
    })
    .unwrap();
    tx.commit().unwrap();
    drop(registry);
    let connection = kernal_api::sqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE execution_sessions SET lease_ids='[1]'", &[])
        .unwrap();
    assert!(matches!(
        Registry::open_read_only(&path)
            .unwrap()
            .execution_sessions(0, 1),
        Err(Error::BadRow("lease ids"))
    ));
    connection
        .execute("UPDATE volume_creation_intents SET labels='[]'", &[])
        .unwrap();
    assert!(matches!(
        Registry::open_read_only(&path)
            .unwrap()
            .volume_creation_intents(0, 1),
        Err(Error::BadRow("labels"))
    ));
}

#[test]
fn creates_stable_id_and_excludes_a_second_writer_while_readers_work() {
    let (_directory, path) = database_path();
    let registry = Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    assert_eq!(
        registry.registry_id().unwrap(),
        "11111111-2222-4333-8444-555555555555"
    );
    assert!(Registry::open_read_only(&path).is_ok());
    assert!(matches!(
        Registry::open_writer(&path),
        Err(Error::WriterAlreadyHeld(_))
    ));
    drop(registry);
    assert_eq!(
        Registry::open_writer(&path).unwrap().registry_id().unwrap(),
        "11111111-2222-4333-8444-555555555555"
    );
}

#[test]
fn resource_identity_conflict_preserves_dependents_and_delete_cascades() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let original = resource("resource", "volume");
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&original).unwrap();
    tx.put_resource_use(&ResourceUse {
        resource_id: "resource".into(),
        workspace: "/work".into(),
        stack: "stack".into(),
        generation: "sha256:g".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.put_lease(&Lease {
        id: "lease".into(),
        resource_id: "resource".into(),
        pid: 1,
        proc_start: None,
        acquired_at: 1.0,
        heartbeat_at: 1.0,
        ttl_seconds: 1.0,
    })
    .unwrap();
    tx.commit().unwrap();
    let mut conflicting = original.clone();
    conflicting.id = "different".into();
    assert!(matches!(
        registry
            .begin_immediate()
            .unwrap()
            .put_resource(&conflicting),
        Err(Error::ResourceIdentityConflict)
    ));
    assert_eq!(registry.leases(0, 10).unwrap().items.len(), 1);
    assert_eq!(registry.resource_uses(0, 10).unwrap().items.len(), 1);
    let mut tx = registry.begin_immediate().unwrap();
    tx.delete_resource("resource").unwrap();
    tx.commit().unwrap();
    assert!(registry.leases(0, 10).unwrap().items.is_empty());
    assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
}

#[test]
fn malformed_v5_wrong_foreign_key_is_refused_before_writer_open() {
    let (_directory, path) = database_path();
    let registry = Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    drop(registry);
    let connection = kernal_api::sqlite::Connection::open(&path).unwrap();
    connection.execute("DROP TABLE leases", &[]).unwrap();
    connection.execute("CREATE TABLE leases (id TEXT PRIMARY KEY, resource_id TEXT NOT NULL REFERENCES resources(name) ON DELETE CASCADE, pid INTEGER NOT NULL, proc_start REAL, acquired_at REAL NOT NULL, heartbeat_at REAL NOT NULL, ttl_seconds REAL NOT NULL)", &[]).unwrap();
    connection
        .execute(
            "CREATE INDEX idx_leases_resource ON leases(resource_id)",
            &[],
        )
        .unwrap();
    assert!(matches!(
        Registry::open_writer(&path),
        Err(Error::InvalidSchema)
    ));
}
