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

use crate::managed_retention::{
    SetupContainerReport, StoppedSetupContainer, auto_retention_enabled, managed_retention_pass,
    setup_container_report_line,
};
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
    owned_container_inspect_with(false, &[], size)
}

/// As [`owned_container_inspect`], with liveness and a mount table.
///
/// `mounts` is rendered as Docker renders it: a list of `{Type, Name}`. A stopped
/// `bosn-setup-v2-*` container's cost is exactly this list, which is why the fake has to be able
/// to produce one.
fn owned_container_inspect_with(
    running: bool,
    mounts: &[(&str, &str)],
    size: Option<i128>,
) -> String {
    let size_field = size.map_or_else(String::new, |bytes| format!(r#","SizeRw":{bytes}"#));
    let mounts = mounts
        .iter()
        .map(|(mount_type, name)| format!(r#"{{"Type":"{mount_type}","Name":"{name}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let mounts = if mounts.is_empty() {
        String::new()
    } else {
        format!(r#","Mounts":[{mounts}]"#)
    };
    format!(
        r#"[{{"Id":"{FAKE_ID}","Created":"2020-01-01T00:00:00Z","State":{{"Running":{running}}},
"Config":{{"Labels":{{"{registry}":"{REGISTRY_ID}","{kind}":"container","{stack}":"stack",
"{generation}":"1","{scope}":"scope","{workspace}":"/w","{created}":"2020-01-01T00:00:00Z"}}}}
{mounts}{size_field}}}]"#,
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
    fake_docker_reporting(owned_container_inspect, sizes)
}

/// As [`fake_docker`], with the inspect document built by `render` instead of by
/// [`owned_container_inspect`].
///
/// One builder rather than a second fake: the #518 report and the #522/#523 removal races are the
/// same pass reading the same objects, and a separate fake would let them drift apart.
fn fake_docker_reporting(
    render: impl Fn(Option<i128>) -> String,
    sizes: &[Option<i128>],
) -> (DockerEngine, std::path::PathBuf) {
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
        engine = engine.env(format!("FD_INSPECT_{}", index + 1), render(*size));
    }
    engine = engine.env("FD_INSPECT_LAST", render(None));
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

// ---------------------------------------------------------------------------
// #518: stopped setup containers are reported by default, deleted only on opt-in.
// ---------------------------------------------------------------------------

/// A stopped, fully-labelled setup container holding `volume_count` stack volumes.
///
/// The volumes are named `bosn-v-stack-*`, which is what `bosn-setup` actually mounts; the report
/// counts volumes, so the names matter only in that they are distinct.
fn setup_container_pinning(volume_count: usize) -> String {
    let mounts: Vec<(String, String)> = (0..volume_count)
        .map(|index| ("volume".to_owned(), format!("bosn-v-stack-{index}")))
        .collect();
    let borrowed: Vec<(&str, &str)> = mounts
        .iter()
        .map(|(mount_type, name)| (mount_type.as_str(), name.as_str()))
        .collect();
    owned_container_inspect_with(false, &borrowed, Some(1_000))
}

#[test]
fn a_stopped_setup_container_is_reported_with_the_volumes_it_pins() {
    let (engine, _) =
        fake_docker_reporting(|_| setup_container_pinning(4), &[Some(1_000), Some(1_000)]);
    let dir = state_dir_with_registry("setup-pins-volumes");

    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), false);

    let report = &outcome.setup_containers;
    assert_eq!(
        report.container_count(),
        1,
        "the stopped container must be reported"
    );
    assert_eq!(
        report.pinned_volume_count(),
        4,
        "the whole point of the report is how many volumes each container pins"
    );
    assert_eq!(
        report.stopped[0].pinned_volumes,
        vec![
            "bosn-v-stack-0".to_owned(),
            "bosn-v-stack-1".to_owned(),
            "bosn-v-stack-2".to_owned(),
            "bosn-v-stack-3".to_owned(),
        ],
        "the pinned volumes come from Docker's own mount table"
    );
    assert!(
        report.oldest_age_seconds().is_some_and(|age| age > 0.0),
        "a container created in 2020 is not new"
    );
}

