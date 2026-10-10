//! Opt-in, live-Docker proofs that managed retention reclaims what the real manifest path creates
//! and what an abandoned state directory left behind (#545), with nothing foreign touched.
//!
//! Run them against a throwaway engine with a throwaway machine state root, for example a
//! privileged `docker:29.7.2` daemon whose socket is `$DOCKER_HOST`, and with
//! `XDG_STATE_HOME` pointing at an empty temporary directory:
//! `soldr cargo test -j1 -p bosn-service --test managed_retention_paths_docker --locked -- --ignored --test-threads 1`

mod support;

use support::retention_docker::*;
use support::setup_docker::*;

/// A `bosn.toml` stack with no command (so it runs the idle keepalive), a reclaimable
/// stack-scoped volume and a pinned one, and one task that uses both.
fn manifest() -> String {
    "[stack.live]\n\
     dockerfile = 'live.Dockerfile'\n\
     [stack.live.mounts]\n\
     ws = { source = '.', destination = '/ws', readonly = true }\n\
     [stack.live.volumes]\n\
     state = { scope = 'stack', destination = '/state' }\n\
     keep = { scope = 'stack', destination = '/keep', retention = 'pinned' }\n\
     [task.use]\n\
     stack = 'live'\n\
     cmd = '''echo used > /state/proof && echo kept > /keep/proof && cat /state/proof'''\n"
        .to_owned()
}

/// Run with the command in the module docs.
///
/// Create through the real manifest path (`bosn run --task` against a daemon), use it, let it go
/// idle, then reclaim: the opt-out and the production 6 h gate hold it; at zero age the pass stops
/// the provably idle keepalive (#605), reclaims the container, then its unpinned manifest volume
/// (#604) and its Bosn-built image. The pinned volume, the base image and the decoys stay.
#[test]
#[ignore = "requires a throwaway Docker engine and the pinned Alpine image"]
fn live_docker_managed_retention_reclaims_a_real_manifest_stack() {
    throwaway_machine_root();
    let engine = DockerEngine::docker();
    let base = pinned_alpine_identity(&engine);
    let unique = test_unique_suffix();
    let decoys = Decoys::plant(&engine, &unique);
    let Stack {
        state,
        container,
        image,
        state_volume,
        keep_volume,
        ..
    } = &create_and_use(&engine, &unique);

    // Opted out: nothing is stopped or removed.
    std::fs::write(state.join("retention.toml"), "auto_retention = false\n").expect("opt out");
    let opted_out = zero_age_pass(&engine, state);
    assert!(opted_out.stopped_idle.is_empty() && !opted_out.retention.summary.applied);
    assert!(running(&engine, container), "opt-out keeps the keepalive");
    std::fs::remove_file(state.join("retention.toml")).expect("opt back in");

    // The production gates: the container is younger than 6 h, so it stays running.
    let production = bosn_service::managed_retention::maintenance_pass_with(
        &engine,
        state,
        bosn_core::retention::RetentionPolicy::default(),
    );
    assert!(
        production.stopped_idle.is_empty(),
        "within the 6 h idle gate"
    );
    assert_eq!(production.retention.summary.removed, 0);
    assert!(running(&engine, container));

    // Idle past the (zero) gate: stopped, then reclaimed, then its volume and image.
    let mut removed = 0;
    let mut bytes = 0;
    let mut stopped = Vec::new();
    for _ in 0..3 {
        let pass = zero_age_pass(&engine, state);
        stopped.extend(pass.stopped_idle);
        removed += pass.retention.summary.removed;
        bytes += pass.retention.summary.removed_bytes;
        decoys.assert_intact(&engine);
    }
    assert_eq!(
        &stopped,
        std::slice::from_ref(container),
        "the idle keepalive was stopped"
    );
    assert!(!container_exists(&engine, container), "container reclaimed");
    assert!(
        !volume_exists(&engine, state_volume),
        "unpinned manifest volume reclaimed"
    );
    assert!(
        !docker_capture(&engine, ["image", "inspect", image.as_str()]).ok(),
        "image reclaimed"
    );
    assert!(
        volume_exists(&engine, keep_volume),
        "the pinned manifest volume is honoured"
    );
    assert!(
        docker_capture(&engine, ["image", "inspect", base.as_str()]).ok(),
        "base image kept"
    );
    assert!(
        removed >= 3 && bytes > 0,
        "removed {removed} object(s), {bytes} bytes"
    );
    eprintln!(
        "manifest path: stopped {stopped:?}, removed {removed} object(s), {bytes} bytes; kept {keep_volume}"
    );
}

