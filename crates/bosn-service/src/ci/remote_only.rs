//! Jobs that can only run on GitHub never fail a local run (zackees/ci.yml
//! GATE-012, #400).
//!
//! act runs a job in a Linux container with no GitHub backend, so some jobs
//! cannot work under it whatever the workflow does: a job that reads the
//! GitHub API about its own run (`github.run_id` is a local ID that GitHub
//! answers with 404), one that needs an OIDC token, or one that uses a
//! GitHub-side service. GATE-012 says such a check never gates, stalls or
//! fails a local run.
//!
//! A job is remote-only when it declares so, or when bosn can see it:
//! - its job-level `env:` sets [`MARKER`]; the value is the reason. This is
//!   the general declaration, for what no registry can see (bosn's own
//!   `CI queue timing` and `Full CI coverage` jobs read their run from the
//!   Actions API). On GitHub it is an ordinary, unused variable, so the job,
//!   its name and any required check built on it behave exactly as before;
//! - it uses an action from GATE-012's registry of act-impossible actions
//!   ([`ACTIONS`], kept in step with `ci_lint.remote_only.REMOTE_ONLY_ACTIONS`);
//!   an action in [`LOCAL_STUBS`] is not one: bosn serves it with a local
//!   stub ([`super::pages`]), so the job runs (a Pages *build* job runs
//!   locally; only `actions/deploy-pages` needs GitHub). [`classify`] is the
//!   one decision for every `uses:`;
//! - it requests `id-token: write` (act has no OIDC issuer).
//!
//! In the run's copy of the workflow ([`confine`]) such a job keeps its
//! `if:`, `needs:`, runner and matrix, so act decides exactly as GitHub would
//! whether it runs, and jobs that need it still run; only its steps become
//! one that prints the reason. The run then reports it `remote_only` with
//! that reason ([`super::model::RunTree::mark_remote_only`]): never a
//! failure, never a coverage gap, since by policy it is no local evidence.

use std::collections::BTreeMap;

use serde_yaml::{Mapping, Value};

use super::workflow::{Job, Permissions};

/// The job-level `env:` key declaring a job remote-only; its value is why.
pub const MARKER: &str = "CI_REMOTE_ONLY";

/// GATE-012's act-impossible actions: a `uses:` prefix and why act cannot
/// run it. Extend together with zackees/ci.yml's registry.
pub const ACTIONS: &[(&str, &str)] = &[
    (
        "github/codeql-action/",
        "code scanning needs GitHub's code-scanning backend",
    ),
    (
        "actions/dependency-review-action",
        "needs GitHub's dependency graph API",
    ),
    ("actions/deploy-pages", "deploys to GitHub Pages"),
    (
        "actions/attest-build-provenance",
        "needs an OIDC token and sigstore",
    ),
    ("actions/attest-sbom", "needs an OIDC token and sigstore"),
    ("actions/attest", "needs an OIDC token and sigstore"),
    (
        "pypa/gh-action-pypi-publish",
        "trusted publishing needs an OIDC token",
    ),
    ("dependabot/fetch-metadata", "needs a Dependabot PR"),
    (
        "codecov/codecov-action",
        "uploads to the hosted Codecov service",
    ),
    (
        "coverallsapp/github-action",
        "uploads to the hosted Coveralls service",
    ),
    (
        "sonarsource/sonarcloud-github-action",
        "runs on the hosted SonarCloud service",
    ),
    (
        "sonarsource/sonarqube-scan-action",
        "runs on a hosted SonarQube server",
    ),
    ("coderabbitai/", "a CodeRabbit (GitHub App) action"),
];

/// Actions bosn rewrites into a local stub step instead of confining the
/// job (a GATE-012 refinement): a `uses:` prefix and what the stub does.
/// `actions/configure-pages` only reads the site's Pages metadata; the stub
/// sets the outputs a build reads (`base_url`, `origin`, `host`,
/// `base_path`) for the repository's `https://<owner>.github.io/<repo>`
/// site. `actions/upload-pages-artifact` needs no stub: it uploads through
/// act's artifact server.
pub const LOCAL_STUBS: &[(&str, &str)] = &[(
    "actions/configure-pages",
    "sets base_url, origin, host and base_path for the repository's project site",
)];