#[test]
fn a_running_container_is_not_reported_as_stopped() {
    let (engine, _) = fake_docker_reporting(
        |size| owned_container_inspect_with(true, &[("volume", "bosn-v-stack-0")], size),
        &[Some(1_000), Some(1_000)],
    );
    let dir = state_dir_with_registry("running-not-stopped");

    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), false);

    assert!(
        outcome.setup_containers.is_empty(),
        "a running container is not a leaked one: {:?}",
        outcome.setup_containers
    );
    assert!(
        setup_container_report_line(&outcome.setup_containers, false).is_none(),
        "a healthy machine gets no warning line"
    );
}

/// A bind mount is not a volume, and an anonymous volume's name is not something the operator can
/// act on. Counting either would inflate the number the report exists to make honest.
#[test]
fn only_named_volume_mounts_count_as_pinned() {
    let (engine, _) = fake_docker_reporting(
        |size| {
            owned_container_inspect_with(
                false,
                &[
                    ("bind", "/home/user/src"),
                    ("volume", "bosn-v-stack-0"),
                    ("volume", "bosn-v-stack-1"),
                ],
                size,
            )
        },
        &[Some(1_000), Some(1_000)],
    );
    let dir = state_dir_with_registry("bind-mounts");

    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), false);

    assert_eq!(
        outcome.setup_containers.pinned_volume_count(),
        2,
        "a bind mount is a path on a filesystem Bosn does not own"
    );
}

/// #518: the report must appear on a default install, where nothing is opt-in yet.
#[test]
fn the_report_appears_with_the_default_opt_out_config() {
    let (engine, _) =
        fake_docker_reporting(|_| setup_container_pinning(3), &[Some(1_000), Some(1_000)]);
    // No `retention.toml` at all: this is what a default install looks like.
    let dir = state_dir_with_registry("default-config-report");

    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), false);

    let line = setup_container_report_line(&outcome.setup_containers, outcome.summary.applied)
        .expect("a default install must still be told it is leaking");
    assert!(
        line.contains("1 stopped owned setup container(s)"),
        "{line}"
    );
    assert!(line.contains("pinning 3 volume(s)"), "{line}");
    assert!(
        line.contains("oldest "),
        "the age is part of the report: {line}"
    );
    assert!(line.contains("past the 6h container gate"), "{line}");
    assert!(line.contains("auto_retention = true"), "{line}");
}

/// #518: without the opt-in, a pass that could reclaim removes nothing.
#[test]
fn nothing_is_removed_without_the_opt_in() {
    let (engine, fake_state) =
        fake_docker_reporting(|_| setup_container_pinning(2), &[Some(1_000), Some(1_000)]);
    let dir = state_dir_with_registry("no-opt-in-no-removal");
    assert!(
        !auto_retention_enabled(&dir),
        "a machine with no opt-in file is not opted in"
    );

    // Exactly what `maintenance_pass` computes on the default path.
    let apply = auto_retention_enabled(&dir);
    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), apply);

    assert!(!outcome.summary.applied, "nothing was applied");
    assert_eq!(
        outcome.summary.removed, 0,
        "the pile is reported, never reclaimed without the opt-in"
    );
    assert!(
        !fake_state.join("removed").exists(),
        "no removal command reached Docker at all"
    );
    assert!(!fake_state.join("rm-argv").exists());
    assert_eq!(
        outcome.setup_containers.pinned_volume_count(),
        2,
        "reporting does not depend on the opt-in"
    );
    let line = setup_container_report_line(&outcome.setup_containers, false).expect("a line");
    assert!(line.contains("enable with"), "{line}");
}

/// The opt-in changes only the advice, never the facts.
#[test]
fn an_applied_pass_reports_the_same_pile_and_advises_the_gc_command() {
    let (engine, _) =
        fake_docker_reporting(|_| setup_container_pinning(1), &[Some(1_000), Some(1_000)]);
    let dir = state_dir_with_registry("applied-advice");
    write_config(&dir, "auto_retention = true\n");

    let apply = auto_retention_enabled(&dir);
    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), apply);

    assert_eq!(outcome.setup_containers.pinned_volume_count(), 1);
    let line = setup_container_report_line(&outcome.setup_containers, outcome.summary.applied)
        .expect("a line");
    assert!(line.contains("bosn gc owned --apply --yes"), "{line}");
}

