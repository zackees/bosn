//! Unit tests for engine recovery and loaded-image verification.

use super::*;
#[test]
fn immutable_timestamp_roundtrip_is_exact() {
    for offset in 1..100000_u64 {
        let timestamp = f64::from_bits(1790892332.0_f64.to_bits() + offset);
        let encoded = serde_json::to_string(&timestamp).unwrap();
        let decoded: f64 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            timestamp.to_bits(),
            decoded.to_bits(),
            "timestamp {encoded}"
        );
    }
}
#[test]
fn imported_manifest_and_config_are_separate_from_docker_image_id() {
    let config_bytes = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Volumes":{},"Env":["PATH=/usr/bin:/bin"],"Entrypoint":["/bin/sh"],"Cmd":["-c","true"],"User":"root","WorkingDir":"/tmp"},"rootfs":{"type":"layers","diff_ids":[format!("sha256:{}","c".repeat(64))]}})).unwrap();
    let config = hash(&config_bytes);
    let manifest_bytes = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":config,"size":config_bytes.len(),"mediaType":"application/vnd.oci.image.config.v1+json"}})).unwrap();
    let manifest = hash(&manifest_bytes);
    let expected: Value = serde_json::from_slice(&config_bytes).unwrap();
    let observation = json!([{"Id":manifest,"Descriptor":{"digest":manifest,"mediaType":"application/vnd.oci.image.manifest.v1+json","size":manifest_bytes.len()},"RootFS":{"Type":"layers","Layers":expected["rootfs"]["diff_ids"]},"Config":expected["config"]}]);
    let verify = |v: &Value, m: &[u8], c: &[u8]| {
        verify_loaded_image(&serde_json::to_vec(v).unwrap(), &manifest, &config, m, c)
    };
    assert_eq!(
        verify(&observation, &manifest_bytes, &config_bytes).unwrap(),
        manifest
    );
    for field in [
        "manifest",
        "size",
        "mediaType",
        "layers",
        "volumes",
        "Env",
        "Entrypoint",
        "Cmd",
        "User",
        "WorkingDir",
    ] {
        let mut bad = observation.clone();
        match field {
            "manifest" => bad[0]["Descriptor"]["digest"] = json!(config),
            "size" => bad[0]["Descriptor"]["size"] = json!(0),
            "mediaType" => bad[0]["Descriptor"]["mediaType"] = json!("unknown"),
            "layers" => bad[0]["RootFS"]["Layers"] = json!([]),
            "volumes" => bad[0]["Config"]["Volumes"] = json!({"/data":{}}),
            "Env" | "Entrypoint" | "Cmd" => bad[0]["Config"][field] = json!([]),
            _ => bad[0]["Config"][field] = json!("foreign"),
        };
        assert!(verify(&bad, &manifest_bytes, &config_bytes).is_err());
    }
    assert!(verify(&observation, b"{}", &config_bytes).is_err());
    assert!(verify(&observation, &manifest_bytes, b"{}").is_err());
}

#[cfg(target_os = "linux")]
mod startup_recovery {
    use super::*;
    use bosn_registry::{
        Registry,
        act::{ActEngineIntent, ActEngineObservation, ActEngineState, ActRunOutcome},
    };

