//! Unit tests for the Act runtime.

use super::*;
#[test]
fn real_actor_transport_failures_persist_results_and_never_retire_uncertain_absence() {
    use bosn_registry::{Registry, act::ActEngineState};
    const OWNER: &str = "11111111-2222-4333-8444-555555555555";
    const SCRIPT: &str = r#"import sys,json,sqlite3,time
db,image,mode,log=sys.argv[1:5];args=sys.argv[5:]
with open(log,'a') as f: f.write(json.dumps(args)+'\n')
con=sqlite3.connect(db)
record=json.loads(con.execute("select detail from events where kind like 'act.engine.v1:%' order by id desc limit 1").fetchone()[0])
if args[:2]==['container','rm']:
 assert record['state']=='cleanup_required' and args[-1]==record['engine_id']
 print(record['engine_id'])
elif args[:2]==['container','ls']:
 if mode=='probe-failed': sys.exit(8)
elif args[:4]==['exec','-i',record['engine_id'],'sh']:
 assert record['state']=='registered' and args[-1].startswith('/var/lib/docker/bosn-inputs/')
 assert args[-2]=='bosn-input' and args[4]=='-c' and args[5]=='umask 077; cat > \"$1\"'
 if mode=='copy-failed': sys.exit(7)
 import hashlib
 from pathlib import Path
 received=Path(db).parent/'received';received.mkdir(exist_ok=True)
 digest=hashlib.sha256()
 while True:
  chunk=sys.stdin.buffer.read(8192)
  if not chunk: break
  digest.update(chunk)
 (received/Path(args[-1]).name).write_text(digest.hexdigest())
elif args[:4]==['exec',record['engine_id'],'docker','info']:
 if mode=='readiness-timeout': sys.exit(8)
 value={'DockerRootDir':'/var/lib/docker','Driver':'native','ServerVersion':'29.7.2','OSType':'linux','Architecture':'x86_64','DriverStatus':[['driver-type','io.containerd.snapshotter.v1']]}
 if mode=='foreign-storage': value['DockerRootDir']='/var/lib/containerd'
 print(json.dumps(value))
elif args[:3]==['exec',record['engine_id'],'sha256sum']:
 import hashlib
 from pathlib import Path
 received=Path(db).parent/'received'
 for path in args[3:]:
  digest=(received/Path(path).name).read_text()
  if mode=='copy-corrupt': digest='f'*64
  print(digest+'  '+path)
elif args[:4]==['exec',record['engine_id'],'docker','load']:
 if mode=='load-failed': sys.exit(7)
elif args[:5]==['exec',record['engine_id'],'docker','image','inspect']:
 images=json.load(open(image));is_runner=args[-1]==record['intent']['runner_image_digest']
 assert args[-1]==images['runner' if is_runner else 'act'][0]['Id']
 value=images['runner' if is_runner else 'act']
 if mode=='missing-runner' and is_runner: sys.exit(7)
 if mode=='foreign-image': value[0]['Descriptor']['digest']='sha256:'+'f'*64
 print(json.dumps(value))
elif args[:4]==['exec',record['engine_id'],'docker','run']:
 if mode=='driver-failed':
  print('named Docker cgroup failure',file=sys.stderr);sys.exit(125)
 if mode=='driver-job-failed':
  print(json.dumps({'jobID':'lint','job':'CI/lint','matrix':{},'jobResult':'failure'}))
  print('startup debug '*1024+'named Docker storage failure',file=sys.stderr);sys.exit(1)
 assert record['state']=='registered'
 assert '--network=bridge' in args and '--read-only' in args and '-W' in args and args[args.index('-W')+1].endswith('/source/.github/workflows')
 assert not any('GITHUB_TOKEN' in x for x in args)
 expected=json.load(open(image))['runner'][0]['Id']
 assert 'ubuntu-latest='+expected in args and 'ubuntu-22.04='+expected in args
 if mode in ['timeout','dropped','duplicate']: time.sleep(4)
 print(json.dumps({'jobID':'lint','job':'CI/lint','matrix':{},'jobResult':'success'}))
 if mode=='unsupported': print(json.dumps({'jobID':'mac','job':'CI/mac','matrix':{},'msg':'Skipping unsupported platform macos-latest'}))
elif args[:3]==['exec',record['engine_id'],'mkdir'] or args[:3]==['exec',record['engine_id'],'tar']: pass
else: raise Exception('unexpected args '+repr(args))
"#;
    for mode in [
        "success",
        "copy-failed",
        "copy-corrupt",
        "load-failed",
        "driver-failed",
        "driver-job-failed",
        "foreign-image",
        "missing-runner",
        "unsupported",
        "timeout",
        "probe-failed",
        "cancelled",
        "foreign-storage",
        "readiness-timeout",
        "dropped",
        "duplicate",
        "claim-race",
        "claim-drop",
        "loser-claim-drop",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(source.join(".github/workflows")).unwrap();
        std::fs::write(
            source.join(".github/workflows/ci.yml"),
            "name: CI\non: push\njobs: {}\n",
        )
        .unwrap();
        let snapshot_path = dir.path().join("snapshot.tar");
        assert!(
            std::process::Command::new("tar")
                .args(["--format=ustar", "--owner=0", "--group=0", "-cf"])
                .arg(&snapshot_path)
                .arg("-C")
                .arg(&source)
                .arg(".github/workflows/ci.yml")
                .status()
                .unwrap()
                .success()
        );
        let snapshot = std::fs::read(&snapshot_path).unwrap();
        let base_layer = b"synthetic verified base layer";
        let base_layer_digest = hash(base_layer);
        let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[base_layer_digest]}})).unwrap();
        let base = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":base_layer_digest,"size":base_layer.len()}]})).unwrap();
        let package = crate::act_image::package_act_image(
            &base,
            &hash(&base),
            &config,
            &hash(&config),
            b"synthetic Act",
            &hash(b"synthetic Act"),
            "0.2.88",
        )
        .unwrap();
        let candidate = "a".repeat(40);
        let payload = serde_json::to_vec(&json!({"after":candidate})).unwrap();
        let intent = ActEngineIntent {
            run_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            workspace: source.to_string_lossy().into_owned(),
            candidate_sha: candidate,
            payload_sha256: hash(&payload)[7..].into(),
            snapshot_sha256: hash(&snapshot)[7..].into(),
            act_version: "0.2.88".into(),
            act_image_digest: package.manifest_digest.clone(),
            engine_image_digest: format!("sha256:{}", "e".repeat(64)),
            runner_image_digest: hash(&base),
            creation_profile: Some(
                crate::act_engine::creation_profile(crate::act_engine::ActEngineLimits {
                    memory_bytes: 6 << 30,
                    storage_bytes: 4 << 30,
                    nano_cpus: 1_000_000_000,
                    pids: 256,
                })
                .unwrap(),
            ),
            created_at: now().floor(),
        };
        let observed = ActEngineObservation {
            name: intent.engine_name(),
            engine_id: "1".repeat(64),
            image_digest: intent.engine_image_digest.clone(),
            labels: intent.required_labels(OWNER).unwrap(),
        };
        let db = dir.path().join("registry.sqlite3");
        let mut registry = Registry::create_writer(&db, OWNER).unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.begin_act_engine(&intent).unwrap();
        transaction
            .register_act_engine(&intent.run_id, &observed, now())
            .unwrap_or_else(|e| panic!("{mode}: registration {e:?}"));
        if mode == "loser-claim-drop" {
            transaction
                .claim_act_execution(
                    &intent,
                    &observed,
                    "12345678-1234-4234-8234-123456789abc",
                    now(),
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        let image_config: Value = serde_json::from_slice(&package.config).unwrap();
        let runner_config: Value = serde_json::from_slice(&package.runner_config).unwrap();
        let image = json!({"act":[{"Id":package.manifest_digest,"Descriptor":{"digest":package.manifest_digest,"mediaType":serde_json::from_slice::<Value>(&package.manifest).unwrap()["mediaType"],"size":package.manifest.len()},"RootFS":{"Type":"layers","Layers":image_config["rootfs"]["diff_ids"]},"Config":image_config["config"]}],"runner":[{"Id":package.runner_manifest_digest,"Descriptor":{"digest":package.runner_manifest_digest,"mediaType":serde_json::from_slice::<Value>(&package.runner_manifest).unwrap()["mediaType"],"size":package.runner_manifest.len()},"RootFS":{"Type":"layers","Layers":runner_config["rootfs"]["diff_ids"]},"Config":runner_config["config"]}]});
        let image_path = dir.path().join("image.json");
        std::fs::write(&image_path, serde_json::to_vec(&image).unwrap()).unwrap();
        let script = dir.path().join("docker.py");
        std::fs::write(&script, SCRIPT).unwrap();
        let commands = dir.path().join("commands.jsonl");
        let engine = DockerEngine::synthetic_for_test(
            "uv",
            [
                "run".into(),
                "--no-project".into(),
                "python".into(),
                script.to_string_lossy().into_owned(),
                db.to_string_lossy().into_owned(),
                image_path.to_string_lossy().into_owned(),
                mode.into(),
                commands.to_string_lossy().into_owned(),
            ],
        );
        let evidence = dir.path().join("evidence");
        std::fs::create_dir(&evidence).unwrap();
        let runtime = async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, receiver) = async_engine::channel(16);
            let actor = RegistryActor { sender };
            let task = async_engine::launch(async move {
                if matches!(mode, "claim-drop" | "loser-claim-drop") {
                    async_engine::sleep(Duration::from_millis(250)).await;
                }
                crate::registry_actor(registry, receiver, None).await
            });
            if mode == "claim-race" {
                let contenders = [
                    "12345678-1234-4234-8234-123456789abc",
                    "98765432-1234-4234-8234-123456789abc",
                ];
                let mut claims = Vec::new();
                for token in contenders {
                    let actor = actor.clone();
                    let intent = intent.clone();
                    let observed = observed.clone();
                    claims.push(async_engine::launch(async move {
                        actor
                            .act_registry(ActRegistryCommand::Claim {
                                intent,
                                observed,
                                token: token.into(),
                                at: now(),
                            })
                            .await
                    }));
                }
                let mut owner = None;
                for claim in claims {
                    if let Ok(ActRegistryReply::Claimed(record)) = claim.await.unwrap() {
                        assert!(owner.is_none());
                        owner = record.execution_claim;
                    }
                }
                let owner = owner.expect("one atomic winner");
                assert!(
                    actor
                        .act_registry(ActRegistryCommand::Cleanup {
                            run: intent.run_id.clone(),
                            outcome: ActRunOutcome::Interrupted,
                            at: now()
                        })
                        .await
                        .is_err()
                );
                assert!(!commands.exists());
                actor
                    .act_registry(ActRegistryCommand::CleanupClaimed {
                        run: intent.run_id.clone(),
                        token: owner,
                        outcome: ActRunOutcome::Interrupted,
                        at: now(),
                    })
                    .await
                    .unwrap();
                crate::act_engine::remove_owned_engine(
                    &actor,
                    &engine,
                    &intent.run_id,
                    observed.clone(),
                    now(),
                )
                .await
                .unwrap();
                actor.stop().await;
                task.await.unwrap();
                return;
            }
            let cancellation_source = async_engine::CancellationSource::new();
            let cancellation = cancellation_source.token();
            if mode == "cancelled" {
                cancellation_source.cancel();
            }
            let base_blobs = [ActArchiveBlob {
                digest: &base_layer_digest,
                bytes: base_layer,
            }];
            let execution = run_registered_act(
                &actor,
                &engine,
                ActRuntimeRequest {
                    intent: &intent,
                    observed: &observed,
                    package: &package,
                    base_blobs: &base_blobs,
                    source_archive: &snapshot,
                    event_payload: &payload,
                    event_name: "push",
                    evidence_root: &evidence,
                    archive_ceiling: 1 << 20,
                    execution_deadline: Duration::from_secs(
                        if matches!(mode, "timeout" | "readiness-timeout") {
                            2
                        } else {
                            10
                        },
                    ),
                    output_ceiling: 1 << 20,
                },
                &cancellation,
            );
            if mode == "duplicate" {
                let mut first = Box::pin(execution);
                assert!(
                    async_engine::timeout(Duration::from_millis(500), &mut first)
                        .await
                        .is_err()
                );
                let ActRegistryReply::Recovery(before) = actor
                    .act_registry(ActRegistryCommand::Pending {
                        after_run_id: None,
                        limit: 16,
                    })
                    .await
                    .unwrap()
                else {
                    panic!("recovery reply");
                };
                let commands_before = std::fs::read(&commands).unwrap();
                let receipt_before =
                    std::fs::read(evidence.join(&intent.run_id).join("intent.json")).unwrap();
                let duplicate = run_registered_act(
                    &actor,
                    &engine,
                    ActRuntimeRequest {
                        intent: &intent,
                        observed: &observed,
                        package: &package,
                        base_blobs: &base_blobs,
                        source_archive: &snapshot,
                        event_payload: &payload,
                        event_name: "push",
                        evidence_root: &evidence,
                        archive_ceiling: 1 << 20,
                        execution_deadline: Duration::from_secs(10),
                        output_ceiling: 1 << 20,
                    },
                    &cancellation,
                )
                .await;
                assert!(
                    duplicate.is_err(),
                    "duplicate caller mutated execution: {duplicate:?}"
                );
                let ActRegistryReply::Recovery(page) = actor
                    .act_registry(ActRegistryCommand::Pending {
                        after_run_id: None,
                        limit: 16,
                    })
                    .await
                    .unwrap()
                else {
                    panic!("recovery reply");
                };
                assert_eq!(page.items[0].state, ActEngineState::Registered);
                assert_eq!(page.items, before.items);
                assert_eq!(std::fs::read(&commands).unwrap(), commands_before);
                assert_eq!(
                    std::fs::read(evidence.join(&intent.run_id).join("intent.json")).unwrap(),
                    receipt_before
                );
                assert!(
                    !std::fs::read_to_string(&commands)
                        .unwrap()
                        .contains("container\", \"rm")
                );
                drop(first);
                for _ in 0..100 {
                    let ActRegistryReply::Recovery(page) = actor
                        .act_registry(ActRegistryCommand::Pending {
                            after_run_id: None,
                            limit: 16,
                        })
                        .await
                        .unwrap()
                    else {
                        panic!("recovery reply");
                    };
                    if page.items.is_empty() {
                        break;
                    }
                    async_engine::sleep(Duration::from_millis(20)).await;
                }
                actor.stop().await;
                task.await.unwrap();
                return;
            }
            if mode == "loser-claim-drop" {
                assert!(
                    async_engine::timeout(Duration::from_millis(100), execution)
                        .await
                        .is_err()
                );
                async_engine::sleep(Duration::from_millis(300)).await;
                let ActRegistryReply::Recovery(page) = actor
                    .act_registry(ActRegistryCommand::Pending {
                        after_run_id: None,
                        limit: 16,
                    })
                    .await
                    .unwrap()
                else {
                    panic!("recovery reply");
                };
                assert_eq!(page.items[0].state, ActEngineState::Registered);
                assert_eq!(
                    page.items[0].execution_claim.as_deref(),
                    Some("12345678-1234-4234-8234-123456789abc")
                );
                assert!(!commands.exists());
                assert_eq!(std::fs::read_dir(&evidence).unwrap().count(), 0);
                actor
                    .act_registry(ActRegistryCommand::CleanupClaimed {
                        run: intent.run_id.clone(),
                        token: "12345678-1234-4234-8234-123456789abc".into(),
                        outcome: ActRunOutcome::Interrupted,
                        at: now(),
                    })
                    .await
                    .unwrap();
                crate::act_engine::remove_owned_engine(
                    &actor,
                    &engine,
                    &intent.run_id,
                    observed.clone(),
                    now(),
                )
                .await
                .unwrap();
                actor.stop().await;
                task.await.unwrap();
                return;
            }
            if matches!(mode, "dropped" | "claim-drop") {
                assert!(
                    async_engine::timeout(
                        Duration::from_millis(if mode == "claim-drop" { 100 } else { 500 }),
                        execution
                    )
                    .await
                    .is_err()
                );
                for _ in 0..100 {
                    let ActRegistryReply::Recovery(page) = actor
                        .act_registry(ActRegistryCommand::Pending {
                            after_run_id: None,
                            limit: 16,
                        })
                        .await
                        .unwrap()
                    else {
                        panic!("recovery reply");
                    };
                    if page.items.is_empty() {
                        break;
                    }
                    async_engine::sleep(Duration::from_millis(20)).await;
                }
                actor.stop().await;
                task.await.unwrap();
                return;
            }
            let report = execution.await.unwrap();
            assert_eq!(
                report.execution_success,
                matches!(mode, "success" | "probe-failed"),
                "{mode}: {:?}",
                report.failure
            );
            if mode == "driver-failed" {
                assert!(
                    report
                        .failure
                        .as_deref()
                        .unwrap()
                        .contains("Act driver exited 125: named Docker cgroup failure")
                );
                assert!(report.jobs.is_empty());
            }
            if mode == "driver-job-failed" {
                let failure = report.failure.as_deref().unwrap();
                assert!(failure.contains("named Docker storage failure"));
                assert!(failure.len() < 2200);
                assert_eq!(report.jobs.len(), 1);
                assert_eq!(report.jobs[0].outcome, "failure");
            }
            assert_eq!(report.engine_removed, mode != "probe-failed", "{mode}");
            let persisted: Value = serde_json::from_slice(
                &std::fs::read(evidence.join(&intent.run_id).join("result.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(persisted["engine_removed"], report.engine_removed);
            if mode == "unsupported" {
                assert!(report.jobs.iter().any(|j| j.outcome == "unsupported"));
            }
            actor.stop().await;
            task.await.unwrap();
        });
        let registry = Registry::open_writer(&db).unwrap();
        let record = registry.act_engine(&intent.run_id).unwrap().unwrap();
        assert_eq!(
            record.state,
            if mode == "probe-failed" {
                ActEngineState::CleanupRequired
            } else {
                ActEngineState::Terminal
            }
        );
        assert!(
            std::fs::read_to_string(commands)
                .unwrap()
                .contains("container\", \"rm")
        );
    }
}
#[test]
fn snapshot_admits_system_ustar_and_refuses_extensions_links_and_traversal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
    std::fs::write(dir.path().join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    let archive = dir.path().join("source.tar");
    let build = |format: &str| {
        assert!(
            std::process::Command::new("tar")
                .args(["--format", format, "--owner=0", "--group=0", "-cf"])
                .arg(&archive)
                .arg("-C")
                .arg(dir.path())
                .arg(".github/workflows/ci.yml")
                .status()
                .unwrap()
                .success()
        );
        std::fs::read(&archive).unwrap()
    };
    let valid = build("ustar");
    verify_snapshot(&valid).unwrap();
    assert!(verify_snapshot(&build("pax")).is_err());
    for (name, kind) in [
        ("../escape.yml", b'0'),
        (".github/workflows/ci.yml", b'2'),
        ("/absolute.yml", b'0'),
    ] {
        let mut corrupted = valid.clone();
        corrupted[..100].fill(0);
        corrupted[..name.len()].copy_from_slice(name.as_bytes());
        corrupted[156] = kind;
        corrupted[148..156].fill(b' ');
        let sum: usize = corrupted[..512].iter().map(|b| usize::from(*b)).sum();
        corrupted[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        assert!(verify_snapshot(&corrupted).is_err(), "{name}/{kind}");
    }
    let mut checksum = valid.clone();
    checksum[0] ^= 1;
    assert!(verify_snapshot(&checksum).is_err());
    assert!(verify_snapshot(&valid[..512]).is_err());
}
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

#[test]
fn results_do_not_promote_unknown_or_foreign_jobs() {
    let data=b"{\"jobID\":\"lint\",\"job\":\"lint\",\"matrix\":{},\"jobResult\":\"success\"}\n{\"jobID\":\"mac\",\"job\":\"mac\",\"matrix\":{},\"msg\":\"Skipping unsupported platform macos-latest\"}\n";
    let jobs = parse_job_results(data).unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().any(|v| v.outcome == "unsupported"));
    assert!(parse_job_results(b"not JSON\n").is_err());
    let missing = parse_job_results(
        b"{\"jobID\":\"unknown\",\"job\":\"unknown\",\"matrix\":{},\"msg\":\"started\"}\n",
    )
    .unwrap();
    assert_eq!(missing[0].outcome, "incomplete");
}

#[cfg(target_os = "linux")]
mod startup_recovery {
    use super::*;
    use bosn_registry::{Registry, act::ActEngineState};

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
