//! The live probe cases (ignored: they need reviewed pinned inputs and Docker).

use super::*;

pub(super) async fn probe_case(
    registry: &RegistryActor,
    engine: &DockerEngine,
    images: (&ActImagePackage, &VerifiedEngineManifest),
    blobs: &[ActArchiveBlob<'_>],
    root: &Path,
    owner: &str,
    cancel_case: bool,
) -> std::io::Result<Value> {
    let (package, engine_manifest) = images;
    let (source, snapshot, payload, sha) = source_fixture(root, cancel_case)?;
    let run = random_uuid().await?;
    let intent = ActEngineIntent {
        run_id: run.clone(),
        workspace: source.to_string_lossy().into_owned(),
        candidate_sha: sha,
        payload_sha256: digest(&payload)[7..].into(),
        snapshot_sha256: digest(&snapshot)[7..].into(),
        act_version: "0.2.88".into(),
        act_image_digest: package.manifest_digest.clone(),
        engine_image_digest: ENGINE.into(),
        runner_image_digest: RUNNER.into(),
        creation_profile: Some(
            crate::act_engine::creation_profile(limits())
                .map_err(|error| fail(error.to_string()))?,
        ),
        created_at: at(),
    };
    retain(
        &root.join(format!("{run}-intent.json")),
        &serde_json::to_vec_pretty(&intent)?,
    )?;
    let profile = limits();
    retain(
        &root.join(format!("{run}-profile.json")),
        &serde_json::to_vec_pretty(
            &json!({"memory_bytes":profile.memory_bytes,"storage_bytes":profile.storage_bytes,"nano_cpus":profile.nano_cpus,"pids":profile.pids,"basis":"inferred expanded layer/native-copy demand; not validated minimum"}),
        )?,
    )?;
    let cancellation = CancellationSource::new();
    let mut observer = ProbeObserver::new();
    let result = match async_engine::timeout(Duration::from_secs(120), async {
        let observed = create_owned_engine_from_manifest(
            registry,
            engine,
            intent.clone(),
            owner,
            engine_manifest,
            limits(),
            at(),
        )
        .await
        .map_err(|e| fail(e.to_string()))?;
        let raw = docker(
            engine,
            vec![
                "container".into(),
                "inspect".into(),
                observed.engine_id.clone(),
            ],
        )
        .await?;
        let host_image = docker(
            engine,
            vec![
                "image".into(),
                "inspect".into(),
                format!("docker.io/library/docker@{ENGINE}"),
            ],
        )
        .await?;
        let image = observe_engine_image_from_manifest(&host_image, &intent, engine_manifest)
            .map_err(|e| fail(e.to_string()))?;
        let confirmed = observe_engine(&raw, &intent, owner, &image, limits())
            .map_err(|e| fail(e.to_string()))?;
        if confirmed != observed {
            return Err(fail("registered real engine observation changed"));
        }
        retain(&root.join(format!("{run}-engine-inspect.json")), &raw)?;
        let evidence = root.join("evidence");
        let samples = root.join(format!("{run}-samples"));
        private_dir(&samples)?;
        observer.task = Some(async_engine::launch(watch_cancellation(
            registry.clone(),
            engine.clone(),
            intent.clone(),
            observed.clone(),
            (evidence.clone(), samples, cancel_case),
            cancellation.clone(),
            observer.stop.clone(),
        )));
        run_registered_act(
            registry,
            engine,
            ActRuntimeRequest {
                intent: &intent,
                observed: &observed,
                package,
                base_blobs: blobs,
                source_archive: &snapshot,
                event_payload: &payload,
                event_name: "push",
                evidence_root: &evidence,
                archive_ceiling: 2 << 30,
                execution_deadline: Duration::from_secs(120),
                output_ceiling: OUTPUT,
            },
            &cancellation.token(),
        )
        .await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(fail(
            "whole real probe operation exceeded 120-second deadline",
        )),
    };
    cancellation.cancel();
    let joined = observer.stop_and_join().await;
    let result: std::io::Result<Value> = (|| {
        let nested_cancelled = joined?;
        let report = result?;
        if !report.engine_removed
            || report.execution_success == cancel_case
            || (cancel_case && (!nested_cancelled || report.outcome != "cancelled"))
        {
            return Err(fail(format!(
                "real probe expectations failed: {report:?}; nested cancellation={nested_cancelled}"
            )));
        }
        let evidence = root.join("evidence");
        let frames = bounded_file(&evidence.join(&run).join("output.frames"), OUTPUT + 65536)?;
        if report.output_sha256.as_deref() != Some(digest(&frames).as_str()) {
            return Err(fail("durable output frame identity mismatch"));
        }
        let receipt: Value = serde_json::from_slice(&bounded_file(
            &evidence.join(&run).join("result.json"),
            1 << 20,
        )?)?;
        if receipt["engine_removed"] != true {
            return Err(fail("durable result has no verified absence"));
        }
        Ok(
            json!({"cancel_case":cancel_case,"nested_cancellation_observed":nested_cancelled,"report":report}),
        )
    })();
    let cleanup_result = cleanup(registry, engine, &intent, owner, engine_manifest).await;
    let summary = match (&result, &cleanup_result) {
        (Ok(value), Ok(())) => value.clone(),
        _ => {
            json!({"run_id":run,"error":result.as_ref().err().map(ToString::to_string),"cleanup_error":cleanup_result.as_ref().err().map(ToString::to_string)})
        }
    };
    retain(
        &root.join(format!("{run}-probe-result.json")),
        &serde_json::to_vec_pretty(&summary)?,
    )?;
    cleanup_result?;
    result
}

#[test]
#[ignore = "explicit owned Linux Docker probe; requires reviewed pinned inputs and root authorization"]
pub(super) fn live_pinned_act_success_and_cancellation_remove_private_nested_engines() {
    let runtime = async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let input = PathBuf::from(std::env::var_os("BOSN_ACT_PROBE_INPUT_DIR").expect("set explicit owned BOSN_ACT_PROBE_INPUT_DIR")).canonicalize().unwrap();
        assert!(input.starts_with("/tmp") && input.is_dir(), "probe inputs must be retained in owned /tmp");
        assert_eq!(std::fs::metadata(&input).unwrap().permissions().mode() & 0o077, 0, "probe input directory must be private");
        let manifest = bounded_file(&input.join("runner-manifest.json"), 1<<20).unwrap();
        let config = bounded_file(&input.join("runner-config.json"), 1<<20).unwrap();
        let binary = bounded_file(&input.join("act"), 64<<20).unwrap();
        let package = package_act_image(&manifest, RUNNER, &config, RUNNER_CONFIG, &binary, ACT_BINARY, "0.2.88").unwrap();
        let engine_manifest = VerifiedEngineManifest::verify(&bounded_file(&input.join("engine-manifest.json"),1<<20).unwrap(), ENGINE, &bounded_file(&input.join("engine-config.json"),1<<20).unwrap(), ENGINE_CONFIG).unwrap();
        let owned_blobs = read_layers(&input, &package.base_layers).unwrap();
        let blobs = package.base_layers.iter().zip(&owned_blobs).map(|(layer,bytes)| ActArchiveBlob { digest: &layer.digest, bytes }).collect::<Vec<_>>();
        let root = input.join(format!("probe-{}", random_uuid().await.unwrap())); private_dir(&root).unwrap(); private_dir(&root.join("evidence")).unwrap();
        let owner = random_uuid().await.unwrap();
        let db = root.join("registry.sqlite3");
        let registry = Registry::create_writer(&db, &owner).unwrap();
        let (sender, receiver) = async_engine::channel(16);
        let actor = RegistryActor { sender };
        let task = async_engine::launch(registry_actor(registry, receiver, None));
        let engine = DockerEngine::docker();
        let mut results = Vec::new();
        let mut error = None;
        for cancel in [false,true] { match probe_case(&actor, &engine, (&package, &engine_manifest), &blobs, &root, &owner, cancel).await { Ok(result) => results.push(result), Err(failure) => { error = Some(failure.to_string()); break; } } }
        actor.stop().await; task.await.unwrap();
        retain(&root.join("probe-summary.json"), &serde_json::to_vec_pretty(&json!({"schema_version":1,"scope":"two real Linux shell probes; no public API, fleet graph or native parity proof","results":results,"error":error})).unwrap()).unwrap();
        eprintln!("retained real Act probe evidence: {}", root.display());
        assert!(error.is_none(), "real Act probe failed; retained evidence at {}: {error:?}", root.display());
        let registry = Registry::open_writer(&db).unwrap(); assert!(registry.pending_act_engines(None,16).unwrap().items.is_empty());
    });
}

