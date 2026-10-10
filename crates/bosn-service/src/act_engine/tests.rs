//! Unit tests for the Act engine boundary.

use super::*;
use serde_json::json;

pub(super) fn intent() -> ActEngineIntent {
    ActEngineIntent {
        run_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        workspace: "/private/source".into(),
        candidate_sha: "a".repeat(40),
        payload_sha256: "b".repeat(64),
        snapshot_sha256: "c".repeat(64),
        act_version: "0.2.88".into(),
        act_image_digest: format!("sha256:{}", "d".repeat(64)),
        engine_image_digest: format!("sha256:{}", "e".repeat(64)),
        runner_image_digest: format!("sha256:{}", "f".repeat(64)),
        creation_profile: Some(creation_profile(limits()).unwrap()),
        created_at: 1.0,
        spare: false,
    }
}
pub(super) const OWNER: &str = "11111111-2222-4333-8444-555555555555";
pub(super) fn limits() -> ActEngineLimits {
    ActEngineLimits {
        memory_bytes: 8 << 30,
        storage_bytes: 4 << 30,
        storage: EngineStorage::Memory,
        nano_cpus: 4_000_000_000,
        pids: 1024,
    }
}
fn document() -> serde_json::Value {
    let i = intent();
    let l = limits();
    json!([{"Id":"1".repeat(64),"Name":format!("/{}",i.engine_name()),"Image":format!("sha256:{}","2".repeat(64)),"Config":{"Image":format!("docker.io/library/docker@{}",i.engine_image_digest),"Entrypoint":null,"Labels":i.required_labels(OWNER).unwrap(),"Env":["DOCKER_TLS_CERTDIR=","DOCKER_CONTAINERD_ROOT=/var/lib/docker/containerd/daemon"],"Volumes":{"/var/lib/docker":{}},"Healthcheck":{"Test":["NONE"]},"Cmd":engine_command()},"HostConfig":{"Privileged":true,"Memory":l.memory_bytes,"MemorySwap":l.memory_bytes,"CpuPeriod":100000,"CpuQuota":l.nano_cpus/10_000,"ReadonlyRootfs":true,"PidMode":"","IpcMode":"private","CgroupnsMode":"private","PidsLimit":l.pids,"Binds":null,"PortBindings":{},"NetworkMode":"bridge","LogConfig":{"Type":"local","Config":{"max-size":"1m","max-file":"2"}},"Tmpfs":l.tmpfs()},"Mounts":[{"Type":"tmpfs","Destination":"/var/lib/docker"},{"Type":"tmpfs","Destination":"/run"},{"Type":"tmpfs","Destination":"/tmp"}]}])
}
#[test]
fn real_descriptor_without_annotations_requires_publisher_config_proof() {
    let config =
        serde_json::to_vec(&serde_json::json!({"os":"linux","architecture":"amd64"})).unwrap();
    let hash = |b: &[u8]| format!("sha256:{}", kernal_api::hash::Sha256Hasher::digest(b));
    let config_pin = hash(&config);
    let manifest = serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_pin,"size":config.len()}})).unwrap();
    let pin = hash(&manifest);
    let mut i = intent();
    i.engine_image_digest = pin.clone();
    let image = serde_json::json!([{"Id":pin,"RepoDigests":[format!("docker.io/library/docker@{pin}")],"Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":pin,"size":manifest.len()}}]);
    let document = serde_json::to_vec(&image).unwrap();
    assert!(
        observe_engine_image(&document, &i, &config_pin).is_err(),
        "real shape reproduces legacy refusal"
    );
    let proof = VerifiedEngineManifest::verify(&manifest, &pin, &config, &config_pin).unwrap();
    let verified = observe_engine_image_from_manifest(&document, &i, &proof).unwrap();
    assert_eq!(verified.docker_image_id, pin);
    let classic = serde_json::json!([{"Id":config_pin,"RepoDigests":[format!("docker@{pin}")]}]);
    assert!(
        observe_engine_image_from_manifest(&serde_json::to_vec(&classic).unwrap(), &i, &proof)
            .is_ok()
    );
    let mut foreign_intent = i.clone();
    foreign_intent.engine_image_digest = format!("sha256:{}", "0".repeat(64));
    assert!(observe_engine_image_from_manifest(&document, &foreign_intent, &proof).is_err());

    for change in ["digest", "size", "mediaType", "Id", "RepoDigests"] {
        let mut wrong = image.clone();
        match change {
            "size" => wrong[0]["Descriptor"][change] = serde_json::json!(0),
            "Id" => wrong[0][change] = serde_json::json!(config_pin.replace('a', "b") + "0"),
            "RepoDigests" => wrong[0][change] = serde_json::json!([]),
            _ => wrong[0]["Descriptor"][change] = serde_json::json!("foreign"),
        }
        assert!(
            observe_engine_image_from_manifest(&serde_json::to_vec(&wrong).unwrap(), &i, &proof)
                .is_err(),
            "{change}"
        );
    }
    for change in ["digest", "size", "mediaType"] {
        let mut wrong = serde_json::from_slice::<Value>(&manifest).unwrap();
        wrong["config"][change] = serde_json::json!("foreign");
        let bytes = serde_json::to_vec(&wrong).unwrap();
        assert!(
            VerifiedEngineManifest::verify(&bytes, &hash(&bytes), &config, &config_pin).is_err()
        );
    }
    assert!(VerifiedEngineManifest::verify(&manifest, &pin, b"corrupt", &config_pin).is_err());
    assert!(VerifiedEngineManifest::verify(b"corrupt", &pin, &config, &config_pin).is_err());
}
fn classic_identity() -> VerifiedEngineImage {
    let i = intent();
    let config = format!("sha256:{}", "2".repeat(64));
    let image = json!([{"Id":config,"RepoDigests":[format!("docker@{}",i.engine_image_digest)]}]);
    observe_engine_image(&serde_json::to_vec(&image).unwrap(), &i, &config).unwrap()
}
#[test]
fn containerd_manifest_image_id_is_bound_to_verified_config() {
    let i = intent();
    let config = format!("sha256:{}", "2".repeat(64));
    let image = json!([{"Id":i.engine_image_digest,"Descriptor":{"digest":i.engine_image_digest,"annotations":{"config.digest":config}}}]);
    let identity = observe_engine_image(&serde_json::to_vec(&image).unwrap(), &i, &config).unwrap();
    let mut container = document();
    container[0]["Image"] = json!(i.engine_image_digest);
    assert!(
        observe_engine(
            &serde_json::to_vec(&container).unwrap(),
            &i,
            OWNER,
            &identity,
            limits()
        )
        .is_ok()
    );
    let mut wrong = image.clone();
    wrong[0]["Descriptor"]["annotations"]["config.digest"] =
        json!(format!("sha256:{}", "9".repeat(64)));
    assert!(observe_engine_image(&serde_json::to_vec(&wrong).unwrap(), &i, &config).is_err());
}
#[test]
fn verifies_real_config_identity_and_complete_private_boundary() {
    let i = intent();
    let d = document();
    let observation = observe_engine(
        &serde_json::to_vec(&d).unwrap(),
        &i,
        OWNER,
        &classic_identity(),
        limits(),
    )
    .unwrap();
    assert_eq!(observation.image_digest, i.engine_image_digest);
    assert_eq!(observation.engine_id, "1".repeat(64));
    assert!(
        observe_engine(
            &serde_json::to_vec(&d).unwrap(),
            &i,
            OWNER,
            &VerifiedEngineImage {
                manifest_digest: i.engine_image_digest.clone(),
                docker_image_id: i.engine_image_digest.clone()
            },
            limits()
        )
        .is_err()
    );
}
#[test]
fn created_container_without_reported_tmpfs_mounts_retains_storage_boundary() {
    let mut d = document();
    d[0]["Mounts"] = json!([]);
    let observe = |d: &Value| {
        observe_engine(
            &serde_json::to_vec(d).unwrap(),
            &intent(),
            OWNER,
            &classic_identity(),
            limits(),
        )
    };
    assert!(
        observe(&d).is_ok(),
        "Docker created container omits tmpfs from Mounts"
    );
    for change in 0..7 {
        let mut wrong = d.clone();
        match change {
            0 => wrong[0]["HostConfig"]["Tmpfs"] = json!({}),
            1 => wrong[0]["HostConfig"]["Tmpfs"]["/var/lib/docker"] = json!("rw,size=1"),
            2 => wrong[0]["HostConfig"]["Binds"] = json!(["/foreign:/foreign"]),
            3 => wrong[0]["HostConfig"]["VolumesFrom"] = json!(["foreign"]),
            4 => wrong[0]["HostConfig"]["Mounts"] = json!([{"Type":"volume","Target":"/foreign"}]),
            5 => wrong[0]["Mounts"] = json!([{"Type":"volume","Destination":"/var/lib/docker"}]),
            _ => wrong[0]["Mounts"] = json!([{"Type":"tmpfs","Destination":"/var/lib/docker"}]),
        }
        assert!(observe(&wrong).is_err(), "{change}");
    }
}
#[test]
fn refuses_host_socket_anonymous_storage_labels_and_unbounded_resources() {
    for change in 0..13 {
        let mut d = document();
        match change {
            0 => d[0]["HostConfig"]["Binds"] = json!(["/var/run/docker.sock:/var/run/docker.sock"]),
            1 => d[0]["Mounts"][0]["Type"] = json!("volume"),
            2 => d[0]["HostConfig"]["Memory"] = json!(0),
            3 => d[0]["HostConfig"]["MemorySwap"] = json!(-1),
            4 => d[0]["HostConfig"]["CpuQuota"] = json!(0),
            5 => d[0]["HostConfig"]["PidsLimit"] = json!(-1),
            6 => d[0]["Config"]["Labels"]["com.zackees.bosn.act.run-id"] = json!("foreign"),
            7 => d[0]["HostConfig"]["PortBindings"] = json!({"2375/tcp":[{"HostPort":"2375"}]}),
            8 => d[0]["Config"]["Env"] = json!(["DOCKER_TLS_CERTDIR=/certs"]),
            9 => d[0]["HostConfig"]["ReadonlyRootfs"] = json!(false),
            10 => d[0]["HostConfig"]["LogConfig"]["Config"]["max-size"] = json!("0"),
            11 => d[0]["HostConfig"]["PidMode"] = json!("host"),
            _ => d[0]["Config"]["Cmd"] = json!(["dockerd", "--host=tcp://0.0.0.0:2375"]),
        }
        assert!(
            observe_engine(
                &serde_json::to_vec(&d).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                limits()
            )
            .is_err(),
            "mutation {change}"
        );
    }
}
#[test]
fn image_entrypoint_cannot_add_an_unverified_tcp_docker_listener() {
    let args = create_arguments(&intent(), OWNER, limits()).unwrap();
    assert!(args.windows(2).any(|p| p == ["--entrypoint", ""]));
    let mut observed = document();
    observed[0]["Config"]["Entrypoint"] = json!(["dockerd-entrypoint.sh"]);
    assert!(
        observe_engine(
            &serde_json::to_vec(&observed).unwrap(),
            &intent(),
            OWNER,
            &classic_identity(),
            limits()
        )
        .is_err()
    );
}
#[test]
fn private_cgroup_init_reparents_and_refuses_unavailable_or_foreign_roots() {
    use std::process::Command;
    let root = std::env::temp_dir().join(format!("bosn-cgroup-init-3345-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let group = root.join("self-cgroup");
    let cgroups = root.join("cgroups");
    std::fs::create_dir(&cgroups).unwrap();
    std::fs::write(&group, "0::/\n").unwrap();
    std::fs::write(cgroups.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
    std::fs::write(cgroups.join("cgroup.procs"), "1\n42\n").unwrap();
    std::fs::write(cgroups.join("cgroup.subtree_control"), "").unwrap();
    // Run only against a synthetic file tree, never the host cgroupfs.
    let script = ENGINE_INIT
        .replace("[ \"$$\" -eq 1 ]", "[ 1 -eq 1 ]")
        .replace("/proc/self/cgroup", group.to_str().unwrap())
        .replace("/sys/fs/cgroup", cgroups.to_str().unwrap())
        .replace("exec docker-init --", "exec");
    let run = || {
        Command::new("sh")
            .args(["-ec", &script, "fixture", "sh", "-c", "printf initialized"])
            .output()
            .unwrap()
    };
    let pid_guard = script.replace("[ 1 -eq 1 ]", "[ \"$$\" -eq 1 ]");
    let refused = Command::new("sh")
        .args([
            "-ec",
            &pid_guard,
            "fixture",
            "sh",
            "-c",
            "printf initialized",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    assert_eq!(run().stdout, b"initialized");
    assert_eq!(
        std::fs::read_to_string(cgroups.join("init/cgroup.procs")).unwrap(),
        "42\n"
    );
    assert_eq!(
        std::fs::read_to_string(cgroups.join("cgroup.subtree_control")).unwrap(),
        " +cpu +memory +pids\n"
    );
    for bad in ["0::/foreign\n", "1:memory:/\n"] {
        std::fs::write(&group, bad).unwrap();
        let result = run();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
    std::fs::write(&group, "0::/\n").unwrap();
    std::fs::write(cgroups.join("cgroup.controllers"), "cpu bad-token\n").unwrap();
    assert!(!run().status.success());
    std::fs::write(cgroups.join("cgroup.controllers"), "cpu memory pids\n").unwrap();
    // A controller write failure must finish after a finite retry budget.
    let blocked = root.join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    let failing = script.replace(
        cgroups.join("cgroup.subtree_control").to_str().unwrap(),
        blocked.to_str().unwrap(),
    );
    let result = Command::new("sh")
        .args(["-ec", &failing, "fixture", "sh", "-c", "printf initialized"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("private cgroup controllers unavailable")
    );
    assert!(!ENGINE_INIT.contains("mount "));
    assert!(!ENGINE_INIT.contains("tcp://"));
    let mut wrong = document();
    wrong[0]["Config"]["Cmd"][2] = json!("exec dockerd --host=tcp://0.0.0.0:2375");
    assert!(
        observe_engine(
            &serde_json::to_vec(&wrong).unwrap(),
            &intent(),
            OWNER,
            &classic_identity(),
            limits()
        )
        .is_err()
    );
    eprintln!("retained cgroup fixture {}", root.display());
}
#[test]
fn overlay_containerd_content_and_runtime_remain_under_bounded_roots() {
    // overlayfs, not native: native copies the whole image into every new
    // snapshot, so each job container cost ~5 s and ~5 GiB of the engine's
    // RAM-backed storage (#323 Step 2 gate measurements).
    let args = create_arguments(&intent(), OWNER, limits()).unwrap();
    let expected = [
        "dockerd",
        "--feature=containerd-snapshotter=true",
        "--storage-driver=overlayfs",
        "--data-root=/var/lib/docker",
        "--exec-root=/run/docker",
        "--host=unix:///var/run/docker.sock",
    ];
    assert_eq!(&args[args.len() - expected.len()..], expected);
    assert!(args.windows(2).any(|p| p
        == [
            "--env",
            "DOCKER_CONTAINERD_ROOT=/var/lib/docker/containerd/daemon"
        ]));
    let mut observed = document();
    observed[0]["Config"]["Env"] = json!([
        "DOCKER_TLS_CERTDIR=",
        "DOCKER_CONTAINERD_ROOT=/var/lib/containerd"
    ]);
    assert!(
        observe_engine(
            &serde_json::to_vec(&observed).unwrap(),
            &intent(),
            OWNER,
            &classic_identity(),
            limits()
        )
        .is_err()
    );
}
#[test]
fn engine_arguments_override_image_volume_without_any_host_bind() {
    let args = create_arguments(&intent(), OWNER, limits()).unwrap();
    assert!(args.windows(2).any(
        |p| p[0] == "--tmpfs" && p[1] == "/var/lib/docker:rw,exec,nosuid,nodev,size=4294967296"
    ));
    assert!(!args.iter().any(|v| matches!(
        v.as_str(),
        "--volume" | "-v" | "--mount" | "--publish" | "-p" | "--rm"
    )));
    assert_eq!(args.last().unwrap(), "--host=unix:///var/run/docker.sock");
    assert!(
        create_arguments(
            &intent(),
            OWNER,
            ActEngineLimits {
                memory_bytes: 1,
                ..limits()
            }
        )
        .is_err()
    );
}

#[test]
fn recovery_uses_frozen_command_while_creation_requires_current_producer() {
    let mut prior = intent();
    let mut prior_command = engine_command();
    prior_command.push("--log-level=warn".into());
    prior.creation_profile.as_mut().unwrap().init_command_sha256 =
        command_digest(&prior_command).unwrap();
    let mut observed = document();
    observed[0]["Config"]["Cmd"] = json!(prior_command);
    observed[0]["Config"]["Labels"] = json!(prior.required_labels(OWNER).unwrap());
    assert!(
        observe_engine(
            &serde_json::to_vec(&observed).unwrap(),
            &prior,
            OWNER,
            &classic_identity(),
            frozen_limits(&prior).unwrap()
        )
        .is_ok()
    );
    assert!(create_arguments(&prior, OWNER, limits()).is_err());
    observed[0]["Config"]["Cmd"] = json!(engine_command());
    assert!(
        observe_engine(
            &serde_json::to_vec(&observed).unwrap(),
            &prior,
            OWNER,
            &classic_identity(),
            frozen_limits(&prior).unwrap()
        )
        .is_err()
    );
}

#[test]
fn legacy_intent_or_changed_creation_limits_never_create_or_observe() {
    let mut legacy = intent();
    legacy.creation_profile = None;
    assert!(create_arguments(&legacy, OWNER, limits()).is_err());
    assert!(
        observe_engine(
            &serde_json::to_vec(&document()).unwrap(),
            &legacy,
            OWNER,
            &classic_identity(),
            limits()
        )
        .is_err()
    );
    let changed = ActEngineLimits {
        memory_bytes: 10 << 30,
        ..limits()
    };
    assert!(create_arguments(&intent(), OWNER, changed).is_err());
    assert!(
        observe_engine(
            &serde_json::to_vec(&document()).unwrap(),
            &intent(),
            OWNER,
            &classic_identity(),
            changed
        )
        .is_err()
    );
    let mut profile_drift = intent();
    profile_drift
        .creation_profile
        .as_mut()
        .unwrap()
        .run_tmpfs_bytes = 32 << 20;
    assert!(create_arguments(&profile_drift, OWNER, limits()).is_err());
}

#[test]
fn snapshot_storage_exec_is_required_but_other_tmpfs_cannot_gain_exec() {
    let declared = limits().tmpfs();
    assert_eq!(
        declared["/var/lib/docker"],
        "rw,exec,nosuid,nodev,size=4294967296"
    );
    for path in ["/run", "/tmp"] {
        assert!(!declared[path].split(',').any(|option| option == "exec"));
    }
    for (path, value) in [
        ("/var/lib/docker", "rw,nosuid,nodev,size=4294967296"),
        ("/run", "rw,exec,nosuid,nodev,size=16777216"),
        ("/tmp", "rw,exec,nosuid,nodev,size=67108864"),
    ] {
        let mut observed = document();
        observed[0]["HostConfig"]["Tmpfs"][path] = json!(value);
        assert!(
            observe_engine(
                &serde_json::to_vec(&observed).unwrap(),
                &intent(),
                OWNER,
                &classic_identity(),
                limits()
            )
            .is_err(),
            "unexpected execution policy at {path}"
        );
    }
}

// One test per fixture mode (#600): nextest runs them in parallel, so the
// 31 s slow-remove case no longer serialises the other five.
#[cfg(target_os = "linux")]
#[test]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal_success() {
    actor_commits_before_create_and_start_and_refuses_uncertain_removal("success");
}

#[cfg(target_os = "linux")]
#[test]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal_slow_remove() {
    actor_commits_before_create_and_start_and_refuses_uncertain_removal("slow-remove");
}

#[cfg(target_os = "linux")]
#[test]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal_foreign() {
    actor_commits_before_create_and_start_and_refuses_uncertain_removal("foreign");
}

#[cfg(target_os = "linux")]
#[test]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal_start_failed() {
    actor_commits_before_create_and_start_and_refuses_uncertain_removal("start-failed");
}

#[cfg(target_os = "linux")]
#[test]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal_probe_failed() {
    actor_commits_before_create_and_start_and_refuses_uncertain_removal("probe-failed");
}

#[cfg(target_os = "linux")]
#[test]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal_still_present() {
    actor_commits_before_create_and_start_and_refuses_uncertain_removal("still-present");
}

#[cfg(target_os = "linux")]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn actor_commits_before_create_and_start_and_refuses_uncertain_removal(mode: &str) {
    use bosn_registry::{
        Registry,
        act::{ActEngineState, ActRunOutcome},
    };
    use kernal_api::{async_engine, platform::fs::TemporaryDirectory};
    // A synthetic CLI reads the actor's real SQLite snapshots before each
    // mutation. This proves ordering, not real Docker or Act execution.
    const SCRIPT: &str = r#"
import json, pathlib, sqlite3, sys, time
db, observation, mode, log = sys.argv[1:5]
args = sys.argv[5:]
with pathlib.Path(log).open('a') as out: out.write(json.dumps(args)+'\n')
record=json.loads(sqlite3.connect('file:'+db+'?mode=ro',uri=True).execute("SELECT detail FROM events WHERE kind LIKE 'act.engine.v1:%' ORDER BY id DESC LIMIT 1").fetchone()[0])
if args[:2]==['image','inspect']:
 print(json.dumps([{'Id':'sha256:'+'2'*64,'RepoDigests':['docker@sha256:'+'e'*64]}]))
elif args[0]=='create':
 assert record['state']=='pending' and record['engine_id'] is None
 print('1'*64)
elif args[:2]==['container','inspect']:
 document=json.loads(pathlib.Path(observation).read_text())
 if mode=='foreign': document[0]['Image']='sha256:'+'9'*64
 print(json.dumps(document))
elif args[:2]==['container','start']:
 assert record['state']=='registered' and record['engine_id']=='1'*64
 if mode=='start-failed': sys.exit(7)
 print('1'*64)
elif args[:2]==['container','rm']:
 assert record['state']=='cleanup_required' and record['engine_id']=='1'*64
 assert args[-1]=='1'*64
 if mode=='slow-remove': time.sleep(31)
 print('1'*64)
elif args[:2]==['container','ls']:
 if mode=='probe-failed': sys.exit(8)
 if mode=='still-present': print('1'*64)
else: sys.exit(9)
"#;
    {
        let dir = TemporaryDirectory::new().unwrap();
        let db = dir.path().join("registry.sqlite3");
        let fixture = dir.path().join("docker.py");
        let observation = dir.path().join("inspect.json");
        let log = dir.path().join("commands.jsonl");
        std::fs::write(&fixture, SCRIPT).unwrap();
        std::fs::write(&observation, serde_json::to_vec(&document()).unwrap()).unwrap();
        let engine = DockerEngine::synthetic_for_test(
            "python3",
            [
                fixture.to_string_lossy().into_owned(),
                db.to_string_lossy().into_owned(),
                observation.to_string_lossy().into_owned(),
                mode.into(),
                log.to_string_lossy().into_owned(),
            ],
        );
        let writer = Registry::create_writer(&db, OWNER).unwrap();
        let runtime = async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, receiver) = async_engine::channel(16);
            let actor = RegistryActor { sender };
            let task = async_engine::launch(crate::registry_actor(writer, receiver, None));
            let created = create_owned_engine(
                &actor,
                &engine,
                intent(),
                OWNER,
                &format!("sha256:{}", "2".repeat(64)),
                limits(),
                1.0,
            )
            .await;
            if matches!(mode, "foreign" | "start-failed") {
                assert!(created.is_err());
            } else {
                let observed = created.unwrap();
                let token = "12345678-1234-4234-8234-123456789abc";
                actor
                    .act_registry(ActRegistryCommand::Claim {
                        intent: intent(),
                        observed: observed.clone(),
                        token: token.into(),
                        at: 2.0,
                    })
                    .await
                    .unwrap();
                actor
                    .act_registry(ActRegistryCommand::Execution {
                        run: intent().run_id,
                        token: token.into(),
                        outcome: ActRunOutcome::Passed,
                        at: 2.0,
                    })
                    .await
                    .unwrap();
                actor
                    .act_registry(ActRegistryCommand::CleanupClaimed {
                        run: intent().run_id,
                        token: token.into(),
                        outcome: ActRunOutcome::Passed,
                        at: 2.0,
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    remove_owned_engine(&actor, &engine, &intent().run_id, observed, 2.0)
                        .await
                        .is_ok(),
                    matches!(mode, "success" | "slow-remove")
                );
            }
            actor.stop().await;
            task.await.unwrap();
        });
        let reader = Registry::open_writer(&db).unwrap();
        let record = reader.act_engine(&intent().run_id).unwrap().unwrap();
        assert_eq!(
            record.state,
            match mode {
                "success" | "slow-remove" => ActEngineState::Terminal,
                "foreign" => ActEngineState::Pending,
                _ => ActEngineState::CleanupRequired,
            }
        );
        let commands = std::fs::read_to_string(log).unwrap();
        if mode == "foreign" {
            for line in commands.lines() {
                let args: Vec<String> = serde_json::from_str(line).unwrap();
                assert!(!args.starts_with(&["container".into(), "start".into()]));
                assert!(!args.starts_with(&["container".into(), "rm".into()]));
            }
        }
        if matches!(mode, "success" | "slow-remove") {
            assert_eq!(commands.lines().count(), 7);
        }
    }
}

