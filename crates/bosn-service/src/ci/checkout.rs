//! `actions/checkout` served from the run's frozen snapshot (#335).
//!
//! act serves a checkout of the run's own repository by copying the snapshot
//! into the job container (per job, honouring `path:`), but only when the
//! step's raw `ref:` equals `github.ref` and its `repository:` is absent or
//! equal. Any other `ref:` (an expression such as `${{ github.sha }}`, a
//! SHA) makes act run the real action: it needs a token, fetches from GitHub
//! and discards the snapshot's uncommitted work. bosn therefore removes
//! `ref:` (and a `repository:` naming the run's own repository) from
//! own-repository checkout steps in the run's copy of the workflow; the
//! snapshot *is* that ref. Checkouts of other repositories are untouched.
//! Every file act may run is rewritten: the workflows (reusable ones
//! included) and the repository's own composite actions.

use std::{io, path::Path};

use serde_yaml::{Mapping, Value};

/// Rewrite own-repository checkout steps in every workflow under
/// `.github/workflows/` and every composite action under `.github/actions/`
/// of the snapshot at `root`. Returns how many steps were changed.
pub fn localize_tree(root: &Path, repository: &str) -> io::Result<usize> {
    let yaml = |path: &Path| path.extension().is_some_and(|e| e == "yml" || e == "yaml");
    let mut files: Vec<_> = match std::fs::read_dir(root.join(".github/workflows")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && yaml(p))
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let actions = root.join(".github/actions");
    if actions.is_dir() {
        files.extend(
            kernal_api::platform::fs::DirectoryWalk::new(actions)
                .walk()
                .filter_map(Result::ok)
                .filter(|e| e.is_file())
                .map(|e| e.path().to_path_buf())
                .filter(|p| p.file_stem().is_some_and(|s| s == "action") && yaml(p)),
        );
    }
    files.sort();
    let mut changed = 0;
    for file in files {
        changed += localize(&file, repository)?;
    }
    Ok(changed)
}

/// Rewrite own-repository checkout steps of one workflow or action file.
/// Returns how many steps were changed; the file is rewritten only then.
pub fn localize(workflow: &Path, repository: &str) -> io::Result<usize> {
    let text = std::fs::read_to_string(workflow)?;
    let mut document: Value = serde_yaml::from_str(&text).map_err(io::Error::other)?;
    let changed = localize_document(&mut document, repository);
    if changed > 0 {
        let rewritten = serde_yaml::to_string(&document).map_err(io::Error::other)?;
        std::fs::write(workflow, rewritten)?;
    }
    Ok(changed)
}

/// Steps live under `jobs.<id>.steps` in a workflow and under `runs.steps`
/// in a composite action.
fn localize_document(document: &mut Value, repository: &str) -> usize {
    let mut steps: Vec<&mut Value> = Vec::new();
    let Some(document) = document.as_mapping_mut() else {
        return 0;
    };
    for (key, value) in document.iter_mut() {
        match key.as_str() {
            Some("jobs") => {
                if let Some(jobs) = value.as_mapping_mut() {
                    steps.extend(
                        jobs.iter_mut()
                            .filter_map(|(_, job)| job.get_mut("steps"))
                            .filter_map(Value::as_sequence_mut)
                            .flatten(),
                    );
                }
            }
            Some("runs") => {
                if let Some(list) = value.get_mut("steps").and_then(Value::as_sequence_mut) {
                    steps.extend(list.iter_mut());
                }
            }
            _ => {}
        }
    }
    steps
        .into_iter()
        .map(|step| usize::from(localize_step(step, repository)))
        .sum()
}

fn localize_step(step: &mut Value, repository: &str) -> bool {
    let is_checkout = step
        .get("uses")
        .and_then(Value::as_str)
        .is_some_and(|uses| uses.starts_with("actions/checkout@"));
    let Some(with) = step
        .get_mut("with")
        .and_then(Value::as_mapping_mut)
        .filter(|_| is_checkout)
    else {
        return false;
    };
    if !names_own_repository(with, repository) {
        return false;
    }
    let mut changed = with.remove("ref").is_some();
    if with.contains_key("repository") {
        with.remove("repository");
        changed = true;
    }
    changed
}

/// Context references that name the run's own repository in bosn's payload
/// (a local pull request is from this repository to itself).
const OWN_REPOSITORY_REFS: [&str; 4] = [
    "github.repository",
    "github.event.repository.full_name",
    "github.event.pull_request.head.repo.full_name",
    "github.event.pull_request.base.repo.full_name",
];

