//! Manifest creation identity and the recovery proof fixtures.

use super::*;

pub(super) fn recovery_fixture_image(plan: &SetupPlan, identity: &str) -> PreparedImage {
    let SetupPlanAppSource::PinnedImage { image } = &plan.app_source else {
        panic!("fixture pinned image");
    };
    PreparedImage {
        setup_content_sha256: plan.content_sha256.clone(),
        kind: PreparedImageKind::PinnedImage {
            image: image.clone(),
        },
        reference: image.clone(),
        observed_identity: identity.into(),
    }
}

#[test]
pub(super) fn manifest_creation_binds_workspaces_volumes_and_recovery_requires_actual_proof() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let document = format!(
        "[stack.app]\nimage = 'example.invalid/app@sha256:{}'\n[stack.app.mounts.repo]\nsource = '.'\ndestination = '/repo'\nreadonly = true\n[stack.app.volumes.target]\nscope = 'stack'\ndestination = '/target'\n",
        "a".repeat(64)
    );
    std::fs::write(first.join("bosn.toml"), &document).unwrap();
    std::fs::write(second.join("bosn.toml"), &document).unwrap();
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let request = |workspace| ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(2),
                output_limit: 512 * 1024,
            };
            let first = manifest_stack_setup_plan(&request(first)).await.unwrap();
            let second = manifest_stack_setup_plan(&request(second)).await.unwrap();
            let image = recovery_fixture_image(&first.plan, TEST_IDENTITY);
            let other_image = recovery_fixture_image(&second.plan, TEST_IDENTITY);
            let name =
                bosn_setup::setup_container_name(&first.plan, &first.plan.workspace_root, &image)
                    .unwrap();
            let other_name = bosn_setup::setup_container_name(
                &second.plan,
                &second.plan.workspace_root,
                &other_image,
            )
            .unwrap();
            assert_eq!(image.observed_identity, other_image.observed_identity);
            assert_ne!(name, other_name);
            assert_ne!(
                first.plan.named_volumes[0].name,
                second.plan.named_volumes[0].name
            );
            assert_ne!(
                setup_container_resource_id("manifest-container", "app", &name),
                setup_container_resource_id("manifest-container", "app", &other_name)
            );
            let proof = recovery_fixture_proof(&first.plan, &image);
            let fake = |proof| FakeManifestRecoveryExecutor {
                observed: Mutex::new(None),
                starts: AtomicUsize::new(0),
                proof,
            };
            assert!(
                fake(Some(proof.clone()))
                    .verify_profile(&first.plan, TEST_IDENTITY, &name)
                    .await
                    .is_ok()
            );
            assert!(
                fake(None)
                    .verify_profile(&first.plan, TEST_IDENTITY, &name)
                    .await
                    .is_err()
            );
            for (index, field, replacement) in [
                (0, "Source", "/foreign/repo"),
                (1, "Name", "foreign-volume"),
            ] {
                let mut bad = proof.clone();
                bad.0.configuration["Mounts"][index][field] = replacement.into();
                let fake = fake(Some(bad));
                assert!(
                    fake.verify_profile(&first.plan, TEST_IDENTITY, &name)
                        .await
                        .is_err()
                );
                assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
            }
            for (field, value) in [
                (
                    "Options",
                    serde_json::json!({"type":"none","o":"bind","device":"/foreign/path"}),
                ),
                ("Mountpoint", serde_json::json!("/foreign/path")),
            ] {
                let mut bad = proof.clone();
                bad.2.get_mut(&first.plan.named_volumes[0].name).unwrap()[field] = value;
                let fake = fake(Some(bad));
                assert!(
                    fake.verify_profile(&first.plan, TEST_IDENTITY, &name)
                        .await
                        .is_err()
                );
                assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
            }
        });
}

