//! Actual normal tool publication must enter a selected, retainable store.
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    schema_version: u32,
    id: String,
}

#[test]
#[ignore = "requires isolated Docker and the verified pinned act binary"]
fn normal_completed_tool_save_publishes_a_selected_generation() {
    assert_eq!(std::env::var("BOSN_TEST_ISOLATED").as_deref(), Ok("1"));
    let binary = std::env::var("BOSN_ACT_RETENTION_TEST_BINARY").unwrap();
    assert_eq!(
        kernal_api::hash::sha256_bytes(&std::fs::read(&binary).unwrap()).to_hex(),
        act_artifact(std::env::consts::ARCH).unwrap().binary_sha256
    );
    async_engine::RuntimeBuilder::multi_thread().enable_all().build().unwrap().run(async {
        let backend = DockerActBackend::default();
        let nonce = crate::ci::new_uuid().await.unwrap();
        let name = format!("bosn-tool-publication-proof-{nonce}");
        let image = engine_image();
        let id = backend.checked("tool proof create", owned(&[
            "run", "--rm", "-d", "--name", &name, "--pull", "never",
            "--network", "none", "--read-only", "--cap-drop", "ALL",
            "--memory", "128m", "--cpus", "1", "--tmpfs", "/bosn/cache:exec",
            "--tmpfs", "/var/lib/docker:exec", "--entrypoint", "sleep", &image, "120",
        ]), CONTROL_DEADLINE).await.unwrap();
        assert!(cache_usage::helper::valid_id(&id));
        let result: Result<(), String> = async {
            backend.stream_in("verified act", &id, Path::new(&binary),
                "mkdir -p /var/lib/docker/bosn-ci/bin; cat > /var/lib/docker/bosn-ci/bin/act; chmod 755 /var/lib/docker/bosn-ci/bin/act").await?;
            backend.checked("completed tool fixture", DockerActBackend::exec(&id,
                "mkdir -p /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64; printf warm > /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64/tool; touch /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64.complete"), CONTROL_DEADLINE).await?;
            backend.save_toolcache(&id).await?;
            let current = backend.checked("normal tool selection", owned(&[
                "exec", &id, "/var/lib/docker/bosn-ci/bin/act", "cache", "tool-current",
                "--cache-server-path", "/bosn/cache/toolstore-v1", "--max-bytes", "8589934592",
            ]), CONTROL_DEADLINE).await?;
            let selection: Selection = serde_json::from_str(&current).map_err(|e| e.to_string())?;
            if selection.schema_version != 1 || !cache_usage::helper::valid_id(&selection.id) {
                return Err("normal save did not publish a typed selected generation".into());
            }
            let script = format!("test \"$(cat /bosn/cache/toolstore-v1/.tool-generations-v1/{}/tree/Tool/1/x64/tool)\" = warm", selection.id);
            backend.checked("selected warm payload", DockerActBackend::exec(&id, &script), CONTROL_DEADLINE).await?;
            Ok(())
        }.await;
        backend.checked("tool proof cleanup", owned(&["rm", "-f", &id]), CONTROL_DEADLINE).await.unwrap();
        backend.confirm_measurement_absent(&id).await.unwrap();
        result.unwrap();
    });
}
