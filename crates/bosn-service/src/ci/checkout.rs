//! `actions/checkout` served locally, with no GitHub token (#335).
//!
//! act serves a checkout of the run's own repository by copying the snapshot
//! into the job container (per job, honouring `path:`), but only when the
//! step's raw `ref:` equals `github.ref` and its `repository:` is absent or
//! equal. Any other `ref:` (an expression such as `${{ github.sha }}`, a
//! SHA) makes act run the real action: it needs a token, fetches from GitHub
//! and discards the snapshot's uncommitted work. bosn therefore removes
//! `ref:` (and a `repository:` naming the run's own repository) from
//! own-repository checkout steps in the run's copy of the workflow; the
//! snapshot *is* that ref.
//!
//! A checkout of another repository pinned to a full commit SHA (bosn's own
//! `ci.yml` pins its lint rules this way) is immutable, so it cannot test the
//! wrong code: the step becomes a plain anonymous `git fetch` of that commit,
//! which needs no token, and the run log names every such fetch. Any other
//! checkout of another repository (a moving ref, an explicit `token:`, an
//! expression `path:`) is left to the real action, which fails loudly
//! without a token rather than silently testing something else.
//!
//! Every file act may run is rewritten: the workflows (reusable ones
//! included) and the repository's own composite actions. The rewrites go to
//! an overlay beside the snapshot, never into it (#424): act2 reads them
//! through `--workflow-overlay`, and every job checks out the snapshot, so a
//! repository that inspects its own `.github/` sees exactly what it
//! committed.
//!
//! The same pass gives every POSIX-shell `run:` step the end-of-output trap
//! of [`super::flush`] (#398), confines remote-only jobs
//! ([`super::remote_only`], GATE-012) and gates matrix legs on their own
//! runner ([`super::matrix_runner`], #404), so the files are read and written
//! once.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

/// What the rewrite did to a run's workflows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Localized {
    /// Own-repository checkout steps now served from the frozen snapshot.
    pub own: usize,
    /// `owner/name@commit` of each pinned checkout of another repository,
    /// now fetched anonymously.
    pub pinned: Vec<String>,
    /// `run:` steps given the end-of-output trap ([`super::flush`]).
    pub trapped: usize,
    /// Remote-only jobs confined to a stub, with the reason
    /// ([`super::remote_only`]).
    pub remote_only: Vec<(String, String)>,
    /// Matrix jobs whose legs bosn runs or reports unsupported by their own
    /// runner ([`super::matrix_runner`], #404).
    pub runner_gated: Vec<String>,
    /// The workflow and action files rewritten, relative to the snapshot.
    pub files: Vec<PathBuf>,
}

/// Rewrite checkout and `run:` steps in every workflow under `.github/workflows/` and
/// every composite action under `.github/actions/` of the snapshot at `root`,
/// writing each changed file to the same relative path under `overlay`.
/// `root` itself is only read.
pub fn localize_tree(root: &Path, overlay: &Path, repository: &str) -> io::Result<Localized> {
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
    let mut localized = Localized::default();
    for file in files {
        let relative = file.strip_prefix(root).unwrap_or(&file).to_path_buf();
        if localize(&file, &overlay.join(&relative), repository, &mut localized)? {
            localized.files.push(relative);
        }
    }
    Ok(localized)
}

/// Rewrite the checkout and `run:` steps of one workflow or action file, adding what
/// changed to `localized`; the rewrite is written to `out` (and `true`
/// returned) only when something did.
pub fn localize(
    workflow: &Path,
    out: &Path,
    repository: &str,
    localized: &mut Localized,
) -> io::Result<bool> {
    let text = std::fs::read_to_string(workflow)?;
    let mut document: Value = serde_yaml::from_str(&text).map_err(io::Error::other)?;
    let mut changes = Localized::default();
    localize_document(&mut document, repository, &mut changes);
    changes.remote_only = super::remote_only::confine(&mut document);
    changes.runner_gated = super::matrix_runner::gate(&mut document);
    changes.trapped = super::flush::add_traps(&mut document);
    let changed = changes != Localized::default();
    if changed {
        let rewritten = serde_yaml::to_string(&document).map_err(io::Error::other)?;
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out, rewritten)?;
    }
    localized.own += changes.own;
    localized.pinned.extend(changes.pinned);
    localized.trapped += changes.trapped;
    localized.remote_only.extend(changes.remote_only);
    localized.runner_gated.extend(changes.runner_gated);
    Ok(changed)
}

