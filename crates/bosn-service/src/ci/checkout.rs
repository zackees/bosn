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

use std::{io, path::Path};

use serde_yaml::{Mapping, Value};

/// Rewrite own-repository checkout steps of the workflow file in place.
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

fn localize_document(document: &mut Value, repository: &str) -> usize {
    let Some(jobs) = document.get_mut("jobs").and_then(Value::as_mapping_mut) else {
        return 0;
    };
    jobs.iter_mut()
        .filter_map(|(_, job)| job.get_mut("steps").and_then(Value::as_sequence_mut))
        .flatten()
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

/// `repository:` absent, equal to the run's repository, or the
/// `${{ github.repository }}` expression.
fn names_own_repository(with: &Mapping, repository: &str) -> bool {
    match with.get("repository").and_then(Value::as_str) {
        None => true,
        Some(named) => {
            named.eq_ignore_ascii_case(repository)
                || named.replace(' ', "") == "${{github.repository}}"
        }
    }
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
    fn an_unchanged_workflow_is_not_rewritten() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("ci.yml");
        let original = "# comments survive when nothing changes\non: [push]\njobs: {}\n";
        std::fs::write(&path, original).unwrap();
        assert_eq!(localize(&path, "example/demo").unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
