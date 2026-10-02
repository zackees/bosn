//! The live probe cases (ignored: they need reviewed pinned inputs and Docker).

use super::*;

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
            runner_image_digest: crate::ci::pins::runner_manifest().into(),
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
