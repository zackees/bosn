//! Opt-in, live-Docker proof that a real `bosn ci` job leaves nothing behind (#545, #547): the
//! run creates the shared engine and its storage volume, uses them, and once idle the daemon
//! retires both and releases the machine claim. A zero-age retention pass then finds nothing of
//! this registry's left, and foreign objects are untouched throughout.
//!
//! The machine engine claim is engine-wide, so this test refuses to run on an engine where one is
//! already held. Run it against a throwaway engine (for example a privileged `docker:29.7.2`
//! daemon as `$DOCKER_HOST`) with `XDG_STATE_HOME` pointing at a temporary directory:
//! `soldr cargo test -j1 -p bosn-service --test managed_retention_ci_docker --locked -- --ignored --test-threads 1`
//! It needs network access for the runner image.

mod support;

use support::retention_docker::*;
use support::setup_docker::*;

const CLAIM: &str = "bosn-ci-engine-claim";
const MACHINE_CACHE: &str = "bosn-ci-cache-v1";
const WORKFLOW: &str = "on: [push]\njobs:\n  ok:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo bosn545-used\n";
const RETIRE_DEADLINE: Duration = Duration::from_secs(180);

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

#[test]
#[ignore = "requires a throwaway Docker engine and network access"]
fn live_docker_a_real_ci_job_is_retired_and_leaves_nothing() {
    throwaway_machine_root();
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    assert!(
        !container_exists(&engine, CLAIM),
        "a machine CI engine claim is held on this engine; run against a throwaway engine"
    );
    let unique = test_unique_suffix();
    let decoys = Decoys::plant(&engine, &unique);
    let mut cleanup = Throwaway::new(&engine);
    cleanup.containers.push(CLAIM.to_owned());
    if !volume_exists(&engine, MACHINE_CACHE) {
        cleanup.volumes.push(MACHINE_CACHE.to_owned());
    }
    let root = tempfile::tempdir().expect("temporary test root");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join(".github/workflows")).expect("create repo");
    std::fs::write(repo.join(".github/workflows/ci.yml"), WORKFLOW).expect("write workflow");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let state = root.path().join("state");
    let registry = own_registry(&state);
    std::fs::write(
        state.join("config.toml"),
        "[engine]\nidle_retire_secs = 5\n",
    )
    .expect("write engine config");
    let ours = format!("label={}={registry}", bosn_core::LABEL_REGISTRY);

    // Create and use: one real job through the shared engine.
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("construct runtime");
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
        .args(["ci", "run", "--wait", "--json", "--state-dir"])
        .arg(&state)
        .current_dir(&repo)
        .env("BOSN_CI_ACTOR", "human")
        .output()
        .expect("run bosn ci run");
    assert!(
        output.status.success(),
        "bosn ci run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let engines = listed(&engine, "container", &ours);
    let storage = listed(&engine, "volume", &ours);
    cleanup.containers.extend(engines.iter().cloned());
    cleanup.volumes.extend(storage.iter().cloned());
    eprintln!("created: engine {engines:?}, storage {storage:?}");
    assert_eq!(engines.len(), 1, "one shared engine: {engines:?}");
    assert_eq!(storage.len(), 1, "one engine storage volume: {storage:?}");

    // The engine is up: a zero-age pass holds it as in use.
    let held = zero_age_pass(&engine, &state);
    assert_eq!(
        held.retention.summary.removed, 0,
        "a running engine is in use"
    );
    assert!(container_exists(&engine, &engines[0]));

    // Idle: the daemon retires the engine, its storage volume and the claim.
    let deadline = Instant::now() + RETIRE_DEADLINE;
    let started = Instant::now();
    while !(listed(&engine, "container", &ours).is_empty()
        && listed(&engine, "volume", &ours).is_empty()
        && !container_exists(&engine, CLAIM))
    {
        assert!(Instant::now() < deadline, "the idle engine was not retired");
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!(
        "idle engine retired {:.0}s after the run",
        started.elapsed().as_secs_f64()
    );
    runtime.run(client.shutdown()).expect("shut down daemon");
    assert!(daemon.wait_for_exit().success(), "daemon failed");

    // Nothing of this registry's is left, and nothing foreign was touched.
    let after = zero_age_pass(&engine, &state);
    assert_eq!(after.retention.summary.failed, 0);
    assert!(listed(&engine, "container", &ours).is_empty());
    assert!(listed(&engine, "volume", &ours).is_empty());
    decoys.assert_intact(&engine);
    if volume_exists(&engine, MACHINE_CACHE) {
        let labels = String::from_utf8_lossy(
            &docker_capture(
                &engine,
                [
                    "volume",
                    "inspect",
                    "--format",
                    "{{json .Labels}}",
                    MACHINE_CACHE,
                ],
            )
            .stdout,
        )
        .into_owned();
        eprintln!("machine cache volume kept (machine-scoped): {labels}");
    }
}
