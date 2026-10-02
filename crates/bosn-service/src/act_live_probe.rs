//! Explicit, ignored real-Docker probe. Inputs and all evidence are retained.
//! This is implementation evidence, not a public API or fleet/native CI proof.
#![cfg(target_os = "linux")]

use super::*;
use crate::act_archive::ActArchiveBlob;
use crate::act_engine::{
    ActEngineLimits, VerifiedEngineManifest, create_owned_engine_from_manifest, observe_engine,
    observe_engine_image_from_manifest, remove_owned_engine,
};
use crate::act_image::{ActBaseLayer, ActImagePackage, package_act_image};
use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
use crate::act_runtime::{ActRuntimeRequest, run_registered_act};
use bosn_registry::act::{
    ActEngineIntent, ActEngineObservation, ActEngineRemovalProof, ActEngineState, ActRunOutcome,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RUNNER: &str = "sha256:be3b065b90a7a029ea30aa8ce897a62bfc8bd4d6698951b2527e1f11ba70cc6c";
const RUNNER_CONFIG: &str =
    "sha256:0385872e2126185df5bef04f9b47c04d81b59d48b8d98ee95bac0928adc08c85";
const ACT_BINARY: &str = "sha256:a76aa7627c633f5e9e9b06407d6eb1069213b1ee984599381b84ad4e7bd894f0";
const ENGINE: &str = "sha256:6acc6aaf783ac1c1100822e542534c3dab3f1d38782760b0bdcb688280574d9e";
const ENGINE_CONFIG: &str =
    "sha256:8cdb6d492106752d557cda50e628b88e7bb303a7eaea91a10bdf672b95ad4f52";
const OUTPUT: usize = 8 << 20;
const MARKER: &[u8] = b"BOSN_REAL_CANCEL_STARTED";

fn fail(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}
fn at() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", Sha256Hasher::digest(bytes))
}
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
fn retain(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn bounded_file(path: &Path, ceiling: usize) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > ceiling as u64 {
        return Err(fail("probe input is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(ceiling as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > ceiling {
        return Err(fail("probe input grew beyond its bound"));
    }
    Ok(bytes)
}
const LAYER_BYTES: u64 = 1 << 30;
fn read_layers(input: &Path, layers: &[ActBaseLayer]) -> std::io::Result<Vec<Vec<u8>>> {
    let total = layers
        .iter()
        .try_fold(0u64, |total, layer| total.checked_add(layer.size))
        .ok_or_else(|| fail("aggregate layer byte ceiling overflow"))?;
    if total > LAYER_BYTES {
        return Err(fail("aggregate layer byte ceiling exceeded"));
    }
    layers
        .iter()
        .map(|layer| {
            let ceiling = usize::try_from(layer.size)
                .map_err(|_| fail("layer size exceeds address space"))?;
            let bytes = bounded_file(
                &input.join("blobs/sha256").join(&layer.digest[7..]),
                ceiling,
            )?;
            if bytes.len() as u64 != layer.size || digest(&bytes) != layer.digest {
                return Err(fail("layer digest or size mismatch"));
            }
            Ok(bytes)
        })
        .collect()
}
struct ProbeObserver {
    stop: CancellationSource,
    task: Option<async_engine::Task<std::io::Result<bool>>>,
}
impl ProbeObserver {
    fn new() -> Self {
        Self {
            stop: CancellationSource::new(),
            task: None,
        }
    }
    async fn stop_and_join(&mut self) -> std::io::Result<bool> {
        self.stop.cancel();
        match self.task.take() {
            Some(task) => task.await.map_err(|e| fail(e.to_string()))?,
            None => Ok(false),
        }
    }
}
impl Drop for ProbeObserver {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
async fn random_uuid() -> std::io::Result<String> {
    let entropy = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
        .map_err(|e| fail(e.to_string()))?;
    Ok(uuid(
        &entropy.bytes(16).await.map_err(|e| fail(e.to_string()))?,
    ))
}
fn limits() -> ActEngineLimits {
    ActEngineLimits {
        memory_bytes: 28 << 30,
        storage_bytes: 20 << 30,
        nano_cpus: 2_000_000_000,
        pids: 1024,
    }
}
// Next probe profile is inferred from layer/native-copy demand, not a proven minimum.
fn legacy_limits() -> ActEngineLimits {
    ActEngineLimits {
        memory_bytes: 16 << 30,
        storage_bytes: 12 << 30,
        nano_cpus: 2_000_000_000,
        pids: 1024,
    }
}
fn retained_profile(root: &Path, run: &str) -> std::io::Result<ActEngineLimits> {
    let path = root.join(format!("{run}-profile.json"));
    let bytes = match bounded_file(&path, 4096) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(legacy_limits()),
        Err(e) => return Err(e),
    };
    let value: Value = serde_json::from_slice(&bytes)?;
    profile_from_value(&value)
}
fn profile_from_value(value: &Value) -> std::io::Result<ActEngineLimits> {
    for profile in [limits(), legacy_limits()] {
        if value["memory_bytes"].as_u64() == Some(profile.memory_bytes)
            && value["storage_bytes"].as_u64() == Some(profile.storage_bytes)
            && value["nano_cpus"].as_u64() == Some(profile.nano_cpus)
            && value["pids"].as_u64() == Some(profile.pids)
        {
            return Ok(profile);
        }
    }
    Err(fail("retained probe profile is unsupported"))
}
const RESOURCE_COMMAND: &str = "printf 'df_kib\n'; df -Pk /var/lib/docker; printf 'df_inodes\n'; df -Pi /var/lib/docker; printf 'memory_current\n'; cat /sys/fs/cgroup/memory.current; printf 'memory_peak\n'; cat /sys/fs/cgroup/memory.peak";
fn resource_values(raw: &[u8]) -> std::io::Result<Value> {
    let text = std::str::from_utf8(raw).map_err(|_| fail("resource sample is not UTF8"))?;
    let lines = text.lines().collect::<Vec<_>>();
    let row = |name: &str| -> std::io::Result<Vec<u64>> {
        let position = lines
            .iter()
            .position(|line| *line == name)
            .ok_or_else(|| fail("resource marker missing"))?;
        let fields = lines
            .get(position + 2)
            .ok_or_else(|| fail("df row missing"))?
            .split_whitespace()
            .collect::<Vec<_>>();
        if fields.len() != 6 || fields[5] != "/var/lib/docker" {
            return Err(fail("df mount row mismatch"));
        }
        fields[1..4]
            .iter()
            .map(|v| v.parse().map_err(|_| fail("invalid df counter")))
            .collect()
    };
    let bytes = row("df_kib")?
        .into_iter()
        .map(|v| v.checked_mul(1024).ok_or_else(|| fail("df byte overflow")))
        .collect::<Result<Vec<_>, _>>()?;
    let inodes = row("df_inodes")?;
    let memory = |name: &str| -> std::io::Result<u64> {
        let position = lines
            .iter()
            .position(|line| *line == name)
            .ok_or_else(|| fail("memory marker missing"))?;
        lines
            .get(position + 1)
            .ok_or_else(|| fail("memory value missing"))?
            .parse()
            .map_err(|_| fail("invalid memory counter"))
    };
    Ok(
        json!({"tmpfs_total_bytes":bytes[0],"tmpfs_used_bytes":bytes[1],"tmpfs_available_bytes":bytes[2],"inodes_total":inodes[0],"inodes_used":inodes[1],"inodes_available":inodes[2],"memory_current":memory("memory_current")?,"memory_peak":memory("memory_peak")?}),
    )
}
async fn sample_resources(engine: &DockerEngine, id: &str, elapsed: Duration) -> Value {
    let result = engine
        .with_args(["exec", id, "sh", "-c", RESOURCE_COMMAND])
        .capture_async(RunOptions::bounded(Duration::from_secs(2), 16384))
        .await;
    resource_receipt(result.map_err(|e| e.to_string()), id, elapsed)
}
fn resource_receipt(
    result: Result<bosn_engine::CommandResult, String>,
    id: &str,
    elapsed: Duration,
) -> Value {
    let mut value = json!({"schema_version":1,"source":"owned-engine df and private cgroupfs; observer telemetry only","unix_seconds":at(),"elapsed_ms":elapsed.as_millis(),"command":RESOURCE_COMMAND,"engine_id":id});
    match result {
        Ok(result) => {
            value["command_exit"] = json!(result.exit_code);
            value["stderr"] = json!(String::from_utf8_lossy(&result.stderr));
            if result.exit_code == 0 {
                match resource_values(&result.stdout) {
                    Ok(metrics) => value["metrics"] = metrics,
                    Err(e) => value["observer_error"] = json!(e.to_string()),
                }
            } else {
                value["observer_error"] = json!("resource command failed");
            }
        }
        Err(e) => value["observer_error"] = json!(e.to_string()),
    }
    value
}
const NESTED_LIMIT: usize = 8;
const NESTED_RECEIPTS: usize = 24;
fn nested_rows(raw: &[u8]) -> std::io::Result<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for line in raw.split(|b| *b == b'\n').filter(|b| !b.is_empty()) {
        if rows.len() == NESTED_LIMIT {
            return Err(fail("nested container count exceeds diagnostic bound"));
        }
        let row: Value = serde_json::from_slice(line)?;
        let id = row["ID"]
            .as_str()
            .ok_or_else(|| fail("nested ID missing"))?;
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(fail("nested ID is not canonical lower hex"));
        }
        let state = row["State"]
            .as_str()
            .ok_or_else(|| fail("nested state missing"))?;
        if !matches!(
            state,
            "created" | "running" | "paused" | "restarting" | "removing" | "exited" | "dead"
        ) || rows.iter().any(|(old, _)| old == id)
        {
            return Err(fail("invalid or duplicate nested observation"));
        }
        rows.push((id.to_owned(), state.to_owned()));
    }
    Ok(rows)
}
fn nested_receipt(
    result: Result<bosn_engine::CommandResult, String>,
    id: &str,
    elapsed: Duration,
) -> Value {
    let mut receipt = json!({"source":"private engine nested inspect; diagnostic only","unix_seconds":at(),"elapsed_ms":elapsed.as_millis(),"nested_id":id});
    match result {
        Ok(output) => {
            receipt["command_exit"] = json!(output.exit_code);
            receipt["stderr"] = json!(String::from_utf8_lossy(&output.stderr));
            receipt["raw_stdout"] = json!(String::from_utf8_lossy(&output.stdout));
            if output.exit_code == 0 {
                match serde_json::from_slice::<Value>(&output.stdout) {
                    Ok(v) if v.as_array().is_some_and(|a| a.len() == 1) && v[0]["Id"] == id => {
                        receipt["inspection"] = v
                    }
                    _ => receipt["observer_error"] = json!("nested inspect identity/JSON mismatch"),
                }
            } else {
                receipt["observer_error"] = json!("nested inspect failed");
            }
        }
        Err(e) => receipt["observer_error"] = json!(e),
    }
    receipt
}
fn failed_pinned_runner(receipt: &Value) -> bool {
    let v = &receipt["inspection"][0];
    v["Image"] == RUNNER
        && v["ImageManifestDescriptor"]["digest"] == RUNNER
        && v["State"]["ExitCode"] == 127
        && v["State"]["Error"]
            .as_str()
            .is_some_and(|e| e.contains("exec: \"tail\": executable file not found"))
}
const FAILED_RUNNER_PATHS: [(&str, &str); 6] = [
    (
        "loader-target",
        "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
    ),
    ("loader-interpreter", "/lib64/ld-linux-x86-64.so.2"),
    ("lib64-link", "/lib64"),
    ("lib-link", "/lib"),
    ("usr-bin-tail", "/usr/bin/tail"),
    ("bin-tail", "/bin/tail"),
];
async fn capture_tail_archives(
    engine: &DockerEngine,
    outer: &str,
    id: &str,
    directory: &Path,
    deadline: Instant,
) -> std::io::Result<()> {
    private_dir(directory)?;
    for (label, path) in FAILED_RUNNER_PATHS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            retain(
                &directory.join(format!("{label}.json")),
                &serde_json::to_vec_pretty(
                    &json!({"source":"private failed-runner docker cp tar stream; diagnostic only","unix_seconds":at(),"nested_id":id,"container_path":path,"captured":false,"observer_error":"shared diagnostic deadline exhausted"}),
                )?,
            )?;
            continue;
        }
        let source = format!("{id}:{path}");
        let result = engine
            .with_args(["exec", outer, "docker", "cp", &source, "-"])
            .capture_async(RunOptions::bounded(
                remaining.min(Duration::from_secs(2)),
                1 << 20,
            ))
            .await;
        let mut receipt = json!({"source":"private failed-runner docker cp tar stream; diagnostic only","unix_seconds":at(),"nested_id":id,"container_path":path,"byte_ceiling":1<<20,"follow_symlinks":false});
        match result {
            Ok(output) => {
                receipt["captured"] = json!(true);
                receipt["command_exit"] = json!(output.exit_code);
                receipt["stderr"] = json!(String::from_utf8_lossy(&output.stderr));
                receipt["bytes"] = json!(output.stdout.len());
                receipt["sha256"] = json!(digest(&output.stdout));
                retain(&directory.join(format!("{label}.tar")), &output.stdout)?;
                if output.exit_code != 0 {
                    receipt["observer_error"] = json!("private docker cp failed");
                }
            }
            Err(e) => {
                receipt["captured"] = json!(false);
                receipt["observer_error"] = json!(e.to_string());
            }
        }
        retain(
            &directory.join(format!("{label}.json")),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
    }
    Ok(())
}
async fn sample_nested(
    engine: &DockerEngine,
    outer: &str,
    samples: &Path,
    seen: &mut std::collections::BTreeSet<(String, String)>,
    elapsed: Duration,
    budget: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + budget.min(Duration::from_secs(5));
    if budget.is_zero() {
        return Ok(());
    }
    if seen.len() >= NESTED_RECEIPTS {
        return Ok(());
    }
    let list = engine
        .with_args([
            "exec",
            outer,
            "docker",
            "ps",
            "--all",
            "--no-trunc",
            "--format",
            "{{json .}}",
        ])
        .capture_async(RunOptions::bounded(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(2)),
            16384,
        ))
        .await;
    let rows = match list {
        Ok(r) if r.exit_code == 0 => nested_rows(&r.stdout),
        Ok(r) => Err(fail(format!(
            "nested list exit {}: {}",
            r.exit_code,
            String::from_utf8_lossy(&r.stderr)
        ))),
        Err(e) => Err(fail(e.to_string())),
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            let path = samples.join("nested-list-error.json");
            if !path.exists() {
                retain(
                    &path,
                    &serde_json::to_vec_pretty(
                        &json!({"source":"private nested listing; diagnostic only","unix_seconds":at(),"observer_error":e.to_string()}),
                    )?,
                )?;
            }
            return Ok(());
        }
    };
    let mut tail_captured = false;
    for (id, state) in rows {
        if Instant::now() >= deadline {
            break;
        }
        if seen.len() == NESTED_RECEIPTS {
            break;
        }
        if seen.contains(&(id.clone(), state.clone())) {
            continue;
        }
        let result = engine
            .with_args(["exec", outer, "docker", "inspect", &id])
            .capture_async(RunOptions::bounded(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(2)),
                65536,
            ))
            .await;
        let receipt = nested_receipt(result.map_err(|e| e.to_string()), &id, elapsed);
        retain(
            &samples.join(format!("nested-{:02}.json", seen.len())),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        if !tail_captured && failed_pinned_runner(&receipt) && Instant::now() < deadline {
            capture_tail_archives(
                engine,
                outer,
                &id,
                &samples.join(format!("nested-{:02}-tail", seen.len())),
                deadline,
            )
            .await?;
            tail_captured = true;
        }
        seen.insert((id, state));
    }
    Ok(())
}
pub(crate) async fn diagnose_nested_failure(
    registry: &RegistryActor,
    engine: &DockerEngine,
    intent: &ActEngineIntent,
    observed: &ActEngineObservation,
    token: &str,
    evidence: &Path,
    remaining: Duration,
) -> std::io::Result<()> {
    let budget = remaining.min(Duration::from_secs(5));
    if budget.is_zero() {
        return Ok(());
    }
    let start = Instant::now();
    async_engine::timeout(
        budget,
        registry.act_registry(ActRegistryCommand::VerifyClaimed {
            run: intent.run_id.clone(),
            observed: observed.clone(),
            token: token.to_owned(),
        }),
    )
    .await
    .map_err(|e| fail(format!("nested diagnostic authorization deadline: {e}")))?
    .map_err(|e| fail(e.to_string()))?;
    let budget = budget.saturating_sub(start.elapsed());
    if budget.is_zero() {
        return Ok(());
    }
    let directory = evidence.join("nested-failure-diagnostics");
    private_dir(&directory)?;
    sample_nested(
        engine,
        &observed.engine_id,
        &directory,
        &mut std::collections::BTreeSet::new(),
        start.elapsed(),
        budget,
    )
    .await
}
async fn docker(engine: &DockerEngine, args: Vec<String>) -> std::io::Result<Vec<u8>> {
    let result = engine
        .with_args(args)
        .capture_async(RunOptions::bounded(Duration::from_secs(10), 1 << 20))
        .await
        .map_err(|e| fail(e.to_string()))?;
    if result.exit_code != 0 {
        return Err(fail(format!(
            "probe Docker observation refused: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    Ok(result.stdout)
}
fn git(source: &Path, args: &[&str]) -> std::io::Result<Vec<u8>> {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .current_dir(source)
        .args([
            "-c",
            "user.name=Bosn probe",
            "-c",
            "user.email=bosn-probe@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(fail(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(output.stdout)
}
fn source_fixture(
    root: &Path,
    cancel: bool,
) -> std::io::Result<(PathBuf, Vec<u8>, Vec<u8>, String)> {
    let source = root.join(if cancel {
        "cancel-source"
    } else {
        "success-source"
    });
    private_dir(&source)?;
    private_dir(&source.join(".github"))?;
    private_dir(&source.join(".github/workflows"))?;
    let shell = if cancel {
        "printf 'BOSN_REAL_CANCEL_STARTED\\n'\nsleep 300"
    } else {
        "printf 'BOSN_REAL_SUCCESS\\n'\nsleep 2"
    };
    let workflow = format!(
        "name: Bosn real probe\non: push\njobs:\n  probe:\n    runs-on: ubuntu-latest\n    steps:\n      - name: Verify real event and shell\n        shell: bash\n        run: |\n          test \"$GITHUB_SHA\" = \"${{{{ github.event.after }}}}\"\n          test \"${{{{ github.event.repository.full_name }}}}\" = \"bosn/act-local-probe\"\n          {}\n",
        shell.replace('\n', "\n          ")
    );
    retain(
        &source.join(".github/workflows/probe.yml"),
        workflow.as_bytes(),
    )?;
    git(&source, &["init", "--initial-branch=main"])?;
    git(&source, &["add", ".github/workflows/probe.yml"])?;
    git(
        &source,
        &["commit", "-m", "immutable local Act probe fixture"],
    )?;
    let sha = String::from_utf8(git(&source, &["rev-parse", "HEAD"])?)
        .map_err(|e| fail(e.to_string()))?
        .trim()
        .to_owned();
    let archive_path = root.join(if cancel {
        "cancel-source.tar"
    } else {
        "success-source.tar"
    });
    let output = Command::new("tar")
        .args([
            "--format=ustar",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--mtime=@0",
            "-cf",
        ])
        .arg(&archive_path)
        .arg("-C")
        .arg(&source)
        .arg(".github/workflows/probe.yml")
        .output()?;
    if !output.status.success() {
        return Err(fail(String::from_utf8_lossy(&output.stderr)));
    }
    let snapshot = bounded_file(&archive_path, 1 << 20)?;
    let payload = serde_json::to_vec(
        &json!({"ref":"refs/heads/main","before":"0".repeat(40),"after":sha,"head_commit":{"id":sha},"repository":{"id":1,"name":"act-local-probe","full_name":"bosn/act-local-probe","default_branch":"main","html_url":"https://github.com/bosn/act-local-probe","clone_url":"https://github.com/bosn/act-local-probe.git","owner":{"login":"bosn","name":"bosn","type":"User"}},"sender":{"login":"bosn"}}),
    )?;
    Ok((source, snapshot, payload, sha))
}
async fn record(
    registry: &RegistryActor,
    run: &str,
) -> std::io::Result<Option<bosn_registry::act::ActEngineRecord>> {
    let ActRegistryReply::Recovery(page) = registry
        .act_registry(ActRegistryCommand::Pending {
            after_run_id: None,
            limit: 16,
        })
        .await
        .map_err(|e| fail(e.to_string()))?
    else {
        return Err(fail("probe recovery reply missing"));
    };
    if page.next_run_id.is_some() {
        return Err(fail("unexpected recovery page overflow in two-run probe"));
    }
    Ok(page.items.into_iter().find(|r| r.intent.run_id == run))
}
/// Called only after this probe's runtime future has terminated. A persisted
/// live claim alone is never grounds to interrupt another execution owner.
async fn cleanup(
    registry: &RegistryActor,
    engine: &DockerEngine,
    intent: &ActEngineIntent,
    owner: &str,
    proof: &VerifiedEngineManifest,
    profile: ActEngineLimits,
) -> std::io::Result<()> {
    let Some(current) = record(registry, &intent.run_id).await? else {
        return Ok(());
    };
    if current.intent != *intent {
        return Err(fail("probe cleanup immutable intent changed"));
    }
    let image = docker(
        engine,
        vec![
            "image".into(),
            "inspect".into(),
            format!("docker.io/library/docker@{ENGINE}"),
        ],
    )
    .await?;
    let identity = observe_engine_image_from_manifest(&image, intent, proof)
        .map_err(|e| fail(e.to_string()))?;
    let observed = if let Some(id) = &current.engine_id {
        let raw = docker(
            engine,
            vec!["container".into(), "inspect".into(), id.clone()],
        )
        .await?;
        observe_engine(&raw, intent, owner, &identity, profile).map_err(|e| fail(e.to_string()))?
    } else {
        let ids = docker(
            engine,
            vec![
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--filter".into(),
                format!("name=^/{}$", intent.engine_name()),
                "--format".into(),
                "{{.ID}}".into(),
            ],
        )
        .await?;
        let ids = std::str::from_utf8(&ids)
            .map_err(|e| fail(e.to_string()))?
            .split_whitespace()
            .collect::<Vec<_>>();
        if ids.len() > 1 {
            return Err(fail("ambiguous intended engine name"));
        }
        if ids.is_empty() {
            if current.state == ActEngineState::Pending {
                registry
                    .act_registry(ActRegistryCommand::Cleanup {
                        run: intent.run_id.clone(),
                        outcome: ActRunOutcome::Interrupted,
                        at: at(),
                    })
                    .await
                    .map_err(|e| fail(e.to_string()))?;
            }
            registry
                .act_registry(ActRegistryCommand::Finalize {
                    run: intent.run_id.clone(),
                    proof: ActEngineRemovalProof {
                        name: intent.engine_name(),
                        engine_id: None,
                    },
                    at: at(),
                })
                .await
                .map_err(|e| fail(e.to_string()))?;
            return Ok(());
        }
        let raw = docker(
            engine,
            vec!["container".into(), "inspect".into(), ids[0].into()],
        )
        .await?;
        let observed = observe_engine(&raw, intent, owner, &identity, profile)
            .map_err(|e| fail(e.to_string()))?;
        if current.state == ActEngineState::Pending {
            registry
                .act_registry(ActRegistryCommand::Cleanup {
                    run: intent.run_id.clone(),
                    outcome: ActRunOutcome::Interrupted,
                    at: at(),
                })
                .await
                .map_err(|e| fail(e.to_string()))?;
        }
        registry
            .act_registry(ActRegistryCommand::Recover {
                run: intent.run_id.clone(),
                observed: observed.clone(),
                at: at(),
            })
            .await
            .map_err(|e| fail(e.to_string()))?;
        observed
    };
    if current.state == ActEngineState::Registered {
        let command = match current.execution_claim {
            Some(token) => ActRegistryCommand::CleanupClaimed {
                run: intent.run_id.clone(),
                token,
                outcome: ActRunOutcome::Interrupted,
                at: at(),
            },
            None => ActRegistryCommand::Cleanup {
                run: intent.run_id.clone(),
                outcome: ActRunOutcome::Interrupted,
                at: at(),
            },
        };
        // A detached owner guard may have won this same-owner transition.
        let _ = registry.act_registry(command).await;
    }
    remove_owned_engine(registry, engine, &intent.run_id, observed, at())
        .await
        .map_err(|e| fail(e.to_string()))
}
fn frame_marker(path: &Path) -> std::io::Result<bool> {
    let frames = bounded_file(path, OUTPUT + 65536)?;
    frame_marker_bytes(&frames)
}
fn frame_marker_bytes(frames: &[u8]) -> std::io::Result<bool> {
    let mut offset = 0;
    let mut bytes = Vec::new();
    while offset + 5 <= frames.len() {
        let size = u32::from_le_bytes(frames[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if !matches!(frames[offset], 1 | 2) || size > OUTPUT {
            return Err(fail("invalid retained output frame"));
        }
        if offset + 5 + size > frames.len() {
            break;
        }
        bytes.extend_from_slice(&frames[offset + 5..offset + 5 + size]);
        offset += 5 + size;
    }
    Ok(bytes.windows(MARKER.len()).any(|w| w == MARKER))
}
async fn watch_cancellation(
    registry: RegistryActor,
    engine: DockerEngine,
    intent: ActEngineIntent,
    observed: ActEngineObservation,
    artifacts: (PathBuf, PathBuf, bool),
    cancel: CancellationSource,
    stop: CancellationSource,
) -> std::io::Result<bool> {
    let (evidence, samples, cancel_case) = artifacts;
    let mut sampled = 0usize;
    let mut nested_seen = std::collections::BTreeSet::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) && !stop.token().is_cancelled() {
        if let Some(current) = record(&registry, &intent.run_id).await?
            && current.state == ActEngineState::Registered
            && current.execution.is_none()
            && let Some(token) = current.execution_claim
        {
            if let Err(error) = registry
                .act_registry(ActRegistryCommand::VerifyClaimed {
                    run: intent.run_id.clone(),
                    observed: observed.clone(),
                    token,
                })
                .await
            {
                retain(
                    &samples.join("observer-authorization-error.json"),
                    &serde_json::to_vec_pretty(
                        &json!({"source":"probe observer authorization; not runtime evidence","unix_seconds":at(),"observer_error":error.to_string()}),
                    )?,
                )?;
                return Ok(false);
            }
            if sampled < 240 && !stop.token().is_cancelled() {
                let sample = sample_resources(&engine, &observed.engine_id, start.elapsed()).await;
                retain(
                    &samples.join(format!("resources-{sampled:03}.json")),
                    &serde_json::to_vec_pretty(&sample)?,
                )?;
                sampled += 1;
            }
            if !stop.token().is_cancelled() {
                sample_nested(
                    &engine,
                    &observed.engine_id,
                    &samples,
                    &mut nested_seen,
                    start.elapsed(),
                    Duration::from_secs(120)
                        .saturating_sub(start.elapsed())
                        .min(Duration::from_secs(2)),
                )
                .await?;
            }
            if !cancel_case {
                async_engine::sleep(Duration::from_millis(500)).await;
                continue;
            }
            if let Ok(raw) = docker(
                &engine,
                vec![
                    "exec".into(),
                    observed.engine_id.clone(),
                    "docker".into(),
                    "container".into(),
                    "ls".into(),
                    "--no-trunc".into(),
                    "--format".into(),
                    "{{json .}}".into(),
                ],
            )
            .await
            {
                if stop.token().is_cancelled() {
                    return Ok(false);
                }
                let containers = raw
                    .split(|b| *b == b'\n')
                    .filter(|b| !b.is_empty())
                    .map(serde_json::from_slice::<Value>)
                    .collect::<Result<Vec<_>, _>>()?;
                let driver = containers.iter().any(|v| {
                    v["Names"]
                        .as_str()
                        .is_some_and(|s| s == format!("bosn-act-driver-{}", intent.run_id))
                });
                let log = evidence.join(&intent.run_id).join("output.frames");
                if driver && containers.len() >= 2 && log.exists() && frame_marker(&log)? {
                    retain(&samples.join("nested-running.jsonl"), &raw)?;
                    if let Ok(peak) = docker(
                        &engine,
                        vec![
                            "exec".into(),
                            observed.engine_id.clone(),
                            "cat".into(),
                            "/sys/fs/cgroup/memory.peak".into(),
                        ],
                    )
                    .await
                    {
                        retain(&samples.join("memory.peak"), &peak)?;
                    }
                    if let Ok(stats) = docker(
                        &engine,
                        vec![
                            "stats".into(),
                            "--no-stream".into(),
                            "--format".into(),
                            "{{json .}}".into(),
                            observed.engine_id.clone(),
                        ],
                    )
                    .await
                    {
                        retain(&samples.join("docker-stats.json"), &stats)?;
                    }
                    cancel.cancel();
                    return Ok(true);
                }
            }
        }
        async_engine::sleep(Duration::from_millis(250)).await;
    }
    Ok(false)
}
async fn probe_case(
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
    let cleanup_result = cleanup(registry, engine, &intent, owner, engine_manifest, limits()).await;
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
fn live_pinned_act_success_and_cancellation_remove_private_nested_engines() {
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
fn cancellation_marker_requires_valid_complete_frames() {
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
fn aggregate_layer_limit_refuses_before_blob_io() {
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
fn timed_operation_stops_and_joins_owned_observer() {
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

/// Explicit recovery of one retained failed probe, never discovery or creation.
#[test]
#[ignore = "explicit retained-probe recovery; root review and exact owned identity required"]
fn recover_retained_pinned_engine_only() {
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
        let profile=retained_profile(&root,&run).unwrap();
        let result = cleanup(&actor,&DockerEngine::docker(),&intent,&owner,&proof,profile).await;
        actor.stop().await; task.await.unwrap();
        retain(&root.join(format!("{run}-recovery-{}.json",random_uuid().await.unwrap())), &serde_json::to_vec_pretty(&json!({"run_id":run,"registry_id":owner,"verified_cleanup":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string)})).unwrap()).unwrap();
        result.unwrap();
    });
}

#[test]
#[ignore = "explicit retained real inspect fixture; no Docker calls"]
fn retained_created_engine_inspect_is_verified_offline() {
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
    let observed = observe_engine(
        &bounded_file(&root.join("failed-created-engine-inspect.json"), 1 << 20).unwrap(),
        &intent,
        &owner,
        &image,
        legacy_limits(),
    )
    .unwrap();
    assert_eq!(
        observed.engine_id,
        "da29a0806c0d63ac16233240c5f4fb5e8b90c61057accd511c82cc1e6cbbedaa"
    );
}

#[test]
fn resource_sample_preserves_byte_inode_and_memory_axes_and_refuses_bad_data() {
    let raw = b"df_kib\nFilesystem 1024-blocks Used Available Capacity Mounted on\ntmpfs 20971520 1024 20970496 1% /var/lib/docker\ndf_inodes\nFilesystem Inodes IUsed IFree IUse% Mounted on\ntmpfs 1000 800 200 80% /var/lib/docker\nmemory_current\n4096\nmemory_peak\n8192\n";
    let metrics = resource_values(raw).unwrap();
    assert_eq!(metrics["tmpfs_total_bytes"], 20u64 << 30);
    assert_eq!(metrics["tmpfs_used_bytes"], 1 << 20);
    assert_eq!(metrics["inodes_used"], 800);
    assert_eq!(metrics["memory_peak"], 8192);
    for replacement in ["/foreign", "18446744073709551615"] {
        let text = std::str::from_utf8(raw).unwrap().replace(
            if replacement == "/foreign" {
                "/var/lib/docker"
            } else {
                "20971520"
            },
            replacement,
        );
        assert!(resource_values(text.as_bytes()).is_err());
    }
    assert!(resource_values(b"partial observation").is_err());
    assert_eq!(limits().storage_bytes, 20 << 30);
    assert_eq!(limits().memory_bytes, 28 << 30);
    assert_eq!(legacy_limits().storage_bytes, 12 << 30);
}

#[test]
fn observer_failure_cannot_be_promoted_to_resource_measurement_or_runtime_success() {
    let failed = resource_receipt(
        Ok(bosn_engine::CommandResult {
            exit_code: 7,
            stdout: b"invented counters".to_vec(),
            stderr: b"df failed".to_vec(),
        }),
        "owned-id",
        Duration::from_secs(1),
    );
    assert_eq!(failed["command_exit"], 7);
    assert!(failed["metrics"].is_null());
    assert!(failed["observer_error"].is_string());
    assert!(failed["execution_success"].is_null());
    let unavailable =
        resource_receipt(Err("daemon unavailable".into()), "owned-id", Duration::ZERO);
    assert!(unavailable["command_exit"].is_null());
    assert!(unavailable["metrics"].is_null());
    assert_eq!(unavailable["observer_error"], "daemon unavailable");
    for profile in [limits(), legacy_limits()] {
        let value = json!({"memory_bytes":profile.memory_bytes,"storage_bytes":profile.storage_bytes,"nano_cpus":profile.nano_cpus,"pids":profile.pids});
        assert_eq!(profile_from_value(&value).unwrap(), profile);
        for field in ["memory_bytes", "storage_bytes", "nano_cpus", "pids"] {
            let mut wrong = value.clone();
            wrong[field] = json!(0);
            assert!(profile_from_value(&wrong).is_err());
        }
    }
}

#[test]
fn nested_diagnostics_refuse_foreign_identity_and_preserve_failure() {
    let id = "a".repeat(64);
    let rows = serde_json::to_vec(&json!({"ID":id,"State":"created"})).unwrap();
    assert_eq!(
        nested_rows(&rows).unwrap(),
        vec![(id.clone(), "created".into())]
    );
    for bad in ["a".repeat(63), "A".repeat(64), "../foreign".into()] {
        assert!(
            nested_rows(&serde_json::to_vec(&json!({"ID":bad,"State":"created"})).unwrap())
                .is_err()
        );
    }
    let duplicated = [rows.clone(), b"\n".to_vec(), rows].concat();
    assert!(nested_rows(&duplicated).is_err());
    let too_many = (0..9)
        .map(|i| {
            serde_json::to_string(&json!({"ID":format!("{i:064x}"),"State":"created"})).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(nested_rows(too_many.as_bytes()).is_err());
    let output = bosn_engine::CommandResult {
        exit_code: 0,
        stdout: serde_json::to_vec(&json!([{"Id":"b".repeat(64),"Config":{"Env":["PATH=/bin"]}}]))
            .unwrap(),
        stderr: vec![],
    };
    let receipt = nested_receipt(Ok(output), &id, Duration::ZERO);
    assert!(receipt.get("observer_error").is_some());
    assert!(receipt.get("inspection").is_none());
    assert!(receipt.get("execution_success").is_none());
    let failure = nested_receipt(Err("capture deadline exceeded".into()), &id, Duration::ZERO);
    assert_eq!(failure["observer_error"], "capture deadline exceeded");
}

#[test]
fn nested_diagnostic_fake_transport_retains_once_per_state_without_docker() {
    let root = std::env::temp_dir().join(format!(
        "bosn-nested-diagnostic-fixture-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    private_dir(&root).unwrap();
    let id = "c".repeat(64);
    let script = format!(
        "case \"$4\" in ps) printf '%s\n' '{{\"ID\":\"{id}\",\"State\":\"created\"}}';; inspect) printf '%s\n' '[{{\"Id\":\"{id}\",\"Config\":{{\"Env\":[\"PATH=/usr/bin:/bin\"]}},\"State\":{{\"Status\":\"created\"}},\"Mounts\":[]}}]';; *) exit 93;; esac"
    );
    let engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", &script, "fixture"]);
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..2 {
                sample_nested(
                    &engine,
                    "owned-outer",
                    &root,
                    &mut seen,
                    Duration::ZERO,
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
            }
            assert_eq!(seen.len(), 1);
            let exited_script = script.replace("created", "exited");
            let exited =
                DockerEngine::synthetic_for_test("/bin/sh", ["-c", &exited_script, "fixture"]);
            sample_nested(
                &exited,
                "owned-outer",
                &root,
                &mut seen,
                Duration::ZERO,
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert_eq!(seen.len(), 2);
            let forbidden = DockerEngine::synthetic_for_test(
                "/nonexistent-do-not-spawn",
                std::iter::empty::<String>(),
            );
            sample_nested(
                &forbidden,
                "owned-outer",
                &root,
                &mut seen,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap();

            let receipt: Value = serde_json::from_slice(
                &bounded_file(&root.join("nested-00.json"), 1 << 20).unwrap(),
            )
            .unwrap();
            assert_eq!(
                receipt["inspection"][0]["Config"]["Env"][0],
                "PATH=/usr/bin:/bin"
            );
            assert!(root.join("nested-01.json").exists());
            assert!(!root.join("nested-02.json").exists());
        });
}

#[test]
fn tail_archive_fake_transport_checks_exact_paths_and_preserves_binary_tar() {
    let root = std::env::temp_dir().join(format!(
        "bosn-tail-diagnostic-fixture-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    private_dir(&root).unwrap();
    let id = "d".repeat(64);
    std::os::unix::fs::symlink("/not-followed-loader-target", root.join("fixture-link")).unwrap();
    let allowed = FAILED_RUNNER_PATHS
        .iter()
        .map(|(_, p)| format!("{id}:{p}"))
        .collect::<Vec<_>>()
        .join("|");
    let script = format!(
        "[ \"$1\" = exec ] && [ \"$2\" = owned-outer ] && [ \"$3\" = docker ] && [ \"$4\" = cp ] && [ \"$6\" = - ] && [ -z \"$7\" ] || exit 94; case \"$5\" in {allowed}) printf '%s\\n' \"$5\" >> \"$FIXTURE_DIR/order\"; /usr/bin/tar --format=ustar -cf - -C \"$FIXTURE_DIR\" fixture-link;; *) exit 95;; esac"
    );
    let engine = DockerEngine::synthetic_for_test("/bin/sh", ["-c", &script, "fixture"])
        .env("FIXTURE_DIR", root.as_os_str());
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            capture_tail_archives(
                &engine,
                "owned-outer",
                &id,
                &root.join("archives"),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
        });
    for (label, _) in FAILED_RUNNER_PATHS {
        let bytes =
            bounded_file(&root.join("archives").join(format!("{label}.tar")), 1 << 20).unwrap();
        assert!(bytes.len() >= 1024);
        assert_eq!(bytes[156], b'2');
        assert!(bytes[157..257].starts_with(b"/not-followed-loader-target"));
        let receipt: Value = serde_json::from_slice(
            &bounded_file(&root.join("archives").join(format!("{label}.json")), 4096).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["command_exit"], 0);
        assert_eq!(receipt["sha256"], digest(&bytes));
        assert_eq!(receipt["bytes"], bytes.len());
        assert!(receipt.get("execution_success").is_none());
    }
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let failed = DockerEngine::synthetic_for_test(
                "/bin/sh",
                ["-c", "printf missing >&2; exit 17", "fixture"],
            );
            capture_tail_archives(
                &failed,
                "owned-outer",
                &id,
                &root.join("failed"),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
            let receipt: Value = serde_json::from_slice(
                &bounded_file(&root.join("failed/usr-bin-tail.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["command_exit"], 17);
            assert!(receipt.get("observer_error").is_some());
            assert!(receipt.get("execution_success").is_none());
            let oversized = DockerEngine::synthetic_for_test(
                "/bin/sh",
                ["-c", "head -c 1048577 /dev/zero", "fixture"],
            );
            capture_tail_archives(
                &oversized,
                "owned-outer",
                &id,
                &root.join("oversized"),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
            let overflow: Value = serde_json::from_slice(
                &bounded_file(&root.join("oversized/loader-target.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(overflow["captured"], false);
            assert!(overflow.get("observer_error").is_some());
            assert!(!root.join("oversized/loader-target.tar").exists());
            let forbidden = DockerEngine::synthetic_for_test(
                "/nonexistent-do-not-spawn",
                std::iter::empty::<String>(),
            );
            capture_tail_archives(
                &forbidden,
                "owned-outer",
                &id,
                &root.join("exhausted"),
                Instant::now(),
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read_dir(root.join("exhausted")).unwrap().count(),
                6
            );
            let skipped: Value = serde_json::from_slice(
                &bounded_file(&root.join("exhausted/loader-target.json"), 4096).unwrap(),
            )
            .unwrap();
            assert_eq!(skipped["captured"], false);
            assert!(!root.join("exhausted/loader-target.tar").exists());
        });
    assert_eq!(
        std::fs::read_to_string(root.join("order"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        FAILED_RUNNER_PATHS
            .iter()
            .map(|(_, p)| format!("{id}:{p}"))
            .collect::<Vec<_>>()
    );
    let good = json!({"inspection":[{"Image":RUNNER,"ImageManifestDescriptor":{"digest":RUNNER},"State":{"ExitCode":127,"Error":"exec: \"tail\": executable file not found in $PATH"}}]});
    assert!(failed_pinned_runner(&good));
    for field in ["Image", "ImageManifestDescriptor", "State"] {
        let mut foreign = good.clone();
        foreign["inspection"][0][field] = json!(null);
        assert!(!failed_pinned_runner(&foreign));
    }
}