/// The `actions/configure-pages` inputs that make it edit a site
/// generator's config from the live Pages site, which no stub can do.
const PAGES_GENERATOR_INPUTS: &[&str] = &["static_site_generator", "generator_config_file"];

/// What a local run does with one `uses:` step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepClass {
    /// act runs the action as written.
    Local,
    /// bosn replaces the step with a local stub ([`LOCAL_STUBS`]).
    Stubbed,
    /// The job is confined to a stub and reported `remote_only`: why.
    Confined(String),
}

/// How a local run treats `uses` with inputs `with`: stubbed, confined, or
/// run as written.
pub fn classify(uses: &str, with: &BTreeMap<String, Value>) -> StepClass {
    if let Some((prefix, _)) = LOCAL_STUBS
        .iter()
        .find(|(prefix, _)| uses.starts_with(prefix))
    {
        return match PAGES_GENERATOR_INPUTS
            .iter()
            .find(|input| with.contains_key(**input))
        {
            Some(input) => StepClass::Confined(format!(
                "uses {prefix} with {input}: it edits the generator's config from the live \
                 Pages site"
            )),
            None => StepClass::Stubbed,
        };
    }
    ACTIONS
        .iter()
        .find(|(prefix, _)| uses.starts_with(prefix))
        .map_or(StepClass::Local, |(prefix, why)| {
            StepClass::Confined(format!("uses {prefix}: {why}"))
        })
}

/// The job keys a confined job keeps: what decides whether, where and how
/// often it runs. Everything else (steps, outputs, services, container,
/// defaults, env) belongs to the work act cannot do.
const KEPT: &[&str] = &[
    "name",
    "needs",
    "if",
    "runs-on",
    "strategy",
    "timeout-minutes",
    "continue-on-error",
];

/// Why `job` can only run on GitHub, or `None` when act can run it.
pub fn reason(job: &Job) -> Option<String> {
    reason_with_repository(job, None)
}

/// Classify the frozen original using the same repository as localization.
pub fn reason_in_repository(job: &Job, repository: &str) -> Option<String> {
    reason_with_repository(job, Some(repository))
}