pub(super) fn recovery_fixture_proof(
    plan: &SetupPlan,
    image: &PreparedImage,
) -> (
    bosn_setup::SetupEnsureObservedContainer,
    serde_json::Value,
    BTreeMap<String, serde_json::Value>,
) {
    let name = bosn_setup::setup_container_name(plan, &plan.workspace_root, image).unwrap();
    let digest = name.strip_prefix("bosn-setup-v2-").unwrap();
    let labels = BTreeMap::from([
        ("com.zackees.bosn.setup-managed".into(), "v1".into()),
        (
            "com.zackees.bosn.setup-content-sha256".into(),
            plan.content_sha256.clone(),
        ),
        ("com.zackees.bosn.setup-container".into(), name.clone()),
        (
            "com.zackees.bosn.setup-creation-profile".into(),
            format!("v2:{digest}"),
        ),
    ]);
    let image_config = serde_json::json!({"Id":image.observed_identity,"Config":{
        "Env":["PATH=/usr/bin:/bin"],"Cmd":["sh"],"Entrypoint":null,"User":"","WorkingDir":"","Volumes":null}});
    let mut config = image_config["Config"].clone();
    for key in ["Tty", "OpenStdin", "StdinOnce", "AttachStdin"] {
        config[key] = false.into();
    }
    let mut env = plan.app.environment.clone();
    env.entry("PATH".into())
        .or_insert_with(|| "/usr/bin:/bin".into());
    config["Env"] = serde_json::json!(
        env.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
    );
    config["Labels"] = serde_json::json!(labels);
    // Exact five-argument shell carrier used by Docker creation.
    if let Some(command) = &plan.app.command {
        config["Cmd"] = serde_json::json!([
            "sh",
            "-c",
            "BOSN_IMAGE_PATH=\"$PATH\"; export BOSN_IMAGE_PATH; exec sh -lc \"$1\"",
            "sh",
            format!(
                "PATH=\"${{BOSN_IMAGE_PATH:+$BOSN_IMAGE_PATH:}}$PATH\"; export PATH; unset BOSN_IMAGE_PATH\n{command}"
            )
        ]);
    }
    let mut actual_mounts = Vec::new();
    let mut declared_mounts = Vec::new();
    for mount in &plan.app.mounts {
        let source = std::fs::canonicalize(plan.workspace_root.join(&mount.source)).unwrap();
        actual_mounts.push(serde_json::json!({"Type":"bind","Propagation":"rprivate","Source":source,"Destination":mount.target,"RW":!mount.readonly}));
        declared_mounts.push(serde_json::json!({"Type":"bind","Source":source,"Target":mount.target,"ReadOnly":mount.readonly}));
    }
    for volume in &plan.named_volumes {
        actual_mounts.push(serde_json::json!({"Type":"volume","Driver":"local","Name":volume.name,"Source":format!("/var/lib/docker/volumes/{}/_data",volume.name),"Destination":volume.target,"RW":true}));
        declared_mounts.push(serde_json::json!({"Type":"volume","Source":volume.name,"Target":volume.target,"ReadOnly":false}));
    }
    let host = serde_json::json!({"Mounts":declared_mounts,"VolumeDriver":"","Privileged":false,"NetworkMode":"default","Binds":null,
        "VolumesFrom":null,"DeviceRequests":null,"SecurityOpt":null,"GroupAdd":null,"DeviceCgroupRules":null,"PublishAllPorts":false,"AutoRemove":false,"CgroupnsMode":"private","RestartPolicy":{"Name":"no","MaximumRetryCount":0},"ReadonlyRootfs":false,"PidMode":"","UTSMode":"",
        "UsernsMode":"","IpcMode":"private","Devices":[],"CapAdd":null,"CapDrop":null,"PortBindings":{},"Tmpfs":{}});
    let configuration = serde_json::json!({"Id":TEST_CONTAINER_ID,"Name":format!("/{name}"),"Image":image.observed_identity,
        "State":{"Running":false},"Config":config,"HostConfig":host,"Mounts":actual_mounts});
    (
        bosn_setup::SetupEnsureObservedContainer {
            container_id: TEST_CONTAINER_ID.into(),
            running: false,
            image_identity: image.observed_identity.clone(),
            labels,
            configuration,
        },
        image_config,
        plan.named_volumes.iter().map(|v| (v.name.clone(),serde_json::json!({"Name":v.name,"Driver":"local","Scope":"local","Options":null,"Labels":v.labels,"Mountpoint":format!("/var/lib/docker/volumes/{}/_data",v.name)}))).collect(),
    )
}

pub(super) struct FakeManifestRecoveryExecutor {
    pub(super) proof: Option<(
        bosn_setup::SetupEnsureObservedContainer,
        serde_json::Value,
        BTreeMap<String, serde_json::Value>,
    )>,
    pub(super) observed: Mutex<Option<SetupReconcileObserved>>,
    pub(super) starts: AtomicUsize,
}
impl ManifestRecoveryExecutor for FakeManifestRecoveryExecutor {
    fn verify_profile<'a>(
        &'a self,
        plan: &'a SetupPlan,
        image_identity: &'a str,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let (observed, image_config, volumes) = self
                .proof
                .as_ref()
                .ok_or_else(|| "profile proof missing".to_owned())?;
            let image = recovery_fixture_image(plan, image_identity);
            if bosn_setup::setup_container_name(plan, &plan.workspace_root, &image)
                .map_err(|e| e.to_string())?
                != name
            {
                return Err("profile identity mismatch".into());
            }
            bosn_setup::verify_setup_observation(plan, &image, observed, image_config, volumes)
                .map_err(|e| e.to_string())
        })
    }
    fn inspect<'a>(
        &'a self,
        _name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<SetupReconcileObserved>, String>> + Send + 'a>>
    {
        Box::pin(async move { Ok(self.observed.lock().unwrap().clone()) })
    }
    fn start<'a>(
        &'a self,
        _name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if let Some(observed) = self.observed.lock().unwrap().as_mut() {
                observed.running = true;
            }
            Ok(())
        })
    }
}
