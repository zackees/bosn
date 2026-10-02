//! Client-side CI work: planning, snapshotting into the daemon's staging
//! area, and building typed submissions (shared by CLI, MCP and Python).

use std::path::PathBuf;

use kernal_api::async_engine;

use super::{
    provider::{self, Mode, Provider, Trigger},
    reply::Plan,
    snapshot::{self, Head},
    store::Store,
    wire::{CiError, SubmitRequest, new_uuid},
};
use crate::Error;

/// Client-side submission inputs (CLI, MCP, Python).
#[derive(Clone, Debug, Default)]
pub struct SubmitOptions {
    pub workspace: PathBuf,
    pub provider: Option<Provider>,
    pub engine: Option<String>,
    pub workflow: Option<String>,
    pub job: Option<String>,
    pub trigger: Option<Trigger>,
    pub mode: Option<Mode>,
    pub actor: Option<String>,
    /// When given, must equal the workspace `HEAD`.
    pub sha: Option<String>,
    pub pr_number: Option<u64>,
    pub timeout_secs: Option<u64>,
    /// Opt-in daemon-owned secrets by name (`github_token`).
    pub secrets: Vec<String>,
}

/// Who is submitting: `BOSN_CI_ACTOR`, else an agent session when running
/// under an agent (Claude Code / clud), else `human`.
pub fn detect_actor() -> String {
    if let Ok(actor) = std::env::var("BOSN_CI_ACTOR")
        && !actor.trim().is_empty()
    {
        return actor.trim().chars().take(200).collect();
    }
    let session = ["CLAUDE_SESSION_ID", "CLUD_SESSION_ID", "CODEX_SESSION_ID"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    if let Some(session) = session {
        return format!("agent:{}", session.chars().take(100).collect::<String>());
    }
    if std::env::var_os("CLAUDECODE").is_some() || std::env::var_os("CLUD").is_some() {
        return "agent:claude".into();
    }
    "human".into()
}

fn refuse(message: impl Into<String>) -> Error {
    CiError::refused(message).into()
}

/// Provider, workflow and `HEAD` resolved for one set of options.
struct Resolved {
    head: Head,
    provider: Provider,
    workflow: String,
    trigger: Trigger,
    mode: Mode,
}

fn resolve(options: &SubmitOptions) -> Result<Resolved, Error> {
    let head = snapshot::head(&options.workspace).map_err(|e| refuse(e.to_string()))?;
    let provider = provider::detect(&head.root, options.provider).map_err(refuse)?;
    provider::require_supported(provider).map_err(refuse)?;
    if let Some(sha) = &options.sha
        && !sha.eq_ignore_ascii_case(&head.sha)
    {
        return Err(refuse(format!(
            "--sha {sha} is not the workspace HEAD ({})",
            head.sha
        )));
    }
    let trigger = options.trigger.unwrap_or(Trigger::Push);
    let mode = options.mode.unwrap_or(Mode::Minimal);
    provider::validate(trigger, mode, head.dirty).map_err(refuse)?;
    Ok(Resolved {
        workflow: provider::github_workflow(&head.root, options.workflow.as_deref())
            .map_err(refuse)?,
        head,
        provider,
        trigger,
        mode,
    })
}

/// What `bosn ci run` would execute, without copying or contacting the
/// daemon.
pub fn plan(options: &SubmitOptions) -> Result<Plan, Error> {
    let resolved = resolve(options)?;
    let head = resolved.head;
    let repository = provider::repository(head.origin.as_deref());
    let (event, payload) = provider::github_event(
        resolved.trigger,
        resolved.mode,
        &head.sha,
        head.branch.as_deref(),
        &repository,
        options.pr_number.unwrap_or(1),
    );
    Ok(Plan {
        schema_version: super::SCHEMA_VERSION,
        workspace: head.root,
        provider: resolved.provider,
        engine: options.engine.clone().unwrap_or_else(|| "act".into()),
        workflow: resolved.workflow,
        job: options.job.clone(),
        trigger: resolved.trigger,
        mode: resolved.mode,
        event: event.into(),
        payload,
        repository,
        sha: head.sha,
        branch: head.branch,
        dirty: head.dirty,
        actor: options.actor.clone().unwrap_or_else(detect_actor),
    })
}

/// Resolve the options, snapshot the workspace into the daemon's staging
/// area, and build the typed submission.
pub async fn stage_submission(
    state_dir: &std::path::Path,
    options: SubmitOptions,
) -> Result<SubmitRequest, Error> {
    let resolved = resolve(&options)?;
    let staging_id = new_uuid().await?;
    let staging = Store::new(state_dir).staging(&staging_id);
    std::fs::create_dir_all(&staging)?;
    let root = resolved.head.root.clone();
    let source = staging.join("source");
    let receipt = async_engine::launch_blocking(move || snapshot::snapshot(&root, &source))
        .await
        .map_err(|_| Error::ActorClosed)?;
    let receipt = receipt.map_err(|error| {
        let _ = std::fs::remove_dir_all(&staging);
        refuse(format!("snapshot failed: {error}"))
    })?;
    Ok(SubmitRequest {
        staging: staging_id,
        workspace: resolved.head.root.to_string_lossy().into_owned(),
        provider: resolved.provider,
        engine: options.engine.unwrap_or_else(|| "act".into()),
        workflow: resolved.workflow,
        job: options.job,
        trigger: resolved.trigger,
        mode: resolved.mode,
        actor: options.actor.unwrap_or_else(detect_actor),
        sha: receipt.sha,
        branch: receipt.branch,
        tree_digest: receipt.tree_digest,
        dirty: receipt.dirty,
        origin: receipt.origin,
        pr_number: options.pr_number,
        timeout_secs: options.timeout_secs,
        secrets: options.secrets,
    })
}
