use std::collections::BTreeMap;

use bosn_core::{ResourceKind, ResourceState, Retention, Scope};
use bosn_registry::{
    Error, ExecutionSession, Generation, Lease, Registry, Resource, ResourceUse,
    VolumeCreationIntent,
};

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
