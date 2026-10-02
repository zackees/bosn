//! The steps a GitHub workflow declares, read from the frozen snapshot.
//!
//! act prints nothing for a step whose `if:` is false, so the run tree would
//! silently omit it. Knowing every declared step lets a finished job list the
//! ones that never ran as `skipped`. Step identity matches act's `stepID`:
//! the step's `id:` when it has one, otherwise its index.
//!
//! The same typed reading names the jobs that only run on GitHub
//! ([`super::remote_only`], GATE-012), so a finished run reports them as
//! `remote_only` with their reason.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

/// One declared step of one job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredStep {
    pub id: String,
    pub name: String,
}

/// Declared steps by workflow job ID.
pub type DeclaredSteps = BTreeMap<String, Vec<DeclaredStep>>;

/// What a run's workflow declares, read once when the run ends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declared {
    pub steps: DeclaredSteps,
    /// Remote-only jobs (GATE-012) by workflow job ID, with the reason.
    pub remote_only: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Workflow {
    #[serde(default)]
    jobs: BTreeMap<String, Job>,
}

/// One workflow job, read only for the fields bosn acts on.
#[derive(Debug, Default, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub env: BTreeMap<String, serde_yaml::Value>,
    pub permissions: Option<Permissions>,
}

/// `permissions:` is `read-all`/`write-all` or a map of scopes to levels.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Permissions {
    All(String),
    Scopes(BTreeMap<String, String>),
}

#[derive(Debug, Default, Deserialize)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    pub uses: Option<String>,
    pub run: Option<String>,
}

impl Step {
    /// act's display name: `name`, else `uses`, else the `run` text as
    /// written (without the trap bosn added to the run's copy).
    fn display(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.uses.clone())
            .or_else(|| {
                self.run
                    .as_deref()
                    .map(|r| super::flush::untrap(r).trim().to_string())
            })
            .unwrap_or_default()
    }
}

/// Parse the workflow file's jobs. A workflow that cannot be read or parsed
/// yields no declarations (act reports what it can on its own).
pub fn declared(source: &Path, workflow: &str) -> Declared {
    std::fs::read_to_string(source.join(workflow))
        .ok()
        .and_then(|text| parse(&text))
        .unwrap_or_default()
}

fn parse(text: &str) -> Option<Declared> {
    let workflow: Workflow = serde_yaml::from_str(text).ok()?;
    let mut declared = Declared::default();
    for (id, job) in workflow.jobs {
        if let Some(reason) = super::remote_only::reason(&job) {
            declared.remote_only.insert(id.clone(), reason);
        }
        let steps = job
            .steps
            .iter()
            .enumerate()
            // bosn's own gate (#404) is no step of the job.
            .filter(|(_, step)| step.id.as_deref() != Some(super::matrix_runner::STEP_ID))
            .map(|(index, step)| DeclaredStep {
                id: step.id.clone().unwrap_or_else(|| index.to_string()),
                name: step.display(),
            })
            .collect();
        declared.steps.insert(id, steps);
    }
    Some(declared)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #405: the run's localized copy has bosn's end-of-output trap on every
    /// `run:` script; a declared (e.g. skipped) step keeps its written name.
    #[test]
    fn a_trapped_copy_declares_the_steps_as_written() {
        let written = include_str!("../../tests/fixtures/act/act-0.2.88-unnamed-run-steps.yml");
        let mut localized: serde_yaml::Value = serde_yaml::from_str(written).unwrap();
        assert_eq!(super::super::flush::add_traps(&mut localized), 4);
        let localized = parse(&serde_yaml::to_string(&localized).unwrap()).unwrap();
        assert_eq!(localized, parse(written).unwrap());
        let names: Vec<&str> = localized.steps["a"]
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "echo one",
                "Named",
                "echo never",
                "printf 'fatal: no newline' >&2\nexit 3"
            ]
        );
    }

    #[test]
    fn steps_use_explicit_ids_else_their_index() {
        let declared = parse(
            "on: [push]\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo one\n      - id: cache\n        uses: actions/cache@v4\n      - name: Named\n        if: false\n        run: echo never\n  b:\n    uses: ./.github/workflows/reusable.yml\n",
        )
        .unwrap();
        assert!(declared.remote_only.is_empty());
        let declared = declared.steps;
        assert_eq!(
            declared["a"],
            [
                DeclaredStep {
                    id: "0".into(),
                    name: "echo one".into()
                },
                DeclaredStep {
                    id: "cache".into(),
                    name: "actions/cache@v4".into()
                },
                DeclaredStep {
                    id: "2".into(),
                    name: "Named".into()
                },
            ]
        );
        assert!(
            declared["b"].is_empty(),
            "reusable workflow jobs declare no steps here"
        );
        assert!(parse(": not yaml [").is_none());
    }

    #[test]
    fn remote_only_jobs_are_declared_with_their_reason() {
        let declared = parse(
            "on: [push]\njobs:\n  timing:\n    runs-on: ubuntu-latest\n    env:\n      CI_REMOTE_ONLY: reads this run from the GitHub API\n    steps:\n      - run: echo\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n",
        )
        .unwrap();
        assert_eq!(
            declared.remote_only,
            [(
                "timing".to_string(),
                "reads this run from the GitHub API".to_string()
            )]
            .into()
        );
        assert_eq!(declared.steps.len(), 2);
    }
}