/// A disk-backed engine (#425): a budget larger than its whole memory limit.
fn disk_limits() -> ActEngineLimits {
    ActEngineLimits {
        storage_bytes: 64 << 30,
        storage: EngineStorage::Disk,
        ..limits()
    }
}
pub(super) fn disk_intent() -> ActEngineIntent {
    ActEngineIntent {
        creation_profile: Some(creation_profile(disk_limits()).unwrap()),
        ..intent()
    }
}
fn disk_document() -> serde_json::Value {
    let mut d = document();
    let i = disk_intent();
    d[0]["Config"]["Labels"] = json!(i.required_labels(OWNER).unwrap());
    d[0]["HostConfig"]["Tmpfs"] = json!(disk_limits().tmpfs());
    d[0]["HostConfig"]["Mounts"] = json!([{"Type":"volume","Source":i.storage_volume_name().unwrap(),"Target":"/var/lib/docker"}]);
    d[0]["Mounts"] = json!([
        {"Type":"volume","Name":i.storage_volume_name().unwrap(),"Source":format!("/var/lib/docker/volumes/{}/_data",i.storage_volume_name().unwrap()),"Destination":"/var/lib/docker","Driver":"local","Mode":"z","RW":true,"Propagation":""},
        {"Type":"tmpfs","Destination":"/run"},
        {"Type":"tmpfs","Destination":"/tmp"}
    ]);
    d
}
fn observe_disk(d: &Value, i: &ActEngineIntent, l: ActEngineLimits) -> bool {
    observe_engine(
        &serde_json::to_vec(d).unwrap(),
        i,
        OWNER,
        &classic_identity(),
        l,
    )
    .is_ok()
}

