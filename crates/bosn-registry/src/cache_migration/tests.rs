use super::*;
const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const NS: &str = "0123456789abcdef";

fn intent() -> CacheMigrationIntent {
    CacheMigrationIntent {
        namespace: NS.into(),
        nonce: OWNER.into(),
        max_bytes: 100,
        created_at: 1.0,
    }
}
fn proof() -> CachePublicationEvidence {
    CachePublicationEvidence {
        source_fingerprint: "a".repeat(64),
        imported_count: 1,
        imported_bytes: 80,
        retained_source_archive_bytes: 160,
    }
}

#[test]
fn restart_recovers_pending_intent_and_idempotent_publication_without_retry() {
    let dir = fs::TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_cache_migration(&intent()).unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    assert!(
        registry
            .cache_migration(NS)
            .unwrap()
            .unwrap()
            .publication
            .is_none()
    );
    let mut tx = registry.begin_immediate().unwrap();
    assert!(tx.begin_cache_migration(&intent()).is_err());
    tx.record_cache_publication(NS, OWNER, &proof(), 2.0)
        .unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.record_cache_publication(NS, OWNER, &proof(), 3.0)
        .unwrap();
    let mut conflicting = proof();
    conflicting.source_fingerprint = "b".repeat(64);
    assert!(
        tx.record_cache_publication(NS, OWNER, &conflicting, 4.0)
            .is_err()
    );
    tx.commit().unwrap();
    drop(registry);
    let reader = Registry::open_read_only(&path).unwrap();
    assert_eq!(
        reader.cache_migration(NS).unwrap().unwrap().publication,
        Some(proof())
    );
}

#[test]
fn rollback_invalid_identity_and_cold_evidence_never_advance_intent() {
    let dir = fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(dir.path().join("registry.sqlite3"), OWNER).unwrap();
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_cache_migration(&intent()).unwrap();
    }
    assert!(registry.cache_migration(NS).unwrap().is_none());
    let mut tx = registry.begin_immediate().unwrap();
    let mut invalid = intent();
    invalid.namespace = "../escape".into();
    assert!(tx.begin_cache_migration(&invalid).is_err());
    tx.begin_cache_migration(&intent()).unwrap();
    assert!(
        tx.record_cache_publication(NS, "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee", &proof(), 2.0)
            .is_err()
    );
    for (count, bytes, retained) in [(0, 0, 160), (1, 101, 160), (1, 80, 79)] {
        let mut invalid = proof();
        invalid.imported_count = count;
        invalid.imported_bytes = bytes;
        invalid.retained_source_archive_bytes = retained;
        assert!(
            tx.record_cache_publication(NS, OWNER, &invalid, 2.0)
                .is_err()
        );
    }
    assert!(
        tx.record_cache_publication(NS, OWNER, &proof(), f64::NAN)
            .is_err()
    );
    tx.commit().unwrap();
    assert!(
        registry
            .cache_migration(NS)
            .unwrap()
            .unwrap()
            .publication
            .is_none()
    );
}