/// `repository:` absent, the run's repository, or an expression whose every
/// possible value is: in `${{ a == 'x' && v1 || v2 }}` the comparisons are
/// conditions and `v1`, `v2` are the values. Anything this does not parse
/// (parentheses, functions) is treated as another repository.
fn names_own_repository(with: &Mapping, repository: &str) -> bool {
    let Some(named) = with.get("repository").and_then(Value::as_str) else {
        return true;
    };
    let names_this = |value: &str| {
        value.eq_ignore_ascii_case(repository)
            || value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .is_some_and(|v| v.eq_ignore_ascii_case(repository))
            || OWN_REPOSITORY_REFS.contains(&value)
    };
    let Some(expression) = named
        .trim()
        .strip_prefix("${{")
        .and_then(|e| e.strip_suffix("}}"))
    else {
        return names_this(named.trim());
    };
    if expression.contains(['(', ')']) {
        return false;
    }
    expression
        .split("||")
        .flat_map(|alternative| alternative.split("&&"))
        .map(str::trim)
        .filter(|operand| !operand.contains("==") && !operand.contains("!="))
        .all(names_this)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(yaml: &str) -> (usize, Value) {
        let mut document: Value = serde_yaml::from_str(yaml).unwrap();
        let changed = localize_document(&mut document, "example/demo");
        (changed, document)
    }

    #[test]
    fn own_repository_checkouts_lose_their_ref_others_are_untouched() {
        let (changed, document) = rewrite(
            "on: [push]\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.sha }}\n          path: nested\n      - uses: actions/checkout@v4\n        with:\n          repository: ${{ github.repository }}\n          ref: main\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/elsewhere\n          ref: v1\n      - uses: actions/checkout@v4\n      - run: echo hi\n        with:\n          ref: kept\n",
        );
        assert_eq!(changed, 2);
        let steps = document["jobs"]["a"]["steps"].as_sequence().unwrap();
        assert!(steps[0]["with"].get("ref").is_none());
        assert_eq!(steps[0]["with"]["path"], "nested", "other inputs are kept");
        assert!(steps[1]["with"].get("repository").is_none());
        assert_eq!(steps[2]["with"]["repository"], "someone/elsewhere");
        assert_eq!(steps[2]["with"]["ref"], "v1");
        assert_eq!(
            steps[4]["with"]["ref"], "kept",
            "only checkout steps change"
        );
        assert_eq!(document["on"][0], "push", "`on` stays a key, not a boolean");
    }

    #[test]
    fn a_pr_aware_repository_expression_naming_only_this_repository_is_own() {
        let own = |expr: &str| {
            let mut with = Mapping::new();
            with.insert("repository".into(), expr.into());
            names_own_repository(&with, "example/demo")
        };
        // The fleet's checkout: the PR head repository, else this one.
        assert!(own(
            "${{ github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name || github.repository }}"
        ));
        assert!(own("${{ github.event.repository.full_name }}"));
        assert!(
            !own("${{ inputs.fork && 'example/demo' || github.repository }}"),
            "a bare operand counts as a value, and `inputs.fork` names no repository"
        );
        assert!(!own(
            "${{ github.event_name == 'push' && 'someone/else' || github.repository }}"
        ));
        assert!(!own("${{ inputs.repository || github.repository }}"));
        assert!(
            !own("${{ (github.repository) }}"),
            "parentheses are not parsed"
        );
        assert!(own("example/demo"));
        assert!(!own("someone/else"));
    }

    #[test]
    fn reusable_workflows_and_composite_actions_are_localized_too() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = dir.path();
        let fleet = "      - uses: actions/checkout@v4\n        with:\n          repository: ${{ github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name || github.repository }}\n          ref: ${{ inputs.source_ref || github.sha }}\n";
        let write = |relative: &str, text: &str| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            ".github/workflows/ci.yml",
            &format!("on: [push]\njobs:\n  a:\n    steps:\n{fleet}"),
        );
        write(
            ".github/workflows/_build.yaml",
            &format!("on: workflow_call\njobs:\n  b:\n    steps:\n{fleet}"),
        );
        write(
            ".github/actions/setup/action.yml",
            &format!(
                "runs:\n  using: composite\n  steps:\n{}",
                fleet.replace("      ", "    ")
            ),
        );
        let other = "on: [push]\njobs:\n  c:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/else\n          ref: v1\n";
        write(".github/workflows/other.yml", other);
        assert_eq!(localize_tree(root, "example/demo").unwrap(), 3);
        for relative in [
            ".github/workflows/ci.yml",
            ".github/workflows/_build.yaml",
            ".github/actions/setup/action.yml",
        ] {
            let text = std::fs::read_to_string(root.join(relative)).unwrap();
            assert!(!text.contains("ref:"), "{relative}: {text}");
        }
        assert_eq!(
            std::fs::read_to_string(root.join(".github/workflows/other.yml")).unwrap(),
            other,
            "another repository's checkout is untouched"
        );
    }

    #[test]
    fn an_unchanged_workflow_is_not_rewritten() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("ci.yml");
        let original = "# comments survive when nothing changes\non: [push]\njobs: {}\n";
        std::fs::write(&path, original).unwrap();
        assert_eq!(localize(&path, "example/demo").unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
