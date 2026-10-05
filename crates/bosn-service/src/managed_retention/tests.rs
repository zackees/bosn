//! Safety invariants for managed retention (`bosn gc owned`, #456).
//!
//! The policy itself is pure and unit-tested in `bosn-core`. These cover the parts that exist
//! only here: that an incomplete engine read removes nothing, that the opt-in file gates
//! unattended reclamation, that a destructive pass requires confirmation on both wire flags,
//! and that the pre-removal re-check and the removal itself never act on stale or live data.
//! None of them need Docker: the engine reads go through a shell standing in for the CLI.

use std::io::Write;

use bosn_core::retention::RetentionPolicy;
use bosn_engine::DockerEngine;

use crate::managed_retention::{auto_retention_enabled, managed_retention_pass};
use crate::wire::Request;
use crate::wire_validate::{OWNED_MAX_TTL_SECS, validate_managed_retention_request_wire};

/// A scratch directory for one test. The counter keeps concurrently running tests from sharing
/// one, which would make their fakes overwrite each other's state.
fn scratch_dir(name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bosn-retention-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
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

// ---------------------------------------------------------------------------
// The apply pass must act on the state of the world now, not on the plan's.
//
// Both bugs below are races between the pass's opening read and its removals, so both are
// reproduced with a standing-in Docker CLI rather than a live engine. The fake can report one
// object to the opening read and a different one to the re-check, and it can refuse a removal
// the way Docker does for a container that started in between.
// ---------------------------------------------------------------------------

/// The registry id the fake labels its one owned object with.
const REGISTRY_ID: &str = "11111111-2222-4333-8444-555555555555";
/// The one container the fake holds.
const FAKE_ID: &str = "fake-container-id";

/// One old, fully-labelled `bosn-setup-v2-*` container as `docker inspect` reports it.
///
/// `size` is omitted entirely when `None`, which is how Docker says "I did not measure this".
fn owned_container_inspect(size: Option<i128>) -> String {
    let size_field = size.map_or_else(String::new, |bytes| format!(r#","SizeRw":{bytes}"#));
    format!(
        r#"[{{"Id":"{FAKE_ID}","Created":"2020-01-01T00:00:00Z","State":{{"Running":false}},
"Config":{{"Labels":{{"{registry}":"{REGISTRY_ID}","{kind}":"container","{stack}":"stack",
"{generation}":"1","{scope}":"scope","{workspace}":"/w","{created}":"2020-01-01T00:00:00Z"}}}}
{size_field}}}]"#,
        registry = bosn_core::LABEL_REGISTRY,
        kind = bosn_core::LABEL_KIND,
        stack = bosn_core::LABEL_STACK,
        generation = bosn_core::LABEL_GENERATION,
        scope = bosn_core::LABEL_SCOPE,
        workspace = bosn_core::LABEL_WORKSPACE,
        created = bosn_core::LABEL_CREATED,
    )
}

/// A fake `docker` holding one owned container and no volumes or images.
///
/// The *n*-th `inspect` answers `sizes[n-1]`, so a test can hand the pass an opening read and a
/// different re-check. The container is running at removal time whenever `$FD_STATE/running`
/// exists, and a `rm` that Docker would refuse is refused here too — including a `rm -f`, which
/// is the whole point: this fake models Docker, and Docker really does kill.
fn fake_docker(sizes: &[Option<i128>]) -> (DockerEngine, std::path::PathBuf) {
    const SCRIPT: &str = r#"
if [ "$1" = "ps" ]; then
  case "$*" in *label=*) echo fake-container-id ;; esac
  exit 0
fi
if [ "$1" = "volume" ] || [ "$1" = "image" ]; then exit 0; fi
if [ "$1" = "inspect" ]; then
  n=$(cat "$FD_STATE/inspects" 2>/dev/null || echo 0)
  n=$((n + 1))
  printf '%s' "$n" > "$FD_STATE/inspects"
  eval "doc=\$FD_INSPECT_$n"
  printf '%s' "${doc:-$FD_INSPECT_LAST}"
  exit 0
fi
if [ "$1" = "rm" ]; then
  printf '%s\n' "$*" >> "$FD_STATE/rm-argv"
  if [ -f "$FD_STATE/running" ]; then
    if [ "$2" = "-f" ]; then
      rm -f "$FD_STATE/running"
      : > "$FD_STATE/removed"
      exit 0
    fi
    printf 'Error response from daemon: container fake-container-id is running\n' >&2
    exit 1
  fi
  : > "$FD_STATE/removed"
  exit 0
