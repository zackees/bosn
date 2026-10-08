//! Exercise the exact historical producer, recipe and enrollment fence.
use super::toolstore_records::{Install, Intent, Manifest, Proof, Snapshot, Update};
use super::{DockerActBackend, ENGINE_WORK, owned, process_control::ProcessControl};
use crate::ci::cache_policy::CachePolicy;
use std::{path::Path, time::Duration};

const RECIPE: &str = include_str!("fixtures/toolstore_cea9c15e.sh");
const OLD_BINARY: &str = "b4be8d7ef98729ad16a9a6ddba331f1d2b0feb8abd52eb9e5f3a93155fb4f1df";
const OLD_RECIPE: &str = "66430164c4f2fb7cf9190fb29b43058b53f72b2b37442916f49f33b57c394b50";

pub(super) async fn enroll_previous(
    backend: &DockerActBackend,
    engine: &str,
    policy: CachePolicy,
    current_binary: &str,
) -> Result<String, String> {
    let old = std::env::var("BOSN_TEST_PREVIOUS_ACT_BINARY").map_err(|e| e.to_string())?;
    if kernal_api::hash::sha256_bytes(&std::fs::read(&old).map_err(|e| e.to_string())?).to_hex()
        != OLD_BINARY
        || kernal_api::hash::sha256_bytes(RECIPE.as_bytes()).to_hex() != OLD_RECIPE
    {
        return Err("historical producer or exact recipe differs".into());
    }
    install(backend, engine, &old).await?;
    let mut io = open_previous(backend, engine, policy).await?;
    let intent = Intent {
        schema_version: 1,
        nonce: crate::ci::new_uuid().await.map_err(|e| e.to_string())?,
        act_sha256: OLD_BINARY.into(),
        recipe_sha256: OLD_RECIPE.into(),
        policy,
    };
    command(
        &mut io,
        "begin",
        &serde_json::to_string(&intent).map_err(|e| e.to_string())?,
    )
    .await?;
    let source = format!("{}/Tool/1/x64", super::toolstore::SOURCE);
    let object: Snapshot = serde_json::from_slice(&command(&mut io, "object", &source).await?)
        .map_err(|e| e.to_string())?;
    object.validate(&source, false, policy)?;
    let manifest = Manifest {
        schema_version: 1,
        installs: vec![Install {
            path: "Tool/1/x64".into(),
            object_id: object.id,
        }],
    };
    let update: Update = serde_json::from_slice(
        &command(
            &mut io,
            "initialize",
            &serde_json::to_string(&manifest).map_err(|e| e.to_string())?,
        )
        .await?,
    )
    .map_err(|e| e.to_string())?;
    update
        .generation
        .validate(super::toolstore_records::STORE, true, policy)?;
    if !update.selected || update.partial {
        return Err("historical initialization failed".into());
    }
    let proof = Proof {
        schema_version: 1,
        intent,
        initial_generation: update.generation.id,
    };
    let evidence = serde_json::to_string(&proof).map_err(|e| e.to_string())?;
    command(&mut io, "acknowledge", &evidence).await?;
    // A live historical producer still owns FD7 even after acknowledging.
    install(backend, engine, current_binary).await?;
    if !backend
        .maintain_published_tools(engine, policy)
        .await
        .is_err_and(|error| error.contains("busy"))
    {
        return Err("upgrade bypassed historical enrollment fence".into());
    }
    io.send(b"abort\n").await?;
    // Wait for the original lease holder's exit before admitting the upgrade.
    drop(io);
    backend
        .checked(
            "historical control release",
            DockerActBackend::exec(
                engine,
                "exec 7>>/bosn/cache/.bosn-tool-control-v1.lock; flock -x 7",
            ),
            super::CONTROL_DEADLINE,
        )
        .await?;
    if backend
        .maintain_published_tools(engine, policy)
        .await?
        .is_none()
    {
        return Err("upgraded maintenance did not adopt historical authority".into());
    }
    require_evidence(backend, engine, &evidence).await?;
    Ok(evidence)
}

pub(super) async fn require_evidence(
    backend: &DockerActBackend,
    engine: &str,
    evidence: &str,
) -> Result<(), String> {
    let actual = backend
        .checked(
            "unchanged historical proof",
            DockerActBackend::exec(engine, "cat /bosn/cache/.bosn-tool-enrolled-v1.json"),
            super::CONTROL_DEADLINE,
        )
        .await?;
    if actual != evidence {
        return Err("upgrade rewrote historical authority".into());
    }
    Ok(())
}

async fn install(backend: &DockerActBackend, engine: &str, binary: &str) -> Result<(), String> {
    backend.stream_in("upgrade producer", engine, Path::new(binary),
        "cat > /var/lib/docker/bosn-ci/bin/act.next; chmod 755 /var/lib/docker/bosn-ci/bin/act.next; mv /var/lib/docker/bosn-ci/bin/act.next /var/lib/docker/bosn-ci/bin/act").await
}

async fn open_previous(
    backend: &DockerActBackend,
    engine: &str,
    policy: CachePolicy,
) -> Result<ProcessControl, String> {
    let script = RECIPE
        .replace("@CACHE@", super::ENGINE_CACHE)
        .replace("@WORK@", ENGINE_WORK)
        .replace("@PAYLOAD@", &policy.repository_max_bytes.to_string())
        .replace("@EXPIRES@", "2000-01-01T00:00:00Z");
    let mut args = owned(&["exec", "-i", engine, "timeout", "60", "sh", "-c"]);
    args.push(script);
    let process = backend
        .docker
        .with_args(args)
        .spawn_interactive(Duration::from_secs(15))
        .await
        .map_err(|e| e.to_string())?;
    let mut io = ProcessControl::new(
        process,
        "historical producer",
        b"bosn-tool-end:",
        Duration::from_secs(45),
    );
    if io.line().await? != b"bosn-tool-ready" {
        return Err("historical producer is not ready".into());
    }
    Ok(io)
}

async fn command(
    io: &mut ProcessControl,
    operation: &str,
    argument: &str,
) -> Result<Vec<u8>, String> {
    let (code, output) = io
        .command(format!("{operation}\n{argument}\n").as_bytes())
        .await?;
    if code != 0 {
        return Err(format!(
            "historical {operation} failed: {}",
            String::from_utf8_lossy(&output)
        ));
    }
    Ok(output)
}
