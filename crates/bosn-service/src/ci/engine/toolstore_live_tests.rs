//! Actual normal tool publication must enter a selected, retainable store.
use super::*;
use super::{
    toolstore::SOURCE,
    toolstore_records::{STORE, Snapshot},
};
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
        let policy = super::super::cache_policy::CachePolicy {
            repository_max_bytes: 4,
            unused_age_secs: 1,
            ..super::super::cache_policy::CachePolicy::default()
        };
        let id = backend.checked("tool proof create", owned(&[
            "run", "--rm", "-d", "--name", &name, "--pull", "never",
            "--network", "none", "--read-only", "--privileged",
            "--memory", "384m", "--cpus", "1", "--tmpfs", "/bosn/cache:exec,size=16m",
            "--tmpfs", "/var/lib/docker:exec,size=256m", "--tmpfs", "/run:rw,size=16m", "--tmpfs", "/tmp:rw,size=16m", "--entrypoint", "dockerd", &image, "--host", "unix:///var/run/docker.sock", "--iptables=false", "--bridge=none", "--storage-driver=vfs", "--ip-forward=false", "--ip-masq=false", "--userland-proxy=false",
        ]), CONTROL_DEADLINE).await.unwrap();
        assert!(cache_usage::helper::valid_id(&id));
        let result: Result<(), String> = async {
            backend.stream_in("verified act", &id, Path::new(&binary),
                "mkdir -p /var/lib/docker/bosn-ci/bin; cat > /var/lib/docker/bosn-ci/bin/act; chmod 755 /var/lib/docker/bosn-ci/bin/act").await?;
            backend.checked("private nested daemon ready", DockerActBackend::exec(&id,
                "n=0; until docker info >/dev/null 2>&1; do n=$((n+1)); [ \"$n\" -lt 100 ] || exit 1; sleep 0.1; done"), CONTROL_DEADLINE).await?;
            // An empty workflow must not freeze a poisoned first manifest.
            backend.save_toolcache_with_policy(&id, Some(policy)).await?;
            backend.checked("empty save leaves no authority", DockerActBackend::exec(&id,
                "test ! -e /bosn/cache/.bosn-tool-preparing-v1.json; test ! -e /bosn/cache/.bosn-tool-initial-installs-v1.json; docker volume create act-toolcache >/dev/null"), CONTROL_DEADLINE).await?;
            backend.save_toolcache_with_policy(&id, Some(policy)).await?;
            backend.checked("empty existing volume leaves no authority", DockerActBackend::exec(&id,
                "test ! -e /bosn/cache/.bosn-tool-preparing-v1.json; test ! -e /bosn/cache/.bosn-tool-initial-installs-v1.json"), CONTROL_DEADLINE).await?;
            backend.checked("completed tool fixture", DockerActBackend::exec(&id,
                "mkdir -p /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64; printf warm > /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64/tool; touch /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64.complete"), CONTROL_DEADLINE).await?;
            backend.save_toolcache_with_policy(&id, None).await?;
            backend.checked("explicit opt-out stays legacy", DockerActBackend::exec(&id,
                "test \"$(cat /bosn/cache/toolcache/Tool/1/x64/tool)\" = warm; test ! -e /bosn/cache/toolstore-v1; test ! -e /bosn/cache/.bosn-tool-preparing-v1.json"), CONTROL_DEADLINE).await?;
            let writer = backend.checked("private tool writer", DockerActBackend::exec(&id,
                "tar -C / -cf /var/lib/docker/bosn-ci/writer.tar bin/busybox lib; docker import /var/lib/docker/bosn-ci/writer.tar tool-writer >/dev/null; docker run -d --network none --mount type=volume,src=act-toolcache,dst=/tools --entrypoint /bin/busybox tool-writer sleep 60"), CONTROL_DEADLINE).await?;
            if !cache_usage::helper::valid_id(&writer) { return Err("invalid nested writer identity".into()); }
            let refused = backend.save_toolcache_with_policy(&id, Some(policy)).await;
            if !refused.is_err_and(|error| error.contains("tool installs failed (75)")) {
                return Err("normal save did not refuse a live source writer".into());
            }
            backend.checked("writer refusal leaves no authority", DockerActBackend::exec(&id,
                "test ! -e /bosn/cache/.bosn-tool-preparing-v1.json; test ! -e /bosn/cache/toolstore-v1"), CONTROL_DEADLINE).await?;
            backend.checked("private writer cleanup", owned(&["exec", &id, "docker", "rm", "-f", &writer]), CONTROL_DEADLINE).await?;
            backend.save_toolcache_with_policy(&id, Some(policy)).await?;
            let current = backend.checked("normal tool selection", owned(&[
                "exec", &id, "/var/lib/docker/bosn-ci/bin/act", "cache", "tool-current",
                "--cache-server-path", "/bosn/cache/toolstore-v1", "--max-bytes", "4",
            ]), CONTROL_DEADLINE).await?;
            let selection: Selection = serde_json::from_str(&current).map_err(|e| e.to_string())?;
            if selection.schema_version != 1 || !cache_usage::helper::valid_id(&selection.id) {
                return Err("normal save did not publish a typed selected generation".into());
            }
            let script = format!("test \"$(cat /bosn/cache/toolstore-v1/.tool-generations-v1/{}/tree/Tool/1/x64/tool)\" = warm", selection.id);
            backend.checked("selected warm payload", DockerActBackend::exec(&id, &script), CONTROL_DEADLINE).await?;
            backend.checked("replace private source volume", DockerActBackend::exec(&id,
                "docker volume rm act-toolcache >/dev/null"), CONTROL_DEADLINE).await?;
            if !backend.seed_published_tools(&id).await? { return Err("published warm seed was not admitted".into()); }
            backend.checked("normal warm tool payload", DockerActBackend::exec(&id,
                "test \"$(cat /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64/tool)\" = warm; test -f /var/lib/docker/volumes/act-toolcache/_data/Tool/1/x64.complete"), CONTROL_DEADLINE).await?;
            backend.save_toolcache_with_policy(&id, Some(policy)).await?;
            // Each new engine source starts with the selected warm install.
            // Admit a different completed version when the payload budget is
            // already full, then retire the superseded immutable generation.
            let (superseded, selected) = publish_successors(&backend, &id, policy, &selection.id).await?;
            // Leave a completed unselected native publication so the final
            // idle pass has deterministic measurable storage to reclaim even
            // when normal saves already reclaimed older generations.
            let orphan = create_closed_orphan(&backend, &id, policy).await?;
            // Retention uses the actual idle-maintenance path and its complete
            // allocated inventory, rather than deleting fixture objects directly.
            async_engine::sleep(Duration::from_secs(2)).await;
            backend.checked("hide private daemon endpoint", DockerActBackend::exec(&id,
                "mv /var/run/docker.sock /var/run/docker.sock.hidden; ! docker info >/dev/null 2>&1"), CONTROL_DEADLINE).await?;
            let maintenance = backend.maintain_published_tools(&id, policy).await?
                .ok_or("idle tool retention failed to recognize published authority")?;
            if maintenance.allocated_after <= 0 { return Err("idle tool inventory is incomplete".into()); }
            if maintenance.retired_objects == 0 || maintenance.allocated_after >= maintenance.allocated_before {
                return Err("idle retention did not release orphan storage".into());
            }
            for generation in superseded {
                backend.checked("superseded generation absent", DockerActBackend::exec(&id,
                    &format!("test ! -e {STORE}/.tool-generations-v1/{generation}")), CONTROL_DEADLINE).await?;
            }
            backend.checked("unselected closed object absent", DockerActBackend::exec(&id,
                &format!("test ! -e {STORE}/{}", orphan.id)), CONTROL_DEADLINE).await?;
            verify_selected_payload(&backend, &id, &selected).await?;
            backend.checked("idle retention removes original generation", DockerActBackend::exec(&id,
                &format!("test ! -e {STORE}/.tool-generations-v1/{}; test \"$(cat {SOURCE}/Tool/4/x64/tool)\" = next", selection.id)), CONTROL_DEADLINE).await?;
            Ok(())
        }.await;
        backend.checked("tool proof cleanup", owned(&["rm", "-f", &id]), CONTROL_DEADLINE).await.unwrap();
        backend.confirm_measurement_absent(&id).await.unwrap();
        result.unwrap();
    });
}

