//! The act command line, built only from validated semantic fields.

use bosn_core::runner_images::{RunnerSource, RunnerTable};

use super::{ENGINE_CACHE, ENGINE_WORK, RunScope, SecretEnv, runner_tools};

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
    /// inside the shared engine (#547); the lease sets its slot. `None` only
    /// in tests that drive act without serve.
    pub scope: Option<RunScope>,
    /// The `runs-on` labels act maps (`-P label=tag`), each to its image from
    /// the run's [`RunnerTable`]; any other label is unsupported. Images other
    /// than the default are built or loaded before the run
    /// ([`super::runner_images`]).
    pub runners: Vec<(String, RunnerSource)>,
}

/// The labels every run maps: the built-in ones on the default image, which
/// `act serve prepare` loads anyway.
pub fn default_runners() -> Vec<(String, RunnerSource)> {
    RunnerTable::builtin()
        .entries()
        .filter(|(_, source)| source.is_default())
        .map(|(label, source)| (label.to_owned(), source.clone()))
        .collect()
}

/// The labels a run maps: the default ones, plus every other table entry
/// whose label the repository's workflow text names (`texts`), so a run
/// builds or loads only the images it can use.
pub fn runners_for<'a>(
    table: &RunnerTable,
    texts: impl IntoIterator<Item = &'a str> + Clone,
) -> Vec<(String, RunnerSource)> {
    table
        .entries()
        .filter(|(label, source)| {
            source.is_default() || texts.clone().into_iter().any(|t| names_label(t, label))
        })
        .map(|(label, source)| (label.to_owned(), source.clone()))
        .collect()
}

/// Whether `text` names `label` as a whole token, so `ubuntu-26.04-arm`
/// does not count as `ubuntu-26.04`.
fn names_label(text: &str, label: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_');
    text.match_indices(label).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let mut rest = text[at + label.len()..].chars();
        let continues = match rest.next() {
            Some('.') => rest.next().is_some_and(word),
            Some(c) => word(c),
            None => false,
        };
        !before.is_some_and(|c| word(c) || c == '.') && !continues
    })
}

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
        for (label, source) in &self.runners {
            args.push("-P".into());
            args.push(format!("{label}={}", source.tag()));
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