/// Run with the command in the module docs.
///
/// #607: state directory A enrolls in the machine catalog through a real daemon, then its
/// registry database is lost. Only the machine daemon's pass reclaims A's objects; a live peer's
/// and an uncataloged registry's stay foreign, as does everything else.
#[test]
#[ignore = "requires a throwaway Docker engine and the pinned Alpine image"]
fn live_docker_machine_pass_reclaims_an_abandoned_state_directory() {
    let machine = throwaway_machine_root();
    let engine = DockerEngine::docker();
    pinned_alpine_identity(&engine);
    let unique = test_unique_suffix();
    let decoys = Decoys::plant(&engine, &unique);
    let root = tempfile::tempdir().expect("temporary test root");
    if !machine.join("registry.sqlite3").exists() {
        own_registry(&machine);
    }
    let enrolled = |name: &str| {
        let state = root.path().join(name);
        let id = own_registry(&state);
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut daemon = DaemonChild::start(&state);
        let client = wait_for_client(&runtime, &mut daemon, &state);
        runtime.run(client.shutdown()).expect("shut down daemon");
        assert!(daemon.wait_for_exit().success());
        assert!(
            machine.join(CATALOG).join(format!("{id}.json")).is_file(),
            "the daemon enrolled {id} in the machine catalog"
        );
        (state, id)
    };
    let (abandoned_state, abandoned) = enrolled("abandoned");
    let (peer_state, peer) = enrolled("peer");
    let uncataloged = "00000000-0000-4000-8000-00000c0a7a10";
    let mut cleanup = Throwaway::new(&engine);
    let volume = |registry: &str, role: &str| {
        let name = format!("bosn545-{role}-{unique}");
        labelled_volume(&engine, &name, &canonical_labels(registry, "volume"));
        name
    };
    let abandoned_volume = volume(&abandoned, "abandoned");
    let uncataloged_volume = volume(uncataloged, "uncataloged");
    cleanup
        .volumes
        .extend([abandoned_volume.clone(), uncataloged_volume.clone()]);
    std::fs::remove_file(abandoned_state.join("registry.sqlite3")).expect("lose A's registry");

    // A peer's pass is not the machine daemon's: A's object stays.
    zero_age_pass(&engine, &peer_state);
    assert!(
        volume_exists(&engine, &abandoned_volume),
        "only the machine pass reclaims"
    );
    // The peer is live: its object must survive the machine pass.
    let peer_volume = volume(&peer, "peer");
    cleanup.volumes.push(peer_volume.clone());

    let pass = zero_age_pass(&engine, &machine);
    assert!(
        !volume_exists(&engine, &abandoned_volume),
        "the abandoned registry's volume"
    );
    assert!(
        volume_exists(&engine, &peer_volume),
        "a live peer's volume stays"
    );
    assert!(
        volume_exists(&engine, &uncataloged_volume),
        "an uncataloged registry stays foreign"
    );
    decoys.assert_intact(&engine);
    eprintln!(
        "abandoned state dir: removed {} object(s), {} bytes; held {:?}",
        pass.retention.summary.removed,
        pass.retention.summary.removed_bytes,
        pass.retention.summary.held
    );
}

/// The machine catalog's directory under the machine state root.
const CATALOG: &str = "registries";

