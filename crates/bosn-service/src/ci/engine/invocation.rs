//! The act command line, built only from validated semantic fields.

use super::{ENGINE_CACHE, ENGINE_WORK, RunScope, SecretEnv, runner_tag, runner_tools};

/// The act command, built only from validated semantic fields.
#[derive(Clone, Debug)]
pub struct ActInvocation {
    pub event: String,
    pub workflow: String,
    /// bosn rewrote the workflow, so act runs the overlay's copy (#424).
    pub workflow_overlaid: bool,
    pub job: Option<String>,
    /// Typed repository route; cohort selection requires verified enrollment.
    pub cache_route: super::super::cache_cohort::CacheRoute,
    /// Passed to act as `-s NAME`; values travel only in the docker client's
    /// environment (`exec --env NAME`), never in argv.
    pub secrets: SecretEnv,
    /// act `--input`, `--matrix` and `--env` (#430), validated at submit.
    pub params: super::super::params::RunParams,
    /// The run's own cgroup, network, directories, ports and Docker proxy
    /// inside the engine (#547); `None` on an engine without a socket
    /// directory, where the run owns the whole engine.
    pub scope: Option<RunScope>,
}

/// The `runs-on` labels act runs locally, all on the pinned runner image;
/// any other is unsupported ([`super::super::matrix_runner`] decides it per matrix
/// leg).
pub const LOCAL_RUNNER_LABELS: [&str; 3] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"];

impl ActInvocation {
    /// The workflow act plans: bosn's rewrite when there is one. The
    /// workspace jobs check out always holds the original (#424).
    pub fn workflow_arg(&self) -> String {
        if self.workflow_overlaid {
            format!("{}/overlay/{}", self.work(), self.workflow)
        } else {
            self.workflow.clone()
        }
    }

    /// Arguments after `act`. Platform mappings cover the Linux labels; any
    /// other `runs-on` is reported unsupported by act and never passes.
    /// The run's work tree in the engine: its scope's, or the engine's own.
    pub fn work(&self) -> String {
        self.scope
            .as_ref()
            .map_or_else(|| ENGINE_WORK.to_owned(), RunScope::work)
    }

    pub fn args(&self) -> Vec<String> {
        let work = self.work();
        let mut args = vec![
            self.event.clone(),
            "-W".into(),
            self.workflow_arg(),
            // Local reusable workflows and composite actions bosn rewrote.
            "--workflow-overlay".into(),
            format!("{work}/overlay"),
            "--eventpath".into(),
            format!("{work}/event.json"),
            "--json".into(),
            "--pull=false".into(),
            "--action-cache-path".into(),
            format!("{ENGINE_CACHE}/actions"),
            // Legacy in-place checkouts race between concurrent runs
            // (zackees/clud#1724); the new cache extracts per run.
            "--use-new-action-cache".into(),
            // Per run, so concurrent runs never share artifacts or ports.
            "--artifact-server-path".into(),
            format!("{work}/artifacts"),
        ];
        if let Some(scope) = &self.scope {
            args.extend(scope.act_args());
        }
        args.extend(self.cache_route.args());
        args.extend(["--env".into(), runner_tools::path_env()]);
        let runner = runner_tag();
        for label in LOCAL_RUNNER_LABELS {
            args.push("-P".into());
            args.push(format!("{label}={runner}"));
        }
        for (name, _) in &self.secrets.0 {
            args.push("-s".into());
            args.push(name.clone());
        }
        if let Some(job) = &self.job {
            args.push("-j".into());
            args.push(job.clone());
        }
        args.extend(self.params.act_args());
        args
    }
}