#[test]
fn disk_storage_has_an_intent_derived_volume_outside_the_memory_limit() {
    let profile = creation_profile(disk_limits()).unwrap();
    assert_eq!(
        profile.tmpfs_policy,
        ActEngineTmpfsPolicy::NamedDiskStorageRunTmpNoexecV2
    );
    assert!(profile.storage_bytes > profile.memory_bytes);
    assert_eq!(frozen_limits(&disk_intent()).unwrap(), disk_limits());
    // The same budget in RAM cannot fit the memory limit.
    assert!(
        creation_profile(ActEngineLimits {
            storage: EngineStorage::Memory,
            ..disk_limits()
        })
        .is_err()
    );
    let args = create_arguments(&disk_intent(), OWNER, disk_limits()).unwrap();
    assert!(args.windows(2).any(|p| p[0] == "--mount"
        && p[1]
            == format!(
                "type=volume,source={},target=/var/lib/docker",
                disk_intent().storage_volume_name().unwrap()
            )));
    assert!(!args.iter().any(|v| v.starts_with("/var/lib/docker:")));
    for path in ["/run:", "/tmp:"] {
        assert!(
            args.windows(2)
                .any(|p| p[0] == "--tmpfs" && p[1].starts_with(path))
        );
    }
    // A disk intent never creates a memory-backed engine, nor the reverse.
    assert!(create_arguments(&disk_intent(), OWNER, limits()).is_err());
    assert!(create_arguments(&intent(), OWNER, disk_limits()).is_err());
}