fn reason_with_repository(job: &Job, repository: Option<&str>) -> Option<String> {
    if let Some(declared) = job.env.get(MARKER).filter(|v| !v.is_null()) {
        let text = match declared {
            Value::String(text) => text.trim().to_string(),
            other => serde_yaml::to_string(other)
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        return Some(if text.is_empty() {
            format!("declared {MARKER}")
        } else {
            text
        });
    }
    let registered = job
        .steps
        .iter()
        .filter(|step| !inactive(step.condition.as_ref(), repository))
        .find_map(|step| match classify(step.uses.as_deref()?, &step.with) {
            StepClass::Confined(why) => Some(why),
            StepClass::Local | StepClass::Stubbed => None,
        });
    if registered.is_some() {
        return registered;
    }
    match &job.permissions {
        Some(Permissions::Scopes(scopes))
            if scopes.get("id-token").is_some_and(|l| l == "write") =>
        {
            Some("requests id-token: write; act has no OIDC issuer".into())
        }
        _ => None,
    }
}

/// Only literal false and exact repository comparisons are decidable here.
/// Unknown expressions remain remote-only; this is not an expression evaluator.
fn inactive(condition: Option<&Value>, repository: Option<&str>) -> bool {
    let Some(condition) = condition else {
        return false;
    };
    if condition == &Value::Bool(false) {
        return true;
    }
    let Some(text) = condition.as_str() else {
        return false;
    };
    let text = text.trim();
    let expression = text
        .strip_prefix("${{")
        .and_then(|s| s.strip_suffix("}}"))
        .unwrap_or(text)
        .trim();
    if expression == "false" {
        return true;
    }
    let Some(repository) = repository else {
        return false;
    };
    let Some(comparison) = expression.strip_prefix("github.repository") else {
        return false;
    };
    let comparison = comparison.trim_start();
    let (literal, equal) = if let Some(value) = comparison.strip_prefix("==") {
        (value, true)
    } else if let Some(value) = comparison.strip_prefix("!=") {
        (value, false)
    } else {
        return false;
    };
    let Some(literal) = literal
        .trim()
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    else {
        return false;
    };
    let Some((owner, name)) = literal.split_once('/') else {
        return false;
    };
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    if !valid(owner) || !valid(name) {
        return false;
    }
    repository.eq_ignore_ascii_case(literal) != equal
}

/// Materialize proven-false remote step guards in the overlay so the runner
/// and context-free coverage classifier agree. The original workflow is intact.
pub fn freeze_inactive_steps(document: &mut Value, repository: &str) -> usize {
    let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        return 0;
    };
    let mut count = 0;
    for job in jobs.values_mut() {
        let Some(steps) = job.get_mut("steps").and_then(Value::as_sequence_mut) else {
            continue;
        };
        for value in steps {
            let Ok(step) = serde_yaml::from_value::<super::workflow::Step>(value.clone()) else {
                continue;
            };
            let Some(uses) = step.uses.as_deref() else {
                continue;
            };
            if matches!(classify(uses, &step.with), StepClass::Confined(_))
                && inactive(step.condition.as_ref(), Some(repository))
                && step.condition != Some(Value::Bool(false))
            {
                value
                    .as_mapping_mut()
                    .unwrap()
                    .insert("if".into(), Value::Bool(false));
                count += 1;
            }
        }
    }
    count
}

/// In a workflow document, replace every remote-only job (one with
/// `steps:`; a reusable-workflow call has none) by a job that keeps
/// [`KEPT`], declares [`MARKER`] with its reason and only prints it. Returns
/// the confined job IDs with their reasons.
pub fn confine(document: &mut Value) -> Vec<(String, String)> {
    let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        return Vec::new();
    };
    let mut confined = Vec::new();
    for (id, job) in jobs.iter_mut() {
        let Some(id) = id.as_str() else { continue };
        if job.get("steps").is_none() {
            continue;
        }
        let Some(why) = serde_yaml::from_value::<Job>(job.clone())
            .ok()
            .as_ref()
            .and_then(reason)
        else {
            continue;
        };
        *job = Value::Mapping(stub(job, &why));
        confined.push((id.to_string(), why));
    }
    confined
}