/// A shared volume is one blob on one filesystem; counting it once per container would overstate
/// the disk problem.
#[test]
fn a_volume_shared_by_two_stopped_containers_is_counted_once() {
    let report = SetupContainerReport {
        stopped: vec![
            StoppedSetupContainer {
                id: "a".to_owned(),
                age_seconds: 900.0,
                pinned_volumes: vec!["bosn-v-stack-0".to_owned()],
            },
            StoppedSetupContainer {
                id: "b".to_owned(),
                age_seconds: 100.0,
                pinned_volumes: vec!["bosn-v-stack-0".to_owned()],
            },
        ],
    };

    assert_eq!(report.container_count(), 2);
    assert_eq!(report.pinned_volume_count(), 1);
    assert_eq!(report.oldest_age_seconds(), Some(900.0));
}

#[test]
fn an_empty_pile_produces_no_line() {
    assert!(SetupContainerReport::default().is_empty());
    assert!(
        setup_container_report_line(&SetupContainerReport::default(), false).is_none(),
        "a machine with nothing leaked must stay quiet"
    );
}

/// A fake `docker` whose owned container `fake-container-id` is listed beside `gone-id`, an
/// object removed between the listing and the reads. Docker answers an inspect naming a missing
/// object with the found ones on stdout and `No such object` on stderr, exiting 1. With
/// `vanish_before_recheck`, the owned container disappears too, after the opening read.
fn fake_docker_with_vanishing(vanish_before_recheck: bool) -> (DockerEngine, std::path::PathBuf) {
    const SCRIPT: &str = r#"
if [ "$1" = "ps" ]; then
  case "$*" in *label=*) printf 'fake-container-id\ngone-id\n' ;; esac
  exit 0
fi
if [ "$1" = "volume" ] || [ "$1" = "image" ]; then exit 0; fi
if [ "$1" = "inspect" ]; then
  n=$(cat "$FD_STATE/inspects" 2>/dev/null || echo 0)
  n=$((n + 1))
  printf '%s' "$n" > "$FD_STATE/inspects"
  if [ "$n" -gt 1 ] && [ -n "$FD_VANISH" ]; then
    printf '[]\n'
    printf 'Error: No such object: fake-container-id\n' >&2
    exit 1
  fi
  printf '%s' "$FD_INSPECT"
  case "$*" in *gone-id*) printf 'Error: No such object: gone-id\n' >&2; exit 1 ;; esac
  exit 0
fi
if [ "$1" = "rm" ]; then
  printf '%s\n' "$*" >> "$FD_STATE/rm-argv"
  exit 0
fi
exit 0
"#;
    let dir = scratch_dir("fake-docker-vanishing");
    let _ = std::fs::remove_file(dir.join("inspects"));
    let _ = std::fs::remove_file(dir.join("rm-argv"));
    let mut engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", SCRIPT, "fake-docker"])
        .env("FD_STATE", dir.as_os_str())
        .env("FD_INSPECT", owned_container_inspect(Some(10)));
    if vanish_before_recheck {
        engine = engine.env("FD_VANISH", "1");
    }
    (engine, dir)
}

/// #550: an object removed between the listing and the inspect is absent, not an unreadable
/// engine, so the pass still reclaims everything else.
#[test]
fn an_object_that_vanished_before_the_read_does_not_refuse_the_pass() {
    let (engine, fake) = fake_docker_with_vanishing(false);
    let dir = state_dir_with_registry("vanished-before-read");
    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), true);

    assert_eq!(outcome.summary.refused, None, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.removed, 1, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.failed, 0, "{:?}", outcome.summary);
    let removed = std::fs::read_to_string(fake.join("rm-argv")).expect("one removal");
    assert_eq!(removed.trim(), "rm fake-container-id");
}