async fn publish_successors(
    backend: &DockerActBackend,
    id: &str,
    policy: super::super::cache_policy::CachePolicy,
    initial: &str,
) -> Result<(Vec<String>, String), String> {
    let mut superseded = Vec::new();
    let mut previous = initial.to_owned();
    for version in 2..=4 {
        let fixture = format!(
            "mkdir -p {SOURCE}/Tool/{version}/x64; printf next > {SOURCE}/Tool/{version}/x64/tool; touch {SOURCE}/Tool/{version}/x64.complete"
        );
        backend
            .checked(
                "successor tool fixture",
                DockerActBackend::exec(id, &fixture),
                CONTROL_DEADLINE,
            )
            .await?;
        backend.save_toolcache_with_policy(id, Some(policy)).await?;
        let current = backend
            .checked(
                "bounded successor selection",
                owned(&[
                    "exec",
                    id,
                    "/var/lib/docker/bosn-ci/bin/act",
                    "cache",
                    "tool-current",
                    "--cache-server-path",
                    STORE,
                    "--max-bytes",
                    "4",
                ]),
                CONTROL_DEADLINE,
            )
            .await?;
        let successor: Selection = serde_json::from_str(&current).map_err(|e| e.to_string())?;
        if successor.schema_version != 1 || !cache_usage::helper::valid_id(&successor.id) {
            return Err("bounded successor lacks verified selection".into());
        }
        superseded.push(previous);
        previous = successor.id.clone();
        let payload = format!(
            "test \"$(cat {STORE}/.tool-generations-v1/{}/tree/Tool/{version}/x64/tool)\" = next; test ! -e {STORE}/.tool-generations-v1/{}/tree/Tool/{}/x64",
            successor.id,
            successor.id,
            version - 1
        );
        backend
            .checked(
                "bounded successor payload",
                DockerActBackend::exec(id, &payload),
                CONTROL_DEADLINE,
            )
            .await?;
        backend
            .checked(
                "reset source for next normal run",
                DockerActBackend::exec(id, "docker volume rm act-toolcache >/dev/null"),
                CONTROL_DEADLINE,
            )
            .await?;
        if !backend.seed_published_tools(id).await? {
            return Err("successor warm seed failed".into());
        }
    }
    Ok((superseded, previous))
}

