//! Attach a task job to its runner slot (#358): CPU and memory limits on the
//! setup container, the per-job Docker accounting proxy, and teardown after
//! the task, whatever its outcome.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use bosn_engine::{DockerEngine, RunOptions};
use kernal_api::async_engine;

use crate::{
    RunContext,
    docker_proxy::{DockerProxy, ProxySettings},
    runners::{CacheKeys, CacheRule, JobVolumes, short_hash},
};

const CONTROL_DEADLINE: Duration = Duration::from_secs(30);
const CONTROL_OUTPUT: usize = 64 * 1024;

/// A running task's runner resources; [`Self::finish`] releases them.
pub(crate) struct RunnerAttachment {
    context: RunContext,
    proxy: Option<DockerProxy>,
    /// `(name, value)` pairs for the exec environment.
    pub env: Vec<(String, String)>,
}

/// Apply the slot's limits to `container` and, for a stack that mounts the
/// host Docker socket, start the job's proxy. Every step that fails degrades
/// to "run without it" with a log line: accounting must never be the reason
/// a task cannot run.
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) async fn attach(
    context: &RunContext,
    container: &str,
    workspace: &Path,
    proxy_dir: Option<&str>,
    rules: &[CacheRule],
    logs: &async_engine::Sender<String>,
) -> RunnerAttachment {
    let runners = &context.runners;
    let record = &context.record;
    let capacity = runners.capacity();
    let job = record.job_id;
    runners.update(job, |r| r.container = Some(container.to_owned()));
    let slot = record.slot.map_or_else(
        || "-".to_owned(),
        |s| format!("{}/{}", s + 1, capacity.runner_slots),
    );
    let mut limits = Vec::new();
    if capacity.cpus_per_slot > 0.0 {
        limits.push(format!("--cpus={}", capacity.cpus_per_slot));
    }
    if let Some(memory) = capacity.memory_per_slot {
        // Swap equal to memory: the limit is a real ceiling.
        limits.push(format!("--memory={memory}"));
        limits.push(format!("--memory-swap={memory}"));
    }
    let mut summary = format!("[bosn] runner slot {slot}, run {}", record.run);
    if !limits.is_empty() {
        let mut args = vec!["update".to_owned()];
        args.extend(limits.iter().cloned());
        args.push(container.to_owned());
        match DockerEngine::docker()
            .with_args(args)
            .capture_async(RunOptions::bounded(CONTROL_DEADLINE, CONTROL_OUTPUT))
            .await
        {
            Ok(result) if result.ok() => {
                summary.push_str(&format!(", limits {}", limits.join(" ")))
            }
            Ok(result) => summary.push_str(&format!(
                ", limits NOT applied: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            )),
            Err(error) => summary.push_str(&format!(", limits NOT applied: {error}")),
        }
    }
    let _ = logs.send(summary).await;
    let mut env = vec![
        ("BOSN_JOB_ID".to_owned(), job.to_string()),
        ("BOSN_RUN".to_owned(), record.run.clone()),
        (
            "BOSN_RUNNER_CPUS".to_owned(),
            if capacity.cpus_per_slot > 0.0 {
                capacity.cpus_per_slot.to_string()
            } else {
                String::new()
            },
        ),
    ];
    if let Some(slot) = record.slot {
        env.push(("BOSN_RUNNER_SLOT".to_owned(), slot.to_string()));
    }
    let proxy = match (proxy_dir, runners.api(), runners.proxy_socket(job)) {
        (Some(_), Some(api), Some(socket)) => {
            let keys = cache_keys(workspace).await;
            let notes_sender = logs.clone();
            let notes: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |line: String| {
                let _ = notes_sender.try_send(line);
            });
            let volumes = JobVolumes::new(
                Arc::clone(runners),
                job,
                rules.to_vec(),
                keys,
                Some(Arc::clone(&notes)),
            );
            let settings = ProxySettings {
                upstream: api.socket().to_owned(),
                run: record.run.clone(),
                name_suffix: crate::runners::run_suffix(&record.run),
                labels: runners.labels(record),
                nano_cpus: capacity.nano_cpus(),
                memory: capacity.memory_per_slot,
                volumes: Arc::new(volumes),
                activity: Arc::clone(&context.activity),
                notes: Some(notes),
            };
            match DockerProxy::start(&socket, settings) {
                Ok(proxy) => {
                    // The proxy directory must be visible inside the
                    // container (a stale bind, or a non-root image user,
                    // can hide it). Otherwise run unproxied.
                    let visible = DockerEngine::docker()
                        .with_args(["exec", container, "test", "-S"])
                        .with_args([socket.as_os_str()])
                        .capture_async(RunOptions::bounded(CONTROL_DEADLINE, CONTROL_OUTPUT))
                        .await
                        .is_ok_and(|result| result.ok());
                    if visible {
                        let path = socket.to_string_lossy().into_owned();
                        runners.update(job, |r| r.proxy_socket = Some(path.clone()));
                        env.push(("DOCKER_HOST".to_owned(), format!("unix://{path}")));
                        let _ = logs
                            .send(format!(
                                "[bosn] docker proxy for this job: containers it creates are labelled run={} and limited to the slot; caches: {}",
                                record.run,
                                describe_rules(rules)
                            ))
                            .await;
                        Some(proxy)
                    } else {
                        let _ = logs
                            .send("[bosn] docker proxy socket is not visible in the container; running without Docker accounting".into())
                            .await;
                        None
                    }
                }
                Err(error) => {
                    let _ = logs
                        .send(format!(
                            "[bosn] docker proxy could not start ({error}); running without Docker accounting"
                        ))
                        .await;
                    None
                }
            }
        }
        _ => None,
    };
    RunnerAttachment {
        context: context.clone(),
        proxy,
        env,
    }
}

impl RunnerAttachment {
    /// Stop the proxy and remove everything the job created (containers,
    /// networks, non-cache volumes), then log what was removed.
    pub(crate) async fn finish(mut self, logs: &async_engine::Sender<String>) {
        let proxied = self.proxy.is_some();
        if let Some(mut proxy) = self.proxy.take() {
            proxy.stop();
        }
        let runners = Arc::clone(&self.context.runners);
        let run = self.context.record.run.clone();
        let started = Instant::now();
        let teardown = async_engine::launch_blocking(move || runners.teardown(&run))
            .await
            .unwrap_or_default();
        if proxied || !teardown.is_empty() {
            let line = format!(
                "[bosn] teardown of run {} in {} ms: {}",
                self.context.record.run,
                started.elapsed().as_millis(),
                teardown.summary()
            );
            if !teardown.failures.is_empty() {
                eprintln!("bosn: job {} {}", self.context.record.job_id, &line[7..]);
            }
            let _ = logs.send(line).await;
        }
    }
}

fn describe_rules(rules: &[CacheRule]) -> String {
    if rules.is_empty() {
        return "none".into();
    }
    rules
        .iter()
        .map(|rule| {
            let target = rule
                .volume
                .as_deref()
                .or(rule.destination.as_deref())
                .unwrap_or("?");
            format!("{}={target} ({:?}, {:?})", rule.name, rule.mode, rule.scope).to_lowercase()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Hashed cache keys: the repository is its `origin` URL when git knows one
/// (so every clone and worktree shares), else the checkout path.
async fn cache_keys(workspace: &Path) -> CacheKeys {
    let path = workspace.to_string_lossy().into_owned();
    let lookup = workspace.to_owned();
    let origin = async_engine::launch_blocking(move || {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&lookup)
            .args(["config", "--get", "remote.origin.url"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|url| !url.is_empty())
    })
    .await
    .ok()
    .flatten();
    CacheKeys {
        repo: format!(
            "r{}",
            short_hash(origin.as_deref().unwrap_or(&path).as_bytes(), 10)
        ),
        workspace: format!("w{}", short_hash(path.as_bytes(), 10)),
    }
}
