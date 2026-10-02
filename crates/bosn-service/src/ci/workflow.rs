//! The steps a GitHub workflow declares, read from the frozen snapshot.
//!
//! act prints nothing for a step whose `if:` is false, so the run tree would
//! silently omit it. Knowing every declared step lets a finished job list the
//! ones that never ran as `skipped`. Step identity matches act's `stepID`:
//! the step's `id:` when it has one, otherwise its index.

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

#[derive(Deserialize)]
struct Workflow {
    #[serde(default)]
    jobs: BTreeMap<String, Job>,
}

#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    id: Option<String>,
    name: Option<String>,
    uses: Option<String>,
    run: Option<String>,
}

impl Step {
    /// act's display name: `name`, else `uses`, else the `run` text.
    fn display(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.uses.clone())
            .or_else(|| self.run.as_deref().map(|r| r.trim().to_string()))
            .unwrap_or_default()
    }
}

/// Parse the workflow file's job steps. A workflow that cannot be read or
/// parsed yields no declarations (act reports what it can on its own).
pub fn declared_steps(source: &Path, workflow: &str) -> DeclaredSteps {
    std::fs::read_to_string(source.join(workflow))
        .ok()
        .and_then(|text| parse(&text))
        .unwrap_or_default()
}

fn parse(text: &str) -> Option<DeclaredSteps> {
    let workflow: Workflow = serde_yaml::from_str(text).ok()?;
    Some(
        workflow
            .jobs
            .into_iter()
            .map(|(job, spec)| {
                let steps = spec
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| DeclaredStep {
                        id: step.id.clone().unwrap_or_else(|| index.to_string()),
                        name: step.display(),
                    })
                    .collect();
                (job, steps)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_use_explicit_ids_else_their_index() {
        let declared = parse(
            "on: [push]\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo one\n      - id: cache\n        uses: actions/cache@v4\n      - name: Named\n        if: false\n        run: echo never\n  b:\n    uses: ./.github/workflows/reusable.yml\n",
        )
        .unwrap();
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
}