#[test]
pub(super) fn cancellation_marker_requires_valid_complete_frames() {
    let mut incomplete = vec![2, MARKER.len() as u8, 0, 0, 0];
    incomplete.extend_from_slice(&MARKER[..10]);
    assert!(!frame_marker_bytes(&incomplete).unwrap());
    let mut complete = vec![2, MARKER.len() as u8, 0, 0, 0];
    complete.extend_from_slice(MARKER);
    assert!(frame_marker_bytes(&complete).unwrap());
    complete[0] = 3;
    assert!(frame_marker_bytes(&complete).is_err());
    assert!(frame_marker_bytes(&[1, 1, 0, 128, 0]).is_err());
    assert!(!frame_marker_bytes(&[]).unwrap());
}

#[test]
pub(super) fn aggregate_layer_limit_refuses_before_blob_io() {
    let layer = |size| ActBaseLayer {
        digest: format!("sha256:{}", "0".repeat(64)),
        size,
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
    };
    for layers in [
        vec![layer(LAYER_BYTES), layer(1)],
        vec![layer(u64::MAX), layer(1)],
    ] {
        let error = read_layers(Path::new("/nonexistent-probe-input"), &layers).unwrap_err();
        assert!(error.to_string().contains("aggregate layer byte ceiling"));
    }
}
#[test]
pub(super) fn timed_operation_stops_and_joins_owned_observer() {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let observed_exit = exited.clone();
            let mut observer = ProbeObserver::new();
            let stop = observer.stop.clone();
            observer.task = Some(async_engine::launch(async move {
                while !stop.token().is_cancelled() {
                    async_engine::sleep(Duration::from_millis(1)).await;
                }
                async_engine::sleep(Duration::from_millis(10)).await;
                observed_exit.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(false)
            }));
            assert!(
                async_engine::timeout(
                    Duration::from_millis(5),
                    async_engine::sleep(Duration::from_secs(1))
                )
                .await
                .is_err()
            );
            assert!(!exited.load(std::sync::atomic::Ordering::SeqCst));
            assert!(!observer.stop_and_join().await.unwrap());
            assert!(exited.load(std::sync::atomic::Ordering::SeqCst));
        });
}

