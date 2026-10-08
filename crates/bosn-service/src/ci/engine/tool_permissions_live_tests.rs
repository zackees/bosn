//! Retire a non-root, read-only install using the production helper profile.
use super::cache_usage::helper::Identity;
use super::toolstore_records::{Install, Manifest, Retention, STORE, Snapshot, Update};
use super::*;
use bosn_registry::cache_helper::{CacheHelperIntent, CacheHelperRole};

#[test]
#[ignore = "requires isolated Docker and the verified pinned act binary"]
fn production_helper_retires_nonroot_readonly_tool_object() {
    assert_eq!(std::env::var("BOSN_TEST_ISOLATED").as_deref(), Ok("1"));
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let backend = DockerActBackend::default();
            let binary = std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap();
            assert_eq!(
                kernal_api::hash::sha256_bytes(&std::fs::read(&binary).unwrap()).to_hex(),
                act_artifact(std::env::consts::ARCH).unwrap().binary_sha256
            );
            let nonce = crate::ci::new_uuid().await.unwrap();
            let volume = format!("bosn-tool-permission-proof-{nonce}");
            backend
                .checked(
                    "private permission volume",
                    owned(&["volume", "create", &volume]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            let mut containers = Vec::new();
            let result = permission_case(&backend, &binary, &volume, &nonce, &mut containers).await;
            for id in containers {
                backend
                    .checked(
                        "permission helper cleanup",
                        owned(&["rm", "-f", &id]),
                        CONTROL_DEADLINE,
                    )
                    .await
                    .unwrap();
                backend.confirm_measurement_absent(&id).await.unwrap();
            }
            backend
                .checked(
                    "permission volume cleanup",
                    owned(&["volume", "rm", &volume]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            assert!(!backend.volume_exists(&volume).await.unwrap());
            result.unwrap();
        });
}

async fn install(backend: &DockerActBackend, id: &str, binary: &str) -> Result<(), String> {
    backend.stream_in("permission fixture pinned act", id, Path::new(binary),
        "mkdir -p /var/lib/docker/bosn-ci/bin; cat >/var/lib/docker/bosn-ci/bin/act; chmod +x /var/lib/docker/bosn-ci/bin/act").await
}

async fn native(backend: &DockerActBackend, id: &str, args: &[&str]) -> Result<String, String> {
    let mut command = owned(&["exec", id, "/var/lib/docker/bosn-ci/bin/act", "cache"]);
    command.extend(args.iter().map(|arg| (*arg).into()));
    backend
        .checked("permission fixture native cache", command, PULL_DEADLINE)
        .await
}

async fn object(backend: &DockerActBackend, id: &str, source: &str) -> Result<Snapshot, String> {
    let text = native(
        backend,
        id,
        &[
            "tool-publish",
            "--from",
            source,
            "--source-quiescent",
            "--cache-server-path",
            STORE,
            "--max-bytes",
            "8388608",
            "--apply",
        ],
    )
    .await?;
    let report: Snapshot = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    report.validate(
        source,
        false,
        crate::ci::cache_policy::CachePolicy::default(),
    )?;
    Ok(report)
}

async fn permission_case(
    backend: &DockerActBackend,
    binary: &str,
    volume: &str,
    nonce: &str,
    containers: &mut Vec<String>,
) -> Result<(), String> {
    let image = engine_image();
    let mount = format!("type=volume,source={volume},target={ENGINE_CACHE}");
    let publisher = backend
        .checked(
            "permission publisher",
            owned(&[
                "run",
                "-d",
                "--network",
                "none",
                "--read-only",
                "--memory",
                "128m",
                "--cpus",
                "1",
                "--tmpfs",
                "/var/lib/docker:exec,size=64m",
                "--mount",
                &mount,
                "--entrypoint",
                "sleep",
                &image,
                "300",
            ]),
            CONTROL_DEADLINE,
        )
        .await?;
    if !cache_usage::helper::valid_id(&publisher) {
        return Err("invalid fixture publisher ID".into());
    }
    containers.push(publisher.clone());
    install(backend, &publisher, binary).await?;
    backend.checked("nonroot immutable source", DockerActBackend::exec(&publisher,
        "mkdir -p /var/lib/docker/old /var/lib/docker/current; dd if=/dev/zero of=/var/lib/docker/old/data bs=1M count=4 2>/dev/null; touch /var/lib/docker/old/.complete; chown -R 1001:1001 /var/lib/docker/old; chmod 555 /var/lib/docker/old; printf warm >/var/lib/docker/current/data; touch /var/lib/docker/current/.complete"), CONTROL_DEADLINE).await?;
    let orphan = object(backend, &publisher, "/var/lib/docker/old").await?;
    let current = object(backend, &publisher, "/var/lib/docker/current").await?;
    let manifest = Manifest {
        schema_version: 1,
        installs: vec![Install {
            path: "Tool/1/x64".into(),
            object_id: current.id,
        }],
    };
    let manifest = serde_json::to_string(&manifest).map_err(|e| e.to_string())?;
    backend
        .checked(
            "permission generation manifest",
            DockerActBackend::exec(
                &publisher,
                &format!("printf '%s' '{manifest}' >/var/lib/docker/manifest.json"),
            ),
            CONTROL_DEADLINE,
        )
        .await?;
    let update: Update = serde_json::from_str(
        &native(
            backend,
            &publisher,
            &[
                "tool-update",
                "--initialize",
                "--manifest",
                "/var/lib/docker/manifest.json",
                "--cache-server-path",
                STORE,
                "--max-bytes",
                "8388608",
                "--apply",
            ],
        )
        .await?,
    )
    .map_err(|e| e.to_string())?;
    update
        .generation
        .validate(STORE, true, crate::ci::cache_policy::CachePolicy::default())?;
    if !update.selected || update.partial {
        return Err("fixture generation was not selected".into());
    }
    retire_in_helper(
        backend,
        binary,
        volume,
        nonce,
        containers,
        &FixturePublication {
            orphan: orphan.id,
            selected: update.generation.id,
        },
    )
    .await
}

struct FixturePublication {
    orphan: String,
    selected: String,
}

async fn retire_in_helper(
    backend: &DockerActBackend,
    binary: &str,
    volume: &str,
    nonce: &str,
    containers: &mut Vec<String>,
    publication: &FixturePublication,
) -> Result<(), String> {
    let intent = CacheHelperIntent {
        registry_id: crate::ci::new_uuid().await.map_err(|e| e.to_string())?,
        nonce: nonce.into(),
        image: engine_image(),
        volume: volume.into(),
        created_at: crate::ci::lifecycle::now_seconds(),
        role: Some(CacheHelperRole::MaintenanceV1),
    };
    let identity = Identity::from_intent(&intent)?;
    // Isolate only the volume name; all remaining creation arguments and labels
    // come from the production helper factory and its typed intent.
    let args = maintenance_helper::helper_create_args(&identity)
        .into_iter()
        .map(|arg| {
            arg.replace(
                &format!("source={CACHE_VOLUME},"),
                &format!("source={volume},"),
            )
        })
        .collect();
    let helper = backend
        .checked(
            "production permission helper create",
            args,
            CONTROL_DEADLINE,
        )
        .await?;
    if !cache_usage::helper::valid_id(&helper) {
        return Err("invalid fixture helper ID".into());
    }
    containers.push(helper.clone());
    let inspected = backend
        .checked(
            "production permission helper inspection",
            owned(&["inspect", &helper]),
            CONTROL_DEADLINE,
        )
        .await?;
    if identity.verify(&inspected, volume)? != helper {
        return Err("permission helper identity changed".into());
    }
    backend
        .checked(
            "production permission helper start",
            owned(&["start", &helper]),
            CONTROL_DEADLINE,
        )
        .await?;
    install(backend, &helper, binary).await?;
    let report: Retention = serde_json::from_str(
        &native(
            backend,
            &helper,
            &[
                "tool-retain",
                "--cache-server-path",
                STORE,
                "--max-payload-bytes",
                "8388608",
                "--max-allocated-bytes",
                "8388608",
                "--expire-before",
                "2050-01-01T00:00:00Z",
                "--max-candidates",
                "128",
                "--max-entries",
                "1000000",
                "--apply",
            ],
        )
        .await?,
    )
    .map_err(|e| e.to_string())?;
    if report.partial || report.after.allocated()? >= report.before.allocated()? {
        return Err("production helper did not release complete orphan allocation".into());
    }
    backend.checked("permission cleanup exact orphan and warm preservation", DockerActBackend::exec(&helper,
        &format!("test ! -e {STORE}/{}; test \"$(cat {STORE}/.tool-generations-v1/{}/tree/Tool/1/x64/data)\" = warm", publication.orphan, publication.selected)), CONTROL_DEADLINE).await?;
    Ok(())
}