async fn verify_selected_payload(
    backend: &DockerActBackend,
    id: &str,
    expected: &str,
) -> Result<(), String> {
    let body = backend
        .checked(
            "selected shared generation after GC",
            owned(&[
                "exec",
                id,
                "/var/lib/docker/bosn-ci/bin/act",
                "cache",
                "tool-current",
                "--cache-server-path",
                STORE,
                "--max-bytes",
                "4",
            ]),
            CONTROL_DEADLINE,
        )
        .await?;
    let current: Selection = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if current.schema_version != 1 || current.id != expected {
        return Err("idle retention changed the selected shared generation".into());
    }
    backend.checked("selected shared warm payload after GC", DockerActBackend::exec(id,
        &format!("test \"$(cat {STORE}/.tool-generations-v1/{expected}/tree/Tool/4/x64/tool)\" = next")), CONTROL_DEADLINE).await?;
    Ok(())
}

async fn create_closed_orphan(
    backend: &DockerActBackend,
    id: &str,
    policy: super::super::cache_policy::CachePolicy,
) -> Result<Snapshot, String> {
    let orphan_source = format!("{SOURCE}/Tool/unselected/x64");
    backend.checked("completed orphan source", DockerActBackend::exec(id,
                &format!("mkdir -p {orphan_source}; printf cold > {orphan_source}/tool; touch {orphan_source}.complete")), CONTROL_DEADLINE).await?;
    let orphan_json = backend
        .checked(
            "closed orphan publication",
            owned(&[
                "exec",
                id,
                "/var/lib/docker/bosn-ci/bin/act",
                "cache",
                "tool-publish",
                "--from",
                &orphan_source,
                "--source-quiescent",
                "--cache-server-path",
                STORE,
                "--max-bytes",
                "4",
                "--apply",
            ]),
            CONTROL_DEADLINE,
        )
        .await?;
    let orphan: Snapshot = serde_json::from_str(&orphan_json).map_err(|e| e.to_string())?;
    orphan.validate(&orphan_source, false, policy)?;
    Ok(orphan)
}
