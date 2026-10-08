use super::*;

const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn intent(index: usize) -> CacheHelperIntent {
    CacheHelperIntent {
        registry_id: OWNER.into(),
        nonce: format!("{index:08x}-2222-4333-8444-555555555555"),
        volume: "bosn-ci-cache-v1".into(),
        image: format!("docker.io/library/docker@sha256:{ID}"),
        created_at: 1.0,
        role: None,
    }
}

#[test]
fn helper_role_preserves_legacy_json_and_survives_restart() {
    let legacy = intent(140);
    let encoded = serde_json::to_string(&legacy).unwrap();
    assert!(!encoded.contains("role"));
    assert_eq!(
        serde_json::from_str::<CacheHelperIntent>(&encoded).unwrap(),
        legacy
    );
    let mut maintenance = legacy.clone();
    maintenance.role = Some(CacheHelperRole::MaintenanceV1);
    assert_ne!(maintenance.name(), legacy.name());
    let encoded = serde_json::to_string(&maintenance).unwrap();
    assert!(encoded.contains("maintenance_v1"));
    assert!(
        serde_json::from_str::<CacheHelperIntent>(
            &encoded.replace("maintenance_v1", "unrecognized")
        )
        .is_err()
    );
    let dir = fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_cache_helper(&maintenance).unwrap();
    tx.commit().unwrap();
    drop(registry);
    let registry = Registry::open_read_only(&path).unwrap();
    assert_eq!(
        registry
            .cache_helper(&maintenance.nonce)
            .unwrap()
            .unwrap()
            .intent,
        maintenance
    );
}

#[test]
fn pending_create_survives_restart_and_cannot_finish_without_a_known_id() {
    let dir = fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let value = intent(1);
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_cache_helper(&value).unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    assert_eq!(
        registry.cache_helper(&value.nonce).unwrap().unwrap().state,
        CacheHelperState::Pending
    );
    let mut tx = registry.begin_immediate().unwrap();
    assert!(tx.finish_cache_helper(&value.nonce, ID, 2.0).is_err());
    tx.commit().unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.register_cache_helper(&value.nonce, ID, 2.0).unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    let record = registry.cache_helper(&value.nonce).unwrap().unwrap();
    assert_eq!(record.state, CacheHelperState::Created);
    assert_eq!(record.container_id.as_deref(), Some(ID));
    let mut tx = registry.begin_immediate().unwrap();
    tx.finish_cache_helper(&value.nonce, ID, 3.0).unwrap();
    tx.commit().unwrap();
    assert!(
        registry
            .pending_cache_helpers(None, 64)
            .unwrap()
            .items
            .is_empty()
    );
    let mut tx = registry.begin_immediate().unwrap();
    assert!(
        tx.begin_cache_helper(&value).is_err(),
        "terminal nonces cannot be reused"
    );
    tx.commit().unwrap();
    drop(registry);
    let reader = Registry::open_read_only(&path).unwrap();
    assert_eq!(
        reader.cache_helper(&value.nonce).unwrap().unwrap().state,
        CacheHelperState::Removed
    );
}

#[test]
fn helper_identity_conflicts_and_invalid_receipts_leave_the_claim_intact() {
    let dir = fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(dir.path().join("registry.sqlite3"), OWNER).unwrap();
    let mut value = intent(2);
    value.registry_id = OTHER.into();
    let mut tx = registry.begin_immediate().unwrap();
    assert!(tx.begin_cache_helper(&value).is_err());
    tx.commit().unwrap();
    value.registry_id = OWNER.into();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_cache_helper(&value).unwrap();
    assert!(
        tx.register_cache_helper(&value.nonce, "bad-id", 2.0)
            .is_err()
    );
    tx.register_cache_helper(&value.nonce, ID, 2.0).unwrap();
    let different = "2".repeat(64);
    assert!(
        tx.register_cache_helper(&value.nonce, &different, 3.0)
            .is_err()
    );
    assert!(
        tx.finish_cache_helper(&value.nonce, &different, 3.0)
            .is_err()
    );
    assert!(tx.finish_cache_helper(&value.nonce, ID, 1.0).is_err());
    tx.commit().unwrap();
    assert_eq!(
        registry
            .cache_helper(&value.nonce)
            .unwrap()
            .unwrap()
            .container_id
            .as_deref(),
        Some(ID)
    );
}

#[test]
fn identical_cleanup_registration_does_not_grow_the_audit_ledger() {
    let dir = fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let value = intent(150);
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_cache_helper(&value).unwrap();
    tx.register_cache_helper(&value.nonce, ID, 2.0).unwrap();
    tx.commit().unwrap();
    let initial = registry.events(0, 256).unwrap().items;
    for at in 3..103 {
        let mut tx = registry.begin_immediate().unwrap();
        tx.register_cache_helper(&value.nonce, ID, f64::from(at))
            .unwrap();
        tx.commit().unwrap();
    }
    let snapshots = registry
        .events(0, 256)
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.kind == kind(&value.nonce).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        snapshots, initial,
        "identical retries must not change the journal"
    );
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    let record = registry.cache_helper(&value.nonce).unwrap().unwrap();
    assert_eq!(record.state, CacheHelperState::Created);
    assert_eq!(record.container_id.as_deref(), Some(ID));
    assert_eq!(record.updated_at, 2.0);
    let mut tx = registry.begin_immediate().unwrap();
    assert!(tx.register_cache_helper(&value.nonce, ID, 1.0).is_err());
    tx.finish_cache_helper(&value.nonce, ID, 103.0).unwrap();
    assert!(tx.register_cache_helper(&value.nonce, ID, 104.0).is_err());
    assert!(tx.begin_cache_helper(&value).is_err());
    tx.commit().unwrap();
    let snapshots = registry
        .events(0, 256)
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.kind == kind(&value.nonce).unwrap())
        .collect::<Vec<_>>();
    let [terminal] = snapshots.as_slice() else {
        panic!("terminal authority must replace historical helper snapshots: {snapshots:?}");
    };
    assert!(terminal.id > initial[0].id);
    assert_eq!(terminal.at, 103.0);
    drop(registry);
    let registry = Registry::open_writer(&path).unwrap();
    let record = registry.cache_helper(&value.nonce).unwrap().unwrap();
    assert_eq!(record.state, CacheHelperState::Removed);
    assert_eq!(record.updated_at, 103.0);
    assert!(
        registry
            .pending_cache_helpers(None, 64)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn bounded_pending_pages_ignore_completed_history_and_advance_fairly() {
    let dir = fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(dir.path().join("registry.sqlite3"), OWNER).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for index in 0..130 {
        let value = intent(index);
        tx.begin_cache_helper(&value).unwrap();
        if index < 125 {
            tx.register_cache_helper(&value.nonce, ID, 2.0).unwrap();
            tx.finish_cache_helper(&value.nonce, ID, 3.0).unwrap();
        }
    }
    tx.commit().unwrap();
    assert!(registry.pending_cache_helpers(None, 0).is_err());
    assert!(registry.pending_cache_helpers(None, 65).is_err());
    let first = registry.pending_cache_helpers(None, 2).unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.items[0].intent.nonce, intent(125).nonce);
    let second = registry
        .pending_cache_helpers(first.next_nonce.as_deref(), 2)
        .unwrap();
    assert_eq!(second.items.len(), 2);
    assert_eq!(second.items[0].intent.nonce, intent(127).nonce);
    let last = registry
        .pending_cache_helpers(second.next_nonce.as_deref(), 2)
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].intent.nonce, intent(129).nonce);
    assert!(last.next_nonce.is_none());
}