/// What the real manifest path created for one test.
struct Stack {
    _root: tempfile::TempDir,
    state: std::path::PathBuf,
    container: String,
    image: String,
    state_volume: String,
    keep_volume: String,
    _cleanup: Throwaway,
}

/// Create through the real manifest path and use it: a daemon, then `bosn run --task use`.
fn create_and_use(engine: &DockerEngine, unique: &str) -> Stack {
    let root = tempfile::tempdir().expect("temporary test root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    own_registry(&state);
    std::fs::write(
        workspace.join("live.Dockerfile"),
        format!("FROM {PINNED_ALPINE}\nRUN printf '%s\\n' bosn545-{unique} > /proof\n"),
    )
    .expect("write live.Dockerfile");
    std::fs::write(workspace.join("bosn.toml"), manifest()).expect("write bosn.toml");

    // Real production creation and use.
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("construct runtime");
    let mut daemon = DaemonChild::start(&state);
    let client = wait_for_client(&runtime, &mut daemon, &state);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bosn"))
        .current_dir(&workspace)
        .args(["run", "--task", "use", "--state-dir"])
        .arg(&state)
        .output()
        .expect("run bosn run --task");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("used"),
        "bosn run --task use: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    runtime.run(client.shutdown()).expect("shut down daemon");
    assert!(daemon.wait_for_exit().success(), "daemon failed");

    let containers: Vec<String> =
        listed(engine, "container", "label=com.zackees.bosn.setup-managed")
            .into_iter()
            .filter(|name| {
                mounts(engine, name, "Source").contains(&workspace.display().to_string())
            })
            .collect();
    let [container] = containers.as_slice() else {
        panic!("exactly one stack container binds the workspace: {containers:?}");
    };
    let image = docker_text(
        engine,
        [
            "container",
            "inspect",
            "--format",
            "{{.Config.Image}}",
            container,
        ],
    );
    let volumes: Vec<String> = mounts(engine, container, "Name")
        .into_iter()
        .filter(|name| !name.is_empty())
        .collect();
    let mut cleanup = Throwaway::new(engine);
    cleanup.containers.push(container.clone());
    cleanup.volumes.extend(volumes.iter().cloned());
    cleanup.images.push(image.clone());
    // One inspect: Docker does not keep `.Mounts` in a stable order between inspects.
    let at = mounts(engine, container, "Destination}}={{.Name");
    let named = |destination: &str| {
        at.iter()
            .find_map(|pair| pair.strip_prefix(&format!("{destination}=")))
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("a volume at {destination}: {at:?}"))
    };
    let state_volume = named("/state");
    let keep_volume = named("/keep");
    assert!(
        docker_text(
            engine,
            [
                "image",
                "inspect",
                "--format",
                "{{json .RepoTags}}",
                image.as_str()
            ]
        )
        .contains("bosn-setup:"),
        "a Bosn-built image: {image}"
    );
    assert!(
        running(engine, container),
        "the keepalive is running after the task"
    );
    Stack {
        _root: root,
        state,
        container: container.clone(),
        image,
        state_volume,
        keep_volume,
        _cleanup: cleanup,
    }
}

fn docker_text<const N: usize>(engine: &DockerEngine, args: [&str; N]) -> String {
    let result = docker_capture(engine, args);
    assert!(result.ok(), "docker {args:?}");
    String::from_utf8_lossy(&result.stdout).trim().to_owned()
}

fn mounts(engine: &DockerEngine, container: &str, field: &str) -> Vec<String> {
    let format = format!("{{{{range .Mounts}}}}{{{{.{field}}}}}|{{{{end}}}}");
    docker_text(
        engine,
        [
            "container",
            "inspect",
            "--format",
            format.as_str(),
            container,
        ],
    )
    .split_terminator('|')
    .map(str::to_owned)
    .collect()
}

fn running(engine: &DockerEngine, container: &str) -> bool {
    docker_text(
        engine,
        [
            "container",
            "inspect",
            "--format",
            "{{.State.Running}}",
            container,
        ],
    ) == "true"
}
