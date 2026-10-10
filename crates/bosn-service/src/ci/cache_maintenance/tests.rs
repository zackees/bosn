use super::*;
use crate::ci::engine::MaintenanceAttempt;

fn policy() -> CachePolicy {
    toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap()
}
fn report() -> serde_json::Value {
    serde_json::json!({"schema_version":1,"root":root(),"partial":false,"errors":null,
        "budget_bytes":200,"remaining_completed_bytes":160,"protected_bytes":160,"budget_met":true,
        "namespaces":[{"schema_version":1,"namespace":"0123456789abcdef","status":"ready",
        "partial":false,"errors":null,"entry_count":0,"archive_bytes":160,"temporary_bytes":0,"untracked_bytes":160,
        "fingerprint":"a".repeat(64),"entries":null,"next_cursor":null,
        "retention":{"deleted_count":0,"reclaimed_archive_bytes":0,"receipts":null,"receipts_omitted":0,"budget_bytes":100,
        "remaining_completed_bytes":160,"protected_bytes":160,"budget_met":false}}]})
}
fn parse(value: &serde_json::Value) -> Result<CohortReport, String> {
    CohortReport::parse(&serde_json::to_vec(value).unwrap(), policy())
}

#[test]
fn protection_samples_may_cross_the_recent_use_boundary() {
    let mut value = report();
    // Root classification precedes the namespace sample. The same completed
    // archive ages out of protection while the root lease still fences writes.
    value["namespaces"][0]["retention"]["protected_bytes"] = 0.into();
    let parsed =
        parse(&value).expect("independent time samples are valid published-protocol evidence");
    assert_eq!(parsed.protected_bytes, Some(160));
    assert_eq!(
        parsed.namespaces.unwrap()[0]
            .retention
            .as_ref()
            .unwrap()
            .protected_bytes,
        Some(0)
    );
}

#[test]
fn incomplete_unknown_and_unmet_budgets_are_distinct() {
    let mut value = report();
    value["partial"] = true.into();
    value["remaining_completed_bytes"] = serde_json::Value::Null;
    value["protected_bytes"] = serde_json::Value::Null;
    value["budget_met"] = serde_json::Value::Null;
    let attempt = MaintenanceAttempt {
        exit_code: 1,
        report: parse(&value).unwrap(),
        diagnostic: "busy".into(),
    };
    assert!(attempt.require_complete().is_err());
    assert!(attempt.report.remaining_completed_bytes.is_none());
    let mut value = report();
    value["remaining_completed_bytes"] = 240.into();
    value["budget_met"] = false.into();
    value["namespaces"][0]["retention"]["remaining_completed_bytes"] = 240.into();
    let attempt = MaintenanceAttempt {
        exit_code: 0,
        report: parse(&value).unwrap(),
        diagnostic: String::new(),
    };
    attempt.require_complete().unwrap();
    assert!(attempt.require_budget_met().is_err());
    assert_eq!(attempt.report.protected_bytes, Some(160));
}

#[test]
fn contradictory_identity_ceiling_and_success_evidence_is_refused() {
    for (field, invalid) in [
        ("schema_version", 2.into()),
        ("root", "/other".into()),
        ("budget_bytes", 201.into()),
        ("protected_bytes", 161.into()),
        ("budget_met", false.into()),
        ("remaining_completed_bytes", serde_json::Value::Null),
    ] {
        let mut value = report();
        value[field] = invalid;
        assert!(parse(&value).is_err(), "{field}");
    }
    let mut value = report();
    value["partial"] = true.into();
    value["remaining_completed_bytes"] = 160.into();
    value["protected_bytes"] = 0.into();
    let attempt = MaintenanceAttempt {
        exit_code: 1,
        report: parse(&value).unwrap(),
        diagnostic: String::new(),
    };
    assert_eq!(attempt.report.remaining_completed_bytes, Some(160));
    assert!(attempt.require_complete().is_err());
    assert!(CohortReport::parse(&vec![b' '; 65537], policy()).is_err());
}

#[test]
fn eviction_receipts_have_checked_counts_bytes_and_unique_ids() {
    let proof = RetentionReport {
        deleted_count: 2,
        reclaimed_archive_bytes: 80,
        receipts: Some(vec![EvictionReceipt {
            id: 1,
            reason: EvictionReason::AbsoluteAge,
            archive_bytes: 80,
        }]),
        receipts_omitted: 1,
        budget_bytes: 100,
        remaining_completed_bytes: Some(0),
        protected_bytes: Some(0),
        budget_met: Some(true),
    };
    proof.validate(policy().repository_max_bytes).unwrap();
    let mut invalid = proof;
    invalid.receipts_omitted = u64::MAX;
    assert!(invalid.validate(policy().repository_max_bytes).is_err());
    invalid.receipts_omitted = 1;
    invalid.reclaimed_archive_bytes = 79;
    assert!(invalid.validate(policy().repository_max_bytes).is_err());
}