fi
exit 0
"#;
    let dir = scratch_dir("fake-docker");
    let _ = std::fs::remove_file(dir.join("inspects"));
    let _ = std::fs::remove_file(dir.join("removed"));
    let _ = std::fs::remove_file(dir.join("rm-argv"));
    let _ = std::fs::remove_file(dir.join("running"));

    let mut engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", SCRIPT, "fake-docker"])
        .env("FD_STATE", dir.as_os_str());
    for (index, size) in sizes.iter().enumerate() {
        engine = engine.env(
            format!("FD_INSPECT_{}", index + 1),
            owned_container_inspect(*size),
        );
    }
    engine = engine.env("FD_INSPECT_LAST", owned_container_inspect(None));
    (engine, dir)
}

/// A state directory holding a registry whose id matches the fake's labels.
fn state_dir_with_registry(name: &str) -> std::path::PathBuf {
    let dir = scratch_dir(name);
    bosn_registry::Registry::create_writer(dir.join("registry.sqlite3"), REGISTRY_ID)
        .expect("registry");
    dir
}

/// #522: the summary must account for the size the re-check measured, not the plan's copy.
///
/// The container grows while the pass runs. What the removal actually frees is the new size, so
/// a summary built from the opening read under-reports the reclamation.
#[test]
fn removed_bytes_reflects_the_size_the_recheck_measured() {
    let (engine, _) = fake_docker(&[Some(1_000), Some(7_777)]);
    let dir = state_dir_with_registry("removed-bytes");
    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), true);

    assert_eq!(
        outcome.summary.removed, 1,
        "the container should be removed"
    );
    assert_eq!(
        outcome.summary.removed_bytes, 7_777,
        "removed_bytes must come from the re-check, not the plan's opening read"
    );
    assert!(
        outcome.summary.failures.is_empty(),
        "{:?}",
        outcome.summary.failures
    );
}

/// #522: a size the engine does not report is unmeasured, not the plan's stale value.
///
/// Falling back to the plan's number here would claim bytes that were never measured on this
/// pass, which is precisely the failure mode `removed_bytes` exists to avoid.
#[test]
fn an_unmeasured_size_is_reported_as_zero_rather_than_the_plans_number() {
    let (engine, _) = fake_docker(&[Some(1_000), None]);
    let dir = state_dir_with_registry("unmeasured");
    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), true);

    assert_eq!(outcome.summary.removed, 1);
    assert_eq!(
        outcome.summary.removed_bytes, 0,
        "an unmeasured size must stay unmeasured, never borrow the plan's stale value"
    );
}

/// #523: a container that started after the re-check survives, and the refusal is reported.
///
/// `-f` would make this removal succeed and kill live work the pass never inspected. Without it,
/// Docker refuses, and the pass must count that as a failure and move on rather than aborting or
/// pretending the object is gone.
#[test]
fn a_container_that_started_mid_pass_survives_and_is_reported_as_a_failure() {
    let (engine, fake_state) = fake_docker(&[Some(1_000), Some(1_000)]);
    std::fs::write(fake_state.join("running"), b"").expect("start the container mid-pass");
    let dir = state_dir_with_registry("started-mid-pass");

    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), true);

    assert_eq!(
        outcome.summary.removed, 0,
        "a running container is not removed"
    );
    assert_eq!(outcome.summary.removed_bytes, 0, "nothing was freed");
    assert_eq!(
        outcome.summary.failed, 1,
        "the refusal is reported, not swallowed"
    );
    assert_eq!(outcome.summary.failures.len(), 1);
    assert!(
        outcome.summary.failures[0].contains("is running"),
        "the failure must carry Docker's reason: {}",
        outcome.summary.failures[0]
    );
    assert!(
        !fake_state.join("removed").exists(),
        "the container must survive; the fake only removes it when `rm` is allowed to succeed"
    );
    let argv = std::fs::read_to_string(fake_state.join("rm-argv")).expect("argv log");
    assert!(!argv.contains("-f"), "removal must not force: {argv:?}");
}

/// #523: the plain `rm` still removes a container that had already stopped, which is what the
/// force flag was wrongly there for.
#[test]
fn an_already_stopped_container_is_removed_without_force() {
    let (engine, fake_state) = fake_docker(&[Some(1_000), Some(1_000)]);
    let dir = state_dir_with_registry("already-stopped");

    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), true);

    assert_eq!(
        outcome.summary.removed, 1,
        "a stopped container needs no force"
    );
    assert!(
        fake_state.join("removed").exists(),
        "the fake removed it, so `rm` reached Docker and Docker agreed"
    );
}