/// Steps live under `jobs.<id>.steps` in a workflow and under `runs.steps`
/// in a composite action.
fn localize_document(document: &mut Value, repository: &str, localized: &mut Localized) {
    let mut steps: Vec<&mut Value> = Vec::new();
    let Some(document) = document.as_mapping_mut() else {
        return;
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
    for step in steps {
        localize_step(step, repository, localized);
    }
}

/// The `with:` inputs of an `actions/checkout` step that decide how bosn
/// serves it; the others are kept or dropped as a whole.
#[derive(Deserialize)]
struct CheckoutInputs {
    repository: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    path: Option<String>,
    token: Option<String>,
}

fn localize_step(step: &mut Value, repository: &str, localized: &mut Localized) {
    let Some(step) = step.as_mapping_mut().filter(|step| {
        step.get("uses")
            .and_then(Value::as_str)
            .is_some_and(|uses| uses.starts_with("actions/checkout@"))
    }) else {
        return;
    };
    let Some(with) = step.get_mut("with").and_then(Value::as_mapping_mut) else {
        return;
    };
    let Ok(inputs) = serde_yaml::from_value::<CheckoutInputs>(Value::Mapping(with.clone())) else {
        return;
    };
    if names_own_repository(inputs.repository.as_deref(), repository) {
        let removed_ref = with.remove("ref").is_some();
        let removed_repository = with.remove("repository").is_some();
        localized.own += usize::from(removed_ref || removed_repository);
    } else if let Some(pinned) = PinnedCheckout::parse(&inputs) {
        pinned.replace(step);
        localized.pinned.push(pinned.to_string());
    }
}

/// A checkout of another public repository at an immutable commit.
struct PinnedCheckout {
    repository: String,
    commit: String,
    path: Option<String>,
}

impl PinnedCheckout {
    /// `None` unless the repository is a plain `owner/name`, the ref a full
    /// commit SHA, the path a literal, and no explicit token asks for access
    /// an anonymous fetch would not have.
    fn parse(inputs: &CheckoutInputs) -> Option<Self> {
        if inputs.token.is_some() {
            return None;
        }
        let repository = inputs.repository.as_deref()?.trim();
        let segment = |s: &str| {
            !s.is_empty()
                && !s.starts_with('.')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let (owner, name) = repository.split_once('/')?;
        let commit = inputs.reference.as_deref()?.trim();
        let pinned = commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit());
        let path = inputs
            .path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        if !segment(owner) || !segment(name) || !pinned || path.is_some_and(|p| p.contains("${{")) {
            return None;
        }
        Some(Self {
            repository: repository.to_owned(),
            commit: commit.to_ascii_lowercase(),
            path: path.map(str::to_owned),
        })
    }

    /// Turn the step into a `run:` step, keeping its other keys (`name`,
    /// `id`, `if`, `env`, `continue-on-error`, ...).
    fn replace(&self, step: &mut Mapping) {
        step.remove("uses");
        step.remove("with");
        if !step.contains_key("name") {
            step.insert("name".into(), format!("actions/checkout {self}").into());
        }
        step.insert("shell".into(), "bash".into());
        step.insert("run".into(), self.script().into());
    }

    /// Like the real action: the destination is emptied, then holds exactly
    /// that commit. `sparse-checkout` is ignored; the whole tree is a superset.
    fn script(&self) -> String {
        let dest = match &self.path {
            Some(path) => format!("\"$GITHUB_WORKSPACE\"/'{}'", path.replace('\'', r"'\''")),
            None => "\"$GITHUB_WORKSPACE\"".to_owned(),
        };
        format!(
            "set -euo pipefail\n\
             echo 'bosn: fetching {self} anonymously (a pinned commit; no token)'\n\
             dest={dest}\n\
             mkdir -p \"$dest\"\n\
             find \"$dest\" -mindepth 1 -delete\n\
             git -C \"$dest\" init --quiet\n\
             git -C \"$dest\" fetch --quiet --depth 1 https://github.com/{repo}.git {commit}\n\
             git -C \"$dest\" -c advice.detachedHead=false checkout --quiet FETCH_HEAD\n",
            repo = self.repository,
            commit = self.commit,
        )
    }
}

impl fmt::Display for PinnedCheckout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.repository, self.commit)
    }
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
fn names_own_repository(named: Option<&str>, repository: &str) -> bool {
    let Some(named) = named else {
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

    fn rewrite(yaml: &str) -> (Localized, Value) {
        let mut document: Value = serde_yaml::from_str(yaml).unwrap();
        let mut localized = Localized::default();
        localize_document(&mut document, "example/demo", &mut localized);
        (localized, document)
    }

    const PIN: &str = "c87f1a2b88c07037d5c10a8e8ff27dd25895eab9";

    #[test]
    fn a_pinned_checkout_of_another_repository_is_fetched_without_a_token() {
        let (localized, document) = rewrite(&format!(
            "on: [push]\njobs:\n  a:\n    steps:\n      - name: lint rules\n        if: always()\n        uses: actions/checkout@v4\n        with:\n          repository: zackees/ci.yml\n          ref: {PIN}\n          path: .ci-lint\n          persist-credentials: false\n          sparse-checkout: ci_lint\n      - uses: actions/checkout@v4\n        with:\n          repository: zackees/ci.yml\n          ref: {PIN}\n"
        ));
        assert_eq!(localized.own, 0);
        assert_eq!(
            localized.pinned,
            vec![
                format!("zackees/ci.yml@{PIN}"),
                format!("zackees/ci.yml@{PIN}")
            ]
        );
        let steps = document["jobs"]["a"]["steps"].as_sequence().unwrap();
        let step = &steps[0];
        assert!(
            step.get("uses").is_none() && step.get("with").is_none(),
            "{step:?}"
        );
        assert_eq!(step["name"], "lint rules", "the step keeps its name");
        assert_eq!(step["if"], "always()", "and its condition");
        assert_eq!(step["shell"], "bash");
        let script = step["run"].as_str().unwrap();
        assert!(
            script.contains(&format!(
                "fetch --quiet --depth 1 https://github.com/zackees/ci.yml.git {PIN}"
            )),
            "{script}"
        );
        assert!(
            script.contains("dest=\"$GITHUB_WORKSPACE\"/'.ci-lint'"),
            "{script}"
        );
        assert!(
            !script.contains("GITHUB_TOKEN") && !script.contains("Authorization"),
            "no credential is used: {script}"
        );
        assert_eq!(
            steps[1]["name"],
            format!("actions/checkout zackees/ci.yml@{PIN}"),
            "an unnamed step is named after what it fetches"
        );
        assert!(
            steps[1]["run"]
                .as_str()
                .unwrap()
                .contains("dest=\"$GITHUB_WORKSPACE\"\n")
        );
    }

    #[test]
    fn another_repository_at_a_moving_ref_with_a_token_or_an_expression_path_is_untouched() {
        let original = format!(
            "on: [push]\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/else\n          ref: v1\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/private\n          ref: {PIN}\n          token: ${{{{ secrets.PAT }}}}\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/else\n          ref: {PIN}\n          path: ${{{{ inputs.dir }}}}\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/else\n          ref: ${{{{ inputs.sha }}}}\n      - uses: actions/checkout@v4\n        with:\n          repository: \"bad repo/x;y\"\n          ref: {PIN}\n"
        );
        let (localized, document) = rewrite(&original);
        assert_eq!(localized, Localized::default());
        assert_eq!(document, serde_yaml::from_str::<Value>(&original).unwrap());
    }

    #[test]
    fn a_path_is_quoted_for_the_shell() {
        let (_, document) = rewrite(&format!(
            "jobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/else\n          ref: {PIN}\n          path: it's $(here)\n"
        ));
        let script = document["jobs"]["a"]["steps"][0]["run"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            script.contains(r#"dest="$GITHUB_WORKSPACE"/'it'\''s $(here)'"#),
            "{script}"
        );
    }

    #[test]
    fn own_repository_checkouts_lose_their_ref_others_are_untouched() {
        let (localized, document) = rewrite(
            "on: [push]\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.sha }}\n          path: nested\n      - uses: actions/checkout@v4\n        with:\n          repository: ${{ github.repository }}\n          ref: main\n      - uses: actions/checkout@v4\n        with:\n          repository: someone/elsewhere\n          ref: v1\n      - uses: actions/checkout@v4\n      - run: echo hi\n        with:\n          ref: kept\n",
        );
        assert_eq!(localized.own, 2);
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
        let own = |expr: &str| names_own_repository(Some(expr), "example/demo");
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
        let overlay = dir.path().with_extension("overlay");
        let localized = localize_tree(root, &overlay, "example/demo").unwrap();
        assert_eq!(localized.own, 3);
        assert!(localized.pinned.is_empty());
        for relative in [
            ".github/workflows/ci.yml",
            ".github/workflows/_build.yaml",
            ".github/actions/setup/action.yml",
        ] {
            let text = std::fs::read_to_string(overlay.join(relative)).unwrap();
            assert!(!text.contains("ref:"), "{relative}: {text}");
            let original = std::fs::read_to_string(root.join(relative)).unwrap();
            assert!(
                original.contains("ref:"),
                "{relative}: the snapshot is only read"
            );
        }
        assert!(
            !overlay.join(".github/workflows/other.yml").exists(),
            "another repository's checkout is untouched, so not overlaid"
        );
        let _ = std::fs::remove_dir_all(&overlay);
    }

    /// #424 (supersedes #394): bosn's rewrites never touch the snapshot, so
    /// a job's checkout is byte-for-byte the tree under test, clean to Git
    /// with no index tricks, and a repository's own workflow tests read what
    /// it committed.
    #[cfg(unix)]
    #[test]
    fn the_snapshot_stays_the_tree_under_test() {
        use crate::ci::snapshot::tests::{git_in, sh};
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = dir.path();
        let relative = ".github/workflows/ci.yml";
        let workflow = root.join(relative);
        std::fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        let original = "# the repository's own comment\non: [push]\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.sha }}\n      - run: make test\n";
        std::fs::write(&workflow, original).unwrap();
        sh(
            root,
            "printf 'x\\n' > README && git init -q -b main . && git add -A && git commit -qm init",
        );
        let overlay = root.with_extension("overlay");
        let localized = localize_tree(root, &overlay, "example/demo").unwrap();
        assert_eq!(localized.own, 1);
        assert_eq!(localized.trapped, 1, "the run: step ends its output");
        assert_eq!(localized.files, vec![PathBuf::from(relative)]);
        assert_eq!(std::fs::read_to_string(&workflow).unwrap(), original);
        assert_eq!(
            git_in(root, &["status", "--porcelain"]),
            "",
            "clean, unaided"
        );
        let rewritten = std::fs::read_to_string(overlay.join(relative)).unwrap();
        assert!(
            !rewritten.contains("ref:"),
            "act reads the rewrite: {rewritten}"
        );
        let _ = std::fs::remove_dir_all(&overlay);
    }

    #[test]
    fn an_unchanged_workflow_is_not_rewritten() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("ci.yml");
        let original = "# comments survive when nothing changes\non: [push]\njobs: {}\n";
        std::fs::write(&path, original).unwrap();
        let out = dir.path().join("overlay/ci.yml");
        let mut localized = Localized::default();
        localize(&path, &out, "example/demo", &mut localized).unwrap();
        assert_eq!(localized, Localized::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!out.exists(), "nothing to overlay");
    }
}