    #[test]
    fn startup_recovery_cannot_succeed_without_seal() {
        let runtime = async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, receiver) = async_engine::channel(1);
            drop(receiver);
            let actor = RegistryActor { sender };
            let engine = DockerEngine::synthetic_for_test("never-execute", Vec::<String>::new());
            let cancellation = async_engine::CancellationSource::new();
            let failure = recover_startup_act_engines(
                &actor,
                &engine,
                "owner",
                &BTreeMap::new(),
                ActStartupRecoveryOptions {
                    page_size: 1,
                    max_runs: 1,
                    deadline: Duration::from_secs(10),
                },
                &cancellation.token(),
            )
            .await
            .unwrap_err();
            assert!(failure.to_string().contains("seal failed"));
        });
    }

    #[test]
    fn startup_recovery_absence_faults_pages_and_seal() {
        const OWNER: &str = "11111111-2222-4333-8444-555555555555";
        for mode in [
            "absent",
            "failed-list",
            "foreign",
            "missing-proof",
            "legacy",
            "deadline",
            "dropped",
            "ceiling",
            "cancelled",
            "invalid",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("registry.sqlite3");
            let mut registry = Registry::create_writer(&db, OWNER).unwrap();
            let proofs = crate::act_engine::bundled_engine_manifests().unwrap();
            let pin = proofs.keys().next().unwrap().clone();
            let mut intents = Vec::new();
            for n in 1..=3 {
                let intent = ActEngineIntent {
                    run_id: format!("00000000-0000-4000-8000-{n:012}"),
                    workspace: "/private/source".into(),
                    candidate_sha: "a".repeat(40),
                    payload_sha256: "b".repeat(64),
                    snapshot_sha256: "c".repeat(64),
                    act_version: "0.2.88".into(),
                    act_image_digest: format!("sha256:{}", "d".repeat(64)),
                    engine_image_digest: pin.clone(),
                    runner_image_digest: format!("sha256:{}", "f".repeat(64)),
                    created_at: 1.0,
                    spare: false,
                    creation_profile: Some(
                        crate::act_engine::creation_profile(crate::act_engine::ActEngineLimits {
                            memory_bytes: 6 << 30,
                            storage_bytes: 4 << 30,
                            nano_cpus: 1_000_000_000,
                            pids: 256,
                        })
                        .unwrap(),
                    ),
                };
                let mut tx = registry.begin_immediate().unwrap();
                tx.begin_act_engine(&intent).unwrap();
                if n == 2 {
                    let observed = ActEngineObservation {
                        name: intent.engine_name(),
                        engine_id: "1".repeat(64),
                        image_digest: pin.clone(),
                        labels: intent.required_labels(OWNER).unwrap(),
                    };
                    tx.register_act_engine(&intent.run_id, &observed, 2.0)
                        .unwrap();
                    tx.claim_act_execution(
                        &intent,
                        &observed,
                        "12345678-1234-4234-8234-123456789abc",
                        3.0,
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
                intents.push(intent);
            }
            if mode == "legacy" {
                for intent in &intents {
                    let mut old =
                        serde_json::to_value(registry.act_engine(&intent.run_id).unwrap().unwrap())
                            .unwrap();
                    old["schema_version"] = json!(2);
                    old["intent"]
                        .as_object_mut()
                        .unwrap()
                        .remove("creation_profile");
                    let mut tx = registry.begin_immediate().unwrap();
                    tx.append_event(
                        3.0,
                        &format!("act.engine.v1:{}", intent.run_id),
                        &serde_json::to_string(&old).unwrap(),
                    )
                    .unwrap();
                    tx.commit().unwrap();
                }
            }
            let script = dir.path().join("engine.py");
            let log = dir.path().join("commands");
            std::fs::write(
                &script,
                r#"import sys,json
mode,log,db=sys.argv[1:4];args=sys.argv[4:]
import sqlite3
filter=args[args.index('--filter')+1]
if filter.startswith('name=^/bosn-act-'):
 run=filter[len('name=^/bosn-act-'):-1]
 with sqlite3.connect(db) as conn:
  record=json.loads(conn.execute('SELECT detail FROM events WHERE kind=? ORDER BY id DESC LIMIT 1',('act.engine.v1:'+run,)).fetchone()[0])
 assert record['state']=='cleanup_required' and record['outcome']=='interrupted'
 assert record['execution']!='passed'
with open(log,'a') as f:f.write(json.dumps(args)+'\n')
assert args[:4]==['container','ls','--all','--no-trunc']
if mode=='failed-list':sys.exit(7)
if mode=='deadline':
 import time;time.sleep(3)
if mode=='foreign':print('f'*64)
"#,
            )
            .unwrap();
            let engine = DockerEngine::synthetic_for_test(
                "uv",
                [
                    "run".into(),
                    "--no-project".into(),
                    "python".into(),
                    script.to_string_lossy().into_owned(),
                    mode.into(),
                    log.to_string_lossy().into_owned(),
                    db.to_string_lossy().into_owned(),
                ],
            );
            let runtime = async_engine::RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.run(async {
                let (sender, receiver) = async_engine::channel(16);
                let actor = RegistryActor { sender };
                let task = async_engine::launch(async move {
                    if mode == "dropped" {
                        async_engine::sleep(Duration::from_millis(200)).await;
                    }
                    crate::registry_actor(registry, receiver, None).await
                });
                let cancel = async_engine::CancellationSource::new();
                if mode == "cancelled" {
                    cancel.cancel();
                }
                let selected = if mode == "missing-proof" {
                    BTreeMap::new()
                } else {
                    proofs
                };
                let token = cancel.token();
                let recovery = recover_startup_act_engines(
                    &actor,
                    &engine,
                    OWNER,
                    &selected,
                    ActStartupRecoveryOptions {
                        page_size: if mode == "invalid" { 0 } else { 1 },
                        max_runs: if mode == "ceiling" { 1 } else { 3 },
                        deadline: Duration::from_secs(if mode == "deadline" { 6 } else { 20 }),
                    },
                    &token,
                );
                let result = if mode == "dropped" {
                    let timed = async_engine::timeout(Duration::from_millis(20), recovery).await;
                    assert!(timed.is_err());
                    async_engine::sleep(Duration::from_millis(300)).await;
                    Err(error("caller dropped"))
                } else {
                    recovery.await
                };
                if matches!(
                    mode,
                    "ceiling" | "cancelled" | "invalid" | "deadline" | "dropped"
                ) {
                    assert!(result.is_err(), "{mode}");
                } else {
                    let report = result.unwrap();
                    assert_eq!(report.runs.len(), 3);
                    assert_eq!(
                        report.runs.iter().all(|r| r.engine_removed),
                        mode == "absent"
                    );
                    assert_eq!(
                        report.runs.iter().all(|r| r.deferred_reason.is_some()),
                        mode != "absent"
                    );
                }
                assert!(
                    actor
                        .act_registry(ActRegistryCommand::StartupInterrupt {
                            run: intents[0].run_id.clone(),
                            at: now()
                        })
                        .await
                        .is_err()
                );
                actor.stop().await;
                task.await.unwrap();
            });
            let reopened = Registry::open_writer(&db).unwrap();
            for intent in &intents {
                let record = reopened.act_engine(&intent.run_id).unwrap().unwrap();
                if mode == "absent" {
                    assert_eq!(record.state, ActEngineState::Terminal);
                    assert_eq!(record.outcome, Some(ActRunOutcome::Interrupted));
                    assert_ne!(record.execution, Some(ActRunOutcome::Passed));
                } else if !matches!(
                    mode,
                    "cancelled" | "invalid" | "ceiling" | "deadline" | "dropped"
                ) {
                    assert_eq!(record.state, ActEngineState::CleanupRequired);
                    assert!(record.removal.is_none());
                }
            }
            if matches!(
                mode,
                "missing-proof" | "legacy" | "cancelled" | "invalid" | "dropped"
            ) {
                assert!(!log.exists());
            }
            if mode == "deadline" {
                assert_eq!(
                    reopened
                        .act_engine(&intents[0].run_id)
                        .unwrap()
                        .unwrap()
                        .state,
                    ActEngineState::CleanupRequired
                );
            }
            if mode == "absent" {
                let commands = std::fs::read_to_string(log).unwrap();
                assert_eq!(commands.lines().count(), 4);
                assert!(commands.contains("id=111111"));
            }
        }
    }
}
