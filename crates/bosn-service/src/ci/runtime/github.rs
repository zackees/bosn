//! `bosn ci run --github-proxy` in the run's life (#650): started before the
//! run is planned, its fake token added as the run's `GITHUB_TOKEN` secret.

use std::sync::{Arc, OnceLock};

use super::*;
use crate::ci::github::RunGithub;
use crate::github_proxy::ResponseCache;

/// The daemon's ETag cache, shared by every run's proxy.
fn cache() -> Arc<ResponseCache> {
    static CACHE: OnceLock<Arc<ResponseCache>> = OnceLock::new();
    CACHE.get_or_init(Arc::default).clone()
}

impl CiRuntime {
    /// Start the run's GitHub proxy and give every job its bootstrap step,
    /// when the run asked for it.
    pub(super) async fn start_github(
        &self,
        record: &RunRecord,
        observer: &mut RunObserver,
    ) -> Result<Option<Arc<RunGithub>>, String> {
        let Some(writes) = record.github_proxy else {
            return Ok(None);
        };
        let github = RunGithub::start(&self.state_dir, &record.repository, writes, cache()).await?;
        let jobs = crate::github_bootstrap::inject(
            &self.store.source(&record.id),
            &self.store.overlay(&record.id),
            &github.bootstrap_step(),
        )
        .map_err(|e| format!("github proxy bootstrap: {e}"))?;
        observer.proxy_logs = github.take_logs();
        observer.note(&github.summary());
        observer.note(&format!(
            "{jobs} job(s) start with a step that routes api.github.com through the run's GitHub proxy"
        ));
        Ok(Some(Arc::new(github)))
    }
}

/// The plan with the run's proxy: its fake token is the job's `GITHUB_TOKEN`.
pub(super) fn with_github(mut plan: EnginePlan, github: Option<Arc<RunGithub>>) -> EnginePlan {
    if let Some(github) = &github {
        plan.invocation.secrets.0.push(github.secret());
    }
    plan.github = github;
    plan
}

/// Every value the run log masks: the run's secrets, plus the proxy's fake
/// and real tokens.
pub(super) fn masked(plan: &EnginePlan) -> Vec<String> {
    let mut values: Vec<String> = plan
        .invocation
        .secrets
        .0
        .iter()
        .map(|(_, v)| v.clone())
        .collect();
    if let Some(github) = &plan.github {
        values.extend(github.masked());
    }
    values
}
