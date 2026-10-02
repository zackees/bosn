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
//! - it requests `id-token: write` (act has no OIDC issuer).
//!
//! In the run's copy of the workflow ([`confine`]) such a job keeps its
//! `if:`, `needs:`, runner and matrix, so act decides exactly as GitHub would
//! whether it runs, and jobs that need it still run; only its steps become
//! one that prints the reason. The run then reports it `remote_only` with
//! that reason ([`super::model::RunTree::mark_remote_only`]): never a
//! failure, never a coverage gap, since by policy it is no local evidence.

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
    ("actions/configure-pages", "needs a GitHub Pages site"),
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
    let registered = job.steps.iter().find_map(|step| {
        let uses = step.uses.as_deref()?;
        ACTIONS
            .iter()
            .find(|(prefix, _)| uses.starts_with(prefix))
            .map(|(prefix, why)| format!("uses {prefix}: {why}"))
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