fn stub(job: &Value, why: &str) -> Mapping {
    let mut kept = Mapping::new();
    for key in KEPT {
        if let Some(value) = job.get(*key) {
            kept.insert((*key).into(), value.clone());
        }
    }
    let mut env = Mapping::new();
    env.insert(MARKER.into(), why.into());
    kept.insert("env".into(), Value::Mapping(env));
    let mut step = Mapping::new();
    step.insert(
        "name".into(),
        "Remote-only: runs on GitHub, not under act (GATE-012)".into(),
    );
    step.insert(
        "run".into(),
        format!("echo \"bosn ci: not run locally (GATE-012): ${MARKER}\"").into(),
    );
    kept.insert("steps".into(), Value::Sequence(vec![Value::Mapping(step)]));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(yaml: &str) -> Job {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn inactive_registered_steps_leave_the_test_job_runnable() {
        assert_eq!(
            reason(&job(
                "steps:\n  - run: go test ./...\n  - uses: codecov/codecov-action@v5\n    if: false\n"
            )),
            None
        );
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("checks.yml");
        let output = root.path().join("localized.yml");
        std::fs::write(&source, "jobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: go test ./...\n      - uses: codecov/codecov-action@v5\n        if: github.repository == 'nektos/act'\n").unwrap();
        let mut changes = super::super::checkout::Localized::default();
        super::super::checkout::localize(&source, &output, "zackees/act2", &mut changes).unwrap();
        assert!(changes.remote_only.is_empty());
        let document: Value =
            serde_yaml::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        let test: Job = serde_yaml::from_value(document["jobs"]["test"].clone()).unwrap();
        assert_eq!(reason(&test), None);
        assert_eq!(
            super::super::flush::untrap(test.steps[0].run.as_deref().unwrap()),
            "go test ./..."
        );
        assert_eq!(changes.inactive_remote_steps, 1);
        assert_eq!(test.steps[1].condition, Some(Value::Bool(false)));
        // Runtime conclusions read the frozen original, not the overlay.
        let declared = super::super::workflow::declared(root.path(), "checks.yml", "zackees/act2");
        assert!(declared.remote_only.is_empty());
    }

    #[test]
    fn repository_guards_are_narrow_and_unknown_guards_fail_closed() {
        for condition in [
            "false",
            "${{ false }}",
            "github.repository == 'nektos/act'",
            "${{ github.repository != 'ZACKEES/ACT2' }}",
        ] {
            assert!(
                inactive(Some(&Value::String(condition.into())), Some("zackees/act2")),
                "{condition}"
            );
        }
        for condition in [
            "true",
            "github.repository == 'ZACKEES/ACT2'",
            "github.repository != 'nektos/act'",
            "github.repository == 'nektos/act' || true",
            "github.repository == inputs.repository",
            "github.repository == 'a/b/c'",
            "github.repository == ''",
            "!false",
            "${{ false }} || true",
            "github.repository == 'nektos/act''",
            "contains(github.repository, 'act')",
        ] {
            assert!(
                !inactive(Some(&Value::String(condition.into())), Some("zackees/act2")),
                "{condition}"
            );
        }
        assert!(!inactive(
            Some(&Value::String("github.repository == 'nektos/act'".into())),
            None
        ));
        for yaml in [
            "env: {CI_REMOTE_ONLY: declared}\nsteps: [{uses: codecov/codecov-action@v5, if: false}]",
            "permissions: {id-token: write}\nsteps: [{uses: codecov/codecov-action@v5, if: false}]",
            "steps: [{uses: codecov/codecov-action@v5, if: 'inputs.enabled'}]",
        ] {
            assert!(reason(&job(yaml)).is_some(), "{yaml}");
        }
    }

    #[test]
    fn a_job_is_remote_only_by_declaration_registry_or_oidc() {
        assert_eq!(
            reason(&job(
                "env:\n  CI_REMOTE_ONLY: reads this run from the GitHub API\nsteps: [{run: x}]\n"
            )),
            Some("reads this run from the GitHub API".into())
        );
        assert_eq!(
            reason(&job("env:\n  CI_REMOTE_ONLY: true\n")),
            Some("true".into()),
            "a non-text value still declares it"
        );
        assert_eq!(
            reason(&job(
                "steps:\n  - uses: actions/checkout@v4\n  - uses: pypa/gh-action-pypi-publish@release/v1\n"
            )),
            Some("uses pypa/gh-action-pypi-publish: trusted publishing needs an OIDC token".into())
        );
        assert_eq!(
            reason(&job(
                "permissions:\n  id-token: write\n  contents: read\nsteps: [{run: x}]\n"
            )),
            Some("requests id-token: write; act has no OIDC issuer".into())
        );
        for runnable in [
            "steps: [{uses: actions/checkout@v4}, {run: cargo test}]\n",
            "permissions: write-all\nsteps: [{run: x}]\n",
            "permissions:\n  actions: read\nenv:\n  OTHER: x\nsteps: [{run: x}]\n",
            "env:\n  CI_REMOTE_ONLY: null\n",
        ] {
            assert_eq!(reason(&job(runnable)), None, "{runnable}");
        }
    }

    /// A Pages *build* job reads Pages metadata, builds and uploads the site:
    /// it runs locally. Only the deploy needs GitHub.
    #[test]
    fn a_pages_build_job_runs_locally_and_only_its_deploy_is_confined() {
        let build = "permissions: {contents: read, pages: write}\nsteps:\n  - uses: actions/checkout@v4\n  - run: make site\n  - uses: actions/configure-pages@v5\n  - uses: actions/upload-pages-artifact@v3\n    with: {path: site}\n";
        assert_eq!(reason(&job(build)), None);
        let deploy = "permissions: {pages: write, id-token: write}\nsteps:\n  - id: deployment\n    uses: actions/deploy-pages@v4\n";
        assert!(reason(&job(deploy)).is_some());
    }

    /// The one classification of `uses:` steps: run as written, stubbed
    /// locally, or confined with the job.
    #[test]
    fn every_action_is_run_stubbed_or_confined() {
        let none = BTreeMap::new();
        for local in [
            "actions/checkout@v4",
            "actions/upload-pages-artifact@v3",
            "actions/upload-artifact@v4",
            "actions/setup-python@v5",
        ] {
            assert_eq!(classify(local, &none), StepClass::Local, "{local}");
        }
        assert_eq!(
            classify("actions/configure-pages@v5", &none),
            StepClass::Stubbed
        );
        let generator =
            BTreeMap::from([("static_site_generator".to_string(), Value::from("next"))]);
        assert!(matches!(
            classify("actions/configure-pages@v5", &generator),
            StepClass::Confined(_)
        ));
        for confined in [
            "actions/deploy-pages@v4",
            "pypa/gh-action-pypi-publish@release/v1",
            "actions/attest-build-provenance@v2",
            "github/codeql-action/upload-sarif@v3",
            "codecov/codecov-action@v5",
        ] {
            assert!(
                matches!(classify(confined, &none), StepClass::Confined(_)),
                "{confined}"
            );
        }
        // Every stub is outside the confined registry.
        for (stub, _) in LOCAL_STUBS {
            assert!(ACTIONS.iter().all(|(prefix, _)| !stub.starts_with(prefix)));
        }
    }

    #[test]
    fn confine_keeps_when_and_where_a_job_runs_and_replaces_its_work() {
        let mut document: Value = serde_yaml::from_str(
            "on: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps: [{run: make}]\n  timing:\n    name: CI queue timing\n    if: ${{ !cancelled() }}\n    needs: [build]\n    runs-on: ubuntu-latest\n    permissions: {actions: read}\n    outputs: {x: '${{ steps.a.outputs.x }}'}\n    env:\n      CI_REMOTE_ONLY: reads this run from the GitHub API\n      GITHUB_TOKEN: x\n    steps:\n      - uses: actions/checkout@v4\n      - run: python3 ci/report.py --run-id ${{ github.run_id }}\n  called:\n    uses: ./.github/workflows/other.yml\n",
        )
        .unwrap();
        let confined = confine(&mut document);
        assert_eq!(
            confined,
            [(
                "timing".to_string(),
                "reads this run from the GitHub API".to_string()
            )]
        );
        let expected: Value = serde_yaml::from_str(
            "name: CI queue timing\nif: ${{ !cancelled() }}\nneeds: [build]\nruns-on: ubuntu-latest\nenv:\n  CI_REMOTE_ONLY: reads this run from the GitHub API\nsteps:\n  - name: 'Remote-only: runs on GitHub, not under act (GATE-012)'\n    run: 'echo \"bosn ci: not run locally (GATE-012): $CI_REMOTE_ONLY\"'\n",
        )
        .unwrap();
        assert_eq!(document["jobs"]["timing"], expected);
        assert_eq!(document["jobs"]["build"]["steps"][0]["run"], "make");
        assert_eq!(
            reason(&serde_yaml::from_value(document["jobs"]["timing"].clone()).unwrap()),
            Some("reads this run from the GitHub API".into()),
            "the confined job still declares itself, for the end-of-run report"
        );
        assert!(confine(&mut Value::Null).is_empty());
    }
}
