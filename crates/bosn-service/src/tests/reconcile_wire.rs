//! Setup reconcile, retired-stop and done wires.

use super::*;

#[test]
fn reconcile_missing_token_is_exact_and_rejects_gc_or_tampered_forms() {
    let candidate = SetupReconcileCandidate {
        resource: Resource {
            id: "setup-container:abc".into(),
            kind: ResourceKind::Container,
            name: "bosn-setup-abc".into(),
            stack: "setup".into(),
            generation: "sha256:abc".into(),
            scope: Scope::Machine,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        },
        image_identities: vec![],
        missing_repairable: true,
    };
    let token = setup_reconcile_missing_token(&candidate);
    assert_eq!(
        parse_setup_reconcile_missing_token(&token).unwrap(),
        (
            "setup-container:abc".into(),
            "bosn-setup-abc".into(),
            "sha256:abc".into()
        )
    );
    assert!(parse_setup_reconcile_missing_token(&(token + "00")).is_err());
    assert!(parse_setup_reconcile_missing_token("sgc1-7465737400").is_err());
}

#[test]
fn reconcile_classifies_all_read_only_observations_conservatively() {
    let candidate = SetupReconcileCandidate {
        resource: Resource {
            id: "setup-container:abc".into(),
            kind: ResourceKind::Container,
            name: "bosn-setup-abc".into(),
            stack: "setup".into(),
            generation: "sha256:abc".into(),
            scope: Scope::Machine,
            workspace: "/private/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        },
        image_identities: vec!["sha256:image".into()],
        missing_repairable: true,
    };
    let observed = |running| SetupReconcileObserved {
        name: "/bosn-setup-abc".into(),
        running,
        image_identity: "sha256:image".into(),
        managed: "v1".into(),
        content: "abc".into(),
        container: "bosn-setup-abc".into(),
    };
    assert_eq!(
        classify_setup_reconcile(&candidate, Ok(Some(observed(true)))),
        "matching_running"
    );
    assert_eq!(
        classify_setup_reconcile(&candidate, Ok(Some(observed(false)))),
        "matching_stopped"
    );
    assert_eq!(classify_setup_reconcile(&candidate, Ok(None)), "missing");
    let mut value = observed(true);
    value.name = "/wrong".into();
    assert_eq!(
        classify_setup_reconcile(&candidate, Ok(Some(value))),
        "name_mismatch"
    );
    let mut value = observed(true);
    value.managed = "foreign".into();
    assert_eq!(
        classify_setup_reconcile(&candidate, Ok(Some(value))),
        "label_mismatch"
    );
    let mut value = observed(true);
    value.image_identity = "sha256:wrong".into();
    assert_eq!(
        classify_setup_reconcile(&candidate, Ok(Some(value))),
        "image_mismatch"
    );
    assert_eq!(
        classify_setup_reconcile(&candidate, Err("deadline".into())),
        "inspect_error"
    );
    let mut malformed = candidate.clone();
    malformed.resource.generation = "bad".into();
    assert_eq!(
        classify_setup_reconcile(&malformed, Ok(Some(observed(true)))),
        "unknown"
    );
}

#[test]
fn reconcile_preview_wire_is_bounded_and_rejects_every_nonsemantic_control() {
    let valid = Request {
        workspace: "/private/work".into(),
        diagnostic_after: 0,
        diagnostic_limit: 1,
        ..Request::operation(19)
    };
    assert!(validate_setup_reconcile_preview_request_wire(&valid).is_ok());
    let mut malformed = valid;
    malformed.setup_config = "docker run attacker".into();
    assert!(validate_setup_reconcile_preview_request_wire(&malformed).is_err());
    malformed.setup_config.clear();
    malformed.diagnostic_limit = MAX_REGISTRY_DIAGNOSTIC_PAGE + 1;
    assert!(validate_setup_reconcile_preview_request_wire(&malformed).is_err());
}

#[test]
fn reconcile_missing_repair_wire_requires_confirmation_and_only_a_token() {
    let valid = Request {
        workspace: "/work".into(),
        gc_candidate_token: "srm1-610062006300".into(),
        gc_confirm: true,
        ..Request::operation(20)
    };
    assert!(validate_setup_reconcile_repair_missing_request_wire(&valid).is_ok());
    let missing_confirmation = Request {
        workspace: "/work".into(),
        gc_candidate_token: "srm1-610062006300".into(),
        gc_confirm: false,
        ..Request::operation(20)
    };
    assert!(validate_setup_reconcile_repair_missing_request_wire(&missing_confirmation).is_err());
    let nonsemantic = Request {
        workspace: "/work".into(),
        gc_candidate_token: "srm1-610062006300".into(),
        gc_confirm: true,
        setup_config: "https://attacker.invalid/setup.toml".into(),
        ..Request::operation(20)
    };
    assert!(validate_setup_reconcile_repair_missing_request_wire(&nonsemantic).is_err());
}

#[test]
fn retired_stop_wire_requires_only_preview_identity_and_confirmation() {
    let request = || Request {
        workspace: "/workspace".into(),
        gc_candidate_token:
            "sgc1-73657475702d636f6e7461696e65723a6100626f736e2d73657475702d61007368613235363a6100"
                .into(),
        gc_confirm: true,
        ..Request::operation(18)
    };
    assert!(validate_setup_retired_stop_request_wire(&request()).is_ok());
    for invalid in [
        Request {
            gc_confirm: false,
            ..request()
        },
        Request {
            setup_deadline_ms: 1,
            ..request()
        },
        Request {
            setup_config: "docker://bad".into(),
            ..request()
        },
        Request {
            diagnostic_limit: 1,
            ..request()
        },
    ] {
        assert!(validate_setup_retired_stop_request_wire(&invalid).is_err());
    }
}

#[test]
fn setup_done_wire_requires_only_explicit_confirmation() {
    let request = || Request {
        workspace: "/workspace".into(),
        setup_done_confirm: true,
        ..Request::operation(16)
    };
    assert!(validate_setup_done_request_wire(&request()).is_ok());
    for invalid in [
        Request {
            setup_done_confirm: false,
            ..request()
        },
        Request {
            setup_config: "https://user:secret@example.invalid/setup.toml".into(),
            ..request()
        },
        Request {
            gc_confirm: true,
            ..request()
        },
        Request {
            setup_task_name: "injected".into(),
            ..request()
        },
    ] {
        assert!(validate_setup_done_request_wire(&invalid).is_err());
    }
}

#[test]
fn setup_ensure_reactivates_records_after_explicit_done() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace = "/canonical/workspace";
    let execution = setup_ensure_execution(workspace, "same", "sha256:image");
    record_setup_ensure(&mut registry, 1, &execution).unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    assert_eq!(
        transaction
            .complete_setup_workspace(workspace, 2.0)
            .unwrap()
            .uses_completed,
        2
    );
    transaction.commit().unwrap();
    assert!(
        registry
            .resource_uses(0, 10)
            .unwrap()
            .items
            .iter()
            .all(|use_row| use_row.state == ResourceState::Done)
    );
    record_setup_ensure(&mut registry, 2, &execution).unwrap();
    assert!(
        registry
            .resources(0, 10)
            .unwrap()
            .items
            .iter()
            .all(|resource| resource.state == ResourceState::Active)
    );
    assert!(
        registry
            .resource_uses(0, 10)
            .unwrap()
            .items
            .iter()
            .all(|use_row| use_row.state == ResourceState::Active)
    );
}