/// The production service must expire a prior writer's execution claim and
/// remove only its verified engine before acknowledging authenticated requests.
#[test]
#[ignore = "real pinned Docker startup recovery; owned private input directory required"]
pub(super) fn real_startup_retires_claimed_engine_before_ping() {
    async_engine::RuntimeBuilder::multi_thread().enable_all().build().unwrap().run(async {
        let input = PathBuf::from(std::env::var_os("BOSN_ACT_PROBE_INPUT_DIR").unwrap()).canonicalize().unwrap();
        assert!(input.starts_with("/tmp"));
        let metadata = std::fs::metadata(&input).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
        assert_eq!(metadata.uid(), std::fs::metadata("/proc/self").unwrap().uid());
        let root = input.join(format!("startup-{}", random_uuid().await.unwrap()));
        private_dir(&root).unwrap();
        let owner = random_uuid().await.unwrap();
        let intent = ActEngineIntent {
            run_id: random_uuid().await.unwrap(),
            workspace: root.to_string_lossy().into_owned(),
            candidate_sha: "1".repeat(40),
            payload_sha256: "2".repeat(64),
            snapshot_sha256: "3".repeat(64),
            act_version: "0.2.88".into(),
            act_image_digest: "sha256:525269093be45f019646c470b2e90083790350cea112dd4715a9cc2a332b2f80".into(),
            engine_image_digest: ENGINE.into(),
            runner_image_digest: RUNNER.into(),
            created_at: at(),
            creation_profile: Some(crate::act_engine::creation_profile(limits()).unwrap()),
        };
        retain(&root.join("intent.json"), &serde_json::to_vec_pretty(&intent).unwrap()).unwrap();
        retain(&root.join("owner.json"), &serde_json::to_vec(&owner).unwrap()).unwrap();
        let db = root.join("registry.sqlite3");
        let writer = Registry::create_writer(&db, &owner).unwrap();
        let (sender, receiver) = async_engine::channel(16);
        let actor = RegistryActor { sender };
        let writer_task = async_engine::launch(registry_actor(writer, receiver, None));
        let proofs = crate::act_engine::bundled_engine_manifests().unwrap();
        let observed = create_owned_engine_from_manifest(&actor, &DockerEngine::docker(), intent.clone(), &owner, &proofs[ENGINE], limits(), at()).await.unwrap();
        let token = random_uuid().await.unwrap();
        actor.act_registry(ActRegistryCommand::Claim { intent: intent.clone(), observed: observed.clone(), token, at: at() }).await.unwrap();
        actor.stop().await;
        writer_task.await.unwrap();
        // Reopen through the real service, not a test-only recovery entrypoint.
        let service = Service::new(root.clone());
        let stop = service.stop.clone();
        let service_task = async_engine::launch(service.serve());
        let client = Client::for_state(&root).unwrap();
        async_engine::timeout(Duration::from_secs(130), async {
            loop {
                if client.ping().await.is_ok() { break; }
                async_engine::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        stop.cancel();
        service_task.await.unwrap().unwrap();
        let writer = Registry::open_writer(&db).unwrap();
        let record = writer.act_engine(&intent.run_id).unwrap().unwrap();
        assert_eq!(record.state, ActEngineState::Terminal);
        assert_eq!(record.outcome, Some(ActRunOutcome::Interrupted));
        assert!(record.execution.is_none(), "startup may not fabricate execution");
        for filter in [format!("id={}", observed.engine_id), format!("name=^/{}$", observed.name)] {
            let absence = docker(&DockerEngine::docker(), vec!["container".into(), "ls".into(), "--all".into(), "--no-trunc".into(), "--filter".into(), filter, "--format".into(), "{{.ID}}".into()]).await.unwrap();
            assert!(absence.iter().all(u8::is_ascii_whitespace));
        }
        retain(&root.join("startup-result.json"), &serde_json::to_vec_pretty(&json!({"scope":"real private engine startup through Service::serve; no Act execution or fleet workflow proof", "run_id":intent.run_id,"engine_id":observed.engine_id,"owner":owner,"authenticated_ping":true,"state":"terminal","outcome":"interrupted","execution":null,"exact_id_and_name_absent":true})).unwrap()).unwrap();
    });
}

/// Explicit recovery of one retained failed probe, never discovery or creation.
#[test]
#[ignore = "explicit retained-probe recovery; root review and exact owned identity required"]
pub(super) fn recover_retained_pinned_engine_only() {
    async_engine::RuntimeBuilder::multi_thread().enable_all().build().unwrap().run(async {
        let env = |name| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let input = PathBuf::from(env("BOSN_ACT_PROBE_INPUT_DIR")).canonicalize().unwrap();
        let root = PathBuf::from(env("BOSN_ACT_RECOVERY_DIR")).canonicalize().unwrap();
        let run = env("BOSN_ACT_RECOVERY_RUN");
        let owner = env("BOSN_ACT_RECOVERY_OWNER");
        assert!(input.starts_with("/tmp") && root.parent() == Some(input.as_path()));
        assert!(root.file_name().unwrap().to_str().unwrap().starts_with("probe-"));
        for path in [&input, &root] { let meta = std::fs::metadata(path).unwrap(); assert_eq!(meta.permissions().mode() & 0o077, 0); assert_eq!(meta.uid(), std::fs::metadata("/proc/self").unwrap().uid()); }
        // Validate canonical identity before constructing any path from the run ID.
        assert_eq!(run.len(),36); assert!(run.bytes().enumerate().all(|(i,b)| if [8,13,18,23].contains(&i) { b == b'-' } else { b.is_ascii_digit() || (b'a'..=b'f').contains(&b) }));
        let intent: ActEngineIntent = serde_json::from_slice(&bounded_file(&root.join(format!("{run}-intent.json")),1<<20).unwrap()).unwrap();
        assert_eq!(intent.run_id,run); assert_eq!(intent.engine_image_digest, ENGINE);
        let proof = VerifiedEngineManifest::verify(&bounded_file(&input.join("engine-manifest.json"),1<<20).unwrap(), ENGINE, &bounded_file(&input.join("engine-config.json"),1<<20).unwrap(), ENGINE_CONFIG).unwrap();
        let db = root.join("registry.sqlite3");
        assert!(std::fs::symlink_metadata(&db).unwrap().is_file()); assert_eq!(db.canonicalize().unwrap().parent(), Some(root.as_path()));
        let registry = Registry::open_writer(&db).unwrap(); assert_eq!(registry.registry_id().unwrap(),owner);
        let current = registry.pending_act_engines(None,16).unwrap(); assert!(current.next_run_id.is_none());
        let current = current.items.iter().find(|r| r.intent.run_id == run).expect("exact pending run required");
        assert_eq!(current.intent,intent); assert!(current.execution_claim.is_none(), "recovery must not destroy a live execution claim");
        let (sender,receiver) = async_engine::channel(16); let actor = RegistryActor { sender };
        let task = async_engine::launch(registry_actor(registry,receiver,None));
        let result = cleanup(&actor,&DockerEngine::docker(),&intent,&owner,&proof).await;
        actor.stop().await; task.await.unwrap();
        retain(&root.join(format!("{run}-recovery-{}.json",random_uuid().await.unwrap())), &serde_json::to_vec_pretty(&json!({"run_id":run,"registry_id":owner,"verified_cleanup":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string)})).unwrap()).unwrap();
        result.unwrap();
    });
}

#[test]
#[ignore = "explicit retained real inspect fixture; no Docker calls"]
pub(super) fn retained_legacy_engine_inspect_refuses_without_frozen_profile_offline() {
    let input = PathBuf::from(std::env::var_os("BOSN_ACT_PROBE_INPUT_DIR").unwrap());
    let root = PathBuf::from(std::env::var_os("BOSN_ACT_RECOVERY_DIR").unwrap());
    let run = std::env::var("BOSN_ACT_RECOVERY_RUN").unwrap();
    let owner = std::env::var("BOSN_ACT_RECOVERY_OWNER").unwrap();
    let intent: ActEngineIntent = serde_json::from_slice(
        &bounded_file(&root.join(format!("{run}-intent.json")), 1 << 20).unwrap(),
    )
    .unwrap();
    let proof = VerifiedEngineManifest::verify(
        &bounded_file(&input.join("engine-manifest.json"), 1 << 20).unwrap(),
        ENGINE,
        &bounded_file(&input.join("engine-config.json"), 1 << 20).unwrap(),
        ENGINE_CONFIG,
    )
    .unwrap();
    let manifest: Value = serde_json::from_slice(
        &bounded_file(&input.join("engine-manifest.json"), 1 << 20).unwrap(),
    )
    .unwrap();
    let image = json!([{"Id":ENGINE,"RepoDigests":[format!("docker.io/library/docker@{ENGINE}")],"Descriptor":{"digest":ENGINE,"mediaType":manifest["mediaType"],"size":bounded_file(&input.join("engine-manifest.json"),1<<20).unwrap().len()}}]);
    let image =
        observe_engine_image_from_manifest(&serde_json::to_vec(&image).unwrap(), &intent, &proof)
            .unwrap();
    assert!(intent.creation_profile.is_none());
    let observed = observe_engine(
        &bounded_file(&root.join("failed-created-engine-inspect.json"), 1 << 20).unwrap(),
        &intent,
        &owner,
        &image,
        limits(),
    );
    assert!(observed.is_err());
}