#[test]
fn disk_storage_observation_accepts_only_its_own_named_volume() {
    let (i, l) = (disk_intent(), disk_limits());
    assert!(observe_disk(&disk_document(), &i, l));
    // Docker may omit the tmpfs entries, as for a memory-backed engine.
    let mut quiet = disk_document();
    quiet[0]["Mounts"] = json!([quiet[0]["Mounts"][0].clone()]);
    assert!(observe_disk(&quiet, &i, l));
    assert!(!observe_disk(&disk_document(), &intent(), limits()));
    assert!(!observe_disk(&document(), &i, l));
    for change in 0..8 {
        let mut d = disk_document();
        match change {
            0 => d[0]["Mounts"][0]["Name"] = json!("foreign"),
            1 => d[0]["Mounts"][0]["RW"] = json!(false),
            2 => d[0]["Mounts"][0]["Destination"] = json!("/foreign"),
            3 => d[0]["HostConfig"]["Mounts"][0]["Source"] = json!("foreign"),
            4 => d[0]["HostConfig"]["Mounts"][0]["ReadOnly"] = json!(true),
            5 => d[0]["HostConfig"]["Mounts"] = json!([]),
            6 => {
                d[0]["HostConfig"]["Tmpfs"]["/var/lib/docker"] =
                    json!("rw,exec,nosuid,nodev,size=68719476736")
            }
            _ => {
                let extra = d[0]["Mounts"][0].clone();
                d[0]["Mounts"].as_array_mut().unwrap().push(extra);
            }
        }
        assert!(!observe_disk(&d, &i, l), "{change}");
    }
}

#[test]
fn legacy_anonymous_disk_profiles_can_be_observed_but_not_newly_created() {
    let mut i = disk_intent();
    i.creation_profile.as_mut().unwrap().tmpfs_policy =
        ActEngineTmpfsPolicy::DiskStorageRunTmpNoexecV1;
    let mut d = disk_document();
    d[0]["Config"]["Labels"] = json!(i.required_labels(OWNER).unwrap());
    d[0]["HostConfig"]["Mounts"] = json!([{"Type":"volume","Target":"/var/lib/docker"}]);
    d[0]["Mounts"][0]["Name"] = json!("9".repeat(64));
    d[0]["Mounts"][0]["Source"] =
        json!(format!("/var/lib/docker/volumes/{}/_data", "9".repeat(64)));
    assert!(observe_disk(&d, &i, disk_limits()));
    assert!(i.storage_volume_name().is_none());
    assert!(create_arguments(&i, OWNER, disk_limits()).is_err());
}
