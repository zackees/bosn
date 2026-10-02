//! Helpers and imports shared by the registry integration tests.
#![allow(dead_code, unused_imports)]

pub use std::collections::BTreeMap;
pub use std::io::Write as _;

pub use bosn_core::{ResourceKind, ResourceState, Retention, Scope};
pub use bosn_registry::{
    Error, ExecutionSession, Generation, Lease, Registry, Resource, ResourceUse,
    SetupMissingRepair, VolumeCreationIntent, acquire_legacy_migration_guard, import_python_v4,
};

pub fn valid_v4_source() -> (
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

pub fn database_path() -> (
    kernal_api::platform::fs::TemporaryDirectory,
    std::path::PathBuf,
) {
    let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = directory.path().join("registry.sqlite3");
    (directory, path)
}

pub fn resource(id: &str, name: &str) -> Resource {
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

pub fn active_setup_container(id: &str, name: &str, workspace: &str, generation: &str) -> Resource {
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

pub fn active_setup_use(id: &str, workspace: &str, generation: &str) -> ResourceUse {
    ResourceUse {
        resource_id: id.into(),
        workspace: workspace.into(),
        stack: "setup".into(),
        generation: generation.into(),
        last_used: 1.0,
        state: ResourceState::Active,
    }
}
