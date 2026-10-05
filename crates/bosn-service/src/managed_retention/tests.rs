//! Safety invariants for managed retention (`bosn gc owned`, #456).
//!
//! The policy itself is pure and unit-tested in `bosn-core`. These cover the parts that exist
//! only here: that an incomplete engine read removes nothing, that the opt-in file gates
//! unattended reclamation, and that a destructive pass requires confirmation on both wire
//! flags. None of them need Docker.

use std::io::Write;

use crate::managed_retention::auto_retention_enabled;
use crate::wire::Request;
use crate::wire_validate::{OWNED_MAX_TTL_SECS, validate_managed_retention_request_wire};

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bosn-retention-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write_config(dir: &std::path::Path, contents: &str) {
    let mut file = std::fs::File::create(dir.join("retention.toml")).expect("config");
    file.write_all(contents.as_bytes()).expect("write");
}

// ---------------------------------------------------------------------------
// The unattended pass must never delete on a guess.
// ---------------------------------------------------------------------------

#[test]
fn an_absent_config_never_enables_unattended_reclamation() {
    let dir = scratch_dir("absent");
    let _ = std::fs::remove_file(dir.join("retention.toml"));
    assert!(
        !auto_retention_enabled(&dir),
        "a machine with no opt-in file must never reclaim unattended"
    );
}

#[test]
fn an_unreadable_state_directory_is_not_an_opt_in() {
    let missing = std::env::temp_dir().join("bosn-retention-does-not-exist-xyz");
    let _ = std::fs::remove_dir_all(&missing);
    assert!(
        !auto_retention_enabled(&missing),
        "an unreadable config must fail closed, not open"
    );
}

#[test]
fn only_an_explicit_true_enables_unattended_reclamation() {
    let dir = scratch_dir("explicit");
    for (contents, expected) in [
        ("auto_retention = true\n", true),
        ("auto_retention = yes\n", true),
        ("auto_retention = 1\n", true),
        ("[retention]\nauto_retention = true\n", true),
        ("auto_retention = true # nightly\n", true),
        ("auto_retention = false\n", false),
        ("# auto_retention = true\n", false),
        ("auto_retention = \"true\"\n", false),
        ("auto_retention\n", false),
        ("", false),
    ] {
        write_config(&dir, contents);
        assert_eq!(
            auto_retention_enabled(&dir),
            expected,
            "for config {contents:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Wire-level guards on a destructive pass.
// ---------------------------------------------------------------------------

/// A request carrying only the fields a managed-retention pass uses, valid by construction.
fn valid_request() -> Request {
    let mut request = Request::operation(38);
    request.owned_container_ttl_secs = 21_600;
    request.owned_volume_ttl_secs = 14 * 86_400;
    request.owned_image_ttl_secs = 30 * 86_400;
    request
}

#[test]
fn a_preview_needs_no_confirmation() {
    assert!(validate_managed_retention_request_wire(&valid_request()).is_ok());
}

#[test]
fn an_apply_must_confirm_on_both_flags() {
    let mut request = valid_request();
    request.owned_confirm = true;
    request.gc_confirm = true;
    assert!(validate_managed_retention_request_wire(&request).is_ok());

    // Only one of the two is refused, so a request built for another destructive operation
    // cannot be reinterpreted as this one.
    let mut only_owned = valid_request();
    only_owned.owned_confirm = true;
    assert!(validate_managed_retention_request_wire(&only_owned).is_err());

    let mut only_gc = valid_request();
    only_gc.gc_confirm = true;
    assert!(validate_managed_retention_request_wire(&only_gc).is_err());
}

#[test]
fn an_absurd_age_gate_is_refused_rather_than_clamped() {
    let mut request = valid_request();
    request.owned_container_ttl_secs = OWNED_MAX_TTL_SECS + 1;
    assert!(
        validate_managed_retention_request_wire(&request).is_err(),
        "a gate beyond the maximum must be refused, not silently substituted"
    );

    // The boundary itself is accepted.
    let mut boundary = valid_request();
    boundary.owned_container_ttl_secs = OWNED_MAX_TTL_SECS;
    assert!(validate_managed_retention_request_wire(&boundary).is_ok());
}

#[test]
fn a_zero_gate_is_allowed() {
    let mut request = valid_request();
    request.owned_container_ttl_secs = 0;
    request.owned_volume_ttl_secs = 0;
    request.owned_image_ttl_secs = 0;
    assert!(
        validate_managed_retention_request_wire(&request).is_ok(),
        "zero means 'reclaim as soon as idle' and is a legitimate choice"
    );
}

#[test]
fn a_negative_byte_ceiling_is_refused() {
    let mut request = valid_request();
    request.owned_max_bytes = -1;
    assert!(validate_managed_retention_request_wire(&request).is_err());
}

#[test]
fn fields_belonging_to_other_operations_are_refused() {
    let mutators: [fn(&mut Request); 8] = [
        |r| r.job_id = 7,
        |r| r.setup_task_name = "build".into(),
        |r| r.gc_candidate_token = "tok".into(),
        |r| r.unmanaged_include.push("x".into()),
        |r| r.ci_request = "{}".into(),
        |r| r.setup_done_confirm = true,
        |r| r.setup_adopt_confirm = true,
        |r| r.diagnostic_limit = 10,
    ];
    for mutate in mutators {
        let mut request = valid_request();
        mutate(&mut request);
        assert!(
            validate_managed_retention_request_wire(&request).is_err(),
            "a request carrying another operation's payload was accepted"
        );
    }
}