/// #550: a candidate that disappears before its re-check reached the desired state. It is
/// neither a failure nor a removal, and no `rm` is issued for it.
#[test]
fn a_candidate_gone_before_its_recheck_is_neither_removed_nor_failed() {
    let (engine, fake) = fake_docker_with_vanishing(true);
    let dir = state_dir_with_registry("vanished-before-recheck");
    let outcome = managed_retention_pass(&engine, &dir, RetentionPolicy::default(), true);

    assert_eq!(outcome.summary.refused, None, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.planned, 1, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.removed, 0, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.failed, 0, "{:?}", outcome.summary);
    assert!(!fake.join("rm-argv").exists(), "nothing was removed");
}

/// One owned volume's inspect document, as `docker volume inspect` renders it: no size.
fn owned_volume_inspect(name: &str) -> String {
    format!(
        r#"[{{"Name":"{name}","CreatedAt":"2020-01-01T00:00:00Z",
"Labels":{{"{registry}":"{REGISTRY_ID}","{kind}":"volume","{stack}":"stack",
"{generation}":"1","{scope}":"scope","{workspace}":"/w","{created}":"2020-01-01T00:00:00Z"}}}}]"#,
        registry = bosn_core::LABEL_REGISTRY,
        kind = bosn_core::LABEL_KIND,
        stack = bosn_core::LABEL_STACK,
        generation = bosn_core::LABEL_GENERATION,
        scope = bosn_core::LABEL_SCOPE,
        workspace = bosn_core::LABEL_WORKSPACE,
        created = bosn_core::LABEL_CREATED,
    )
}

/// #549: a fake that reports sizes only where Docker does. `SizeRw` appears only for
/// `inspect --size`, and a volume's size only in `system df -v`, never in `volume inspect`.
fn fake_docker_with_sizes() -> (DockerEngine, std::path::PathBuf) {
    const SCRIPT: &str = r#"
case "$1" in
  ps) case "$*" in *label=*) echo fake-container-id ;; esac; exit 0 ;;
  image) exit 0 ;;
  system) printf '%s' '{"Images":[],"Containers":[],"Volumes":[{"Name":"bosn-v-owned","Size":"2GB"}],"BuildCache":[]}'; exit 0 ;;
  volume)
    case "$2" in
      ls) echo bosn-v-owned ;;
      inspect) printf '%s' "$FD_VOLUME" ;;
      rm) printf '%s\n' "$*" >> "$FD_STATE/rm-argv" ;;
    esac
    exit 0 ;;
  inspect)
    case "$*" in *--size*) printf '%s' "$FD_SIZED" ;; *) printf '%s' "$FD_UNSIZED" ;; esac
    exit 0 ;;
  rm) printf '%s\n' "$*" >> "$FD_STATE/rm-argv"; exit 0 ;;
esac
exit 0
"#;
    let dir = scratch_dir("fake-docker-sizes");
    let _ = std::fs::remove_file(dir.join("rm-argv"));
    let engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", SCRIPT, "fake-docker"])
        .env("FD_STATE", dir.as_os_str())
        .env("FD_SIZED", owned_container_inspect(Some(4_096)))
        .env("FD_UNSIZED", owned_container_inspect(None))
        .env("FD_VOLUME", owned_volume_inspect("bosn-v-owned"));
    (engine, dir)
}

/// #549: under a byte ceiling, a container and a volume are measured, fit the budget and are
/// removed, and the reclaimed bytes are accounted for rather than reported as zero.
#[test]
fn containers_and_volumes_are_measured_so_a_byte_ceiling_can_remove_them() {
    let (engine, fake) = fake_docker_with_sizes();
    let dir = state_dir_with_registry("sizes-under-ceiling");
    let policy = RetentionPolicy {
        max_bytes: Some(10_000_000_000),
        ..RetentionPolicy::default()
    };
    let outcome = managed_retention_pass(&engine, &dir, policy, true);

    assert_eq!(outcome.summary.refused, None, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.deferred, 0, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.removed, 2, "{:?}", outcome.summary);
    assert_eq!(outcome.summary.removed_bytes, 4_096 + 2_000_000_000);
    let removed = std::fs::read_to_string(fake.join("rm-argv")).expect("removals");
    assert_eq!(removed, "rm fake-container-id\nvolume rm bosn-v-owned\n");
}
