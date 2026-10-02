//! Resource samples and nested-engine diagnostics retained as probe evidence.

use super::*;

pub(super) const RESOURCE_COMMAND: &str = "printf 'df_kib\n'; df -Pk /var/lib/docker; printf 'df_inodes\n'; df -Pi /var/lib/docker; printf 'memory_current\n'; cat /sys/fs/cgroup/memory.current; printf 'memory_peak\n'; cat /sys/fs/cgroup/memory.peak; printf 'mountinfo\\n'; cat /proc/self/mountinfo";
pub(super) fn resource_values(raw: &[u8]) -> std::io::Result<Value> {
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
pub(super) async fn sample_resources(engine: &DockerEngine, id: &str, elapsed: Duration) -> Value {
    let result = engine
        .with_args(["exec", id, "sh", "-c", RESOURCE_COMMAND])
        .capture_async(RunOptions::bounded(Duration::from_secs(2), 16384))
        .await;
    resource_receipt(result.map_err(|e| e.to_string()), id, elapsed)
}
pub(super) fn resource_receipt(
    result: Result<bosn_engine::CommandResult, String>,
    id: &str,
    elapsed: Duration,
) -> Value {
    let mut value = json!({"schema_version":1,"source":"owned-engine df and private cgroupfs; observer telemetry only","unix_seconds":at(),"elapsed_ms":elapsed.as_millis(),"command":RESOURCE_COMMAND,"engine_id":id});
    match result {
        Ok(result) => {
            value["command_exit"] = json!(result.exit_code);
            value["stderr"] = json!(String::from_utf8_lossy(&result.stderr));
            value["stdout"] = json!(String::from_utf8_lossy(&result.stdout));
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
pub(super) const NESTED_LIMIT: usize = 8;
pub(super) const NESTED_RECEIPTS: usize = 24;
pub(super) fn nested_rows(raw: &[u8]) -> std::io::Result<Vec<(String, String)>> {
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
pub(super) fn nested_receipt(
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
pub(super) fn failed_pinned_runner(receipt: &Value) -> bool {
    let v = &receipt["inspection"][0];
    v["Image"] == RUNNER
        && v["ImageManifestDescriptor"]["digest"] == RUNNER
        && v["State"]["ExitCode"] == 127
        && v["State"]["Error"]
            .as_str()
            .is_some_and(|e| e.contains("exec: \"tail\": executable file not found"))
}
pub(super) const FAILED_RUNNER_PATHS: [(&str, &str); 6] = [
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
pub(super) async fn capture_tail_archives(
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
pub(super) async fn sample_nested(
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
pub(super) async fn docker(engine: &DockerEngine, args: Vec<String>) -> std::io::Result<Vec<u8>> {
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
