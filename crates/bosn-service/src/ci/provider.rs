//! CI providers (the workflow syntax) and bosn's trigger vocabulary.
//!
//! The provider is detected from the repository and is a separate axis from
//! the engine that executes it. Triggers map onto provider events here:
//!
//! | trigger   | GitHub                                              |
//! |-----------|-----------------------------------------------------|
//! | `pr`      | `pull_request` + `ci-test`/`ci-full` label by mode  |
//! | `push`    | `push` to the current branch                        |
//! | `release` | `workflow_dispatch` with the exact `commit_sha`     |

use std::path::{Component, Path};

use serde_json::{Value, json};

vocabulary!(
    /// The CI syntax: GitHub workflows now, `.gitlab-ci.yml` later.
    Provider, "provider" { Github => "github", Gitlab => "gitlab" }
);
vocabulary!(
    /// bosn's trigger vocabulary, mapped per provider.
    Trigger, "trigger" { Pr => "pr", Push => "push", Release => "release" }
);
vocabulary!(
    /// How much of the CI to run (fleet `ci-test`/`ci-full` tiers).
    Mode, "mode" { Minimal => "minimal", Test => "test", Full => "full" }
);

/// Providers with an executable adapter today.
pub fn require_supported(provider: Provider) -> Result<(), String> {
    match provider {
        Provider::Github => Ok(()),
        Provider::Gitlab => {
            Err("only the GitHub provider is supported so far (GitLab is planned)".into())
        }
    }
}

/// Detect the provider from the files in a checkout. Both present requires
/// an explicit choice; neither is a clear error.
pub fn detect(workspace: &Path, requested: Option<Provider>) -> Result<Provider, String> {
    let github = workspace.join(".github/workflows").is_dir();
    let gitlab = workspace.join(".gitlab-ci.yml").is_file();
    match (requested, github, gitlab) {
        (Some(p @ Provider::Github), true, _) | (Some(p @ Provider::Gitlab), _, true) => Ok(p),
        (Some(p), _, _) => Err(format!(
            "this checkout has no {} CI configuration",
            p.as_str()
        )),
        (None, true, false) => Ok(Provider::Github),
        (None, false, true) => Ok(Provider::Gitlab),
        (None, true, true) => Err(
            "this checkout has both GitHub workflows and .gitlab-ci.yml; pass --provider".into(),
        ),
        (None, false, false) => {
            Err("no CI configuration found (expected .github/workflows/ or .gitlab-ci.yml)".into())
        }
    }
}

/// Resolve the workflow file for a GitHub checkout: the requested one, else
/// `ci.yml`/`ci.yaml`, else the only workflow present.
pub fn github_workflow(workspace: &Path, requested: Option<&str>) -> Result<String, String> {
    if let Some(requested) = requested {
        let relative = Path::new(requested);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("workflow must be a relative path without traversal".into());
        }
        // Only workflows under .github/workflows/ run (the daemon refuses
        // anything else), so a bare name never resolves to a root-level file.
        let candidate = if requested.starts_with(".github/workflows/") {
            requested.to_string()
        } else {
            format!(".github/workflows/{requested}")
        };
        return if workspace.join(&candidate).is_file() {
            Ok(candidate)
        } else {
            Err(format!("workflow {requested:?} does not exist"))
        };
    }
    for name in ["ci.yml", "ci.yaml"] {
        let candidate = format!(".github/workflows/{name}");
        if workspace.join(&candidate).is_file() {
            return Ok(candidate);
        }
    }
    let mut found: Vec<String> = std::fs::read_dir(workspace.join(".github/workflows"))
        .map_err(|_| "no .github/workflows directory".to_string())?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
        .collect();
    found.sort();
    match found.as_slice() {
        [one] => Ok(format!(".github/workflows/{one}")),
        [] => Err("no workflow files in .github/workflows".into()),
        _ => Err(format!(
            "several workflows exist ({}); pass --workflow",
            found.join(", ")
        )),
    }
}

/// The repository name used when the checkout has no `origin`.
pub const LOCAL_REPOSITORY: &str = "local/repository";

/// `owner/repo` from an origin URL, when it names one.
pub fn repository(origin: Option<&str>) -> String {
    let fallback = LOCAL_REPOSITORY.to_string();
    let Some(origin) = origin else {
        return fallback;
    };
    let path = origin
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplitn(3, ['/', ':'])
        .take(2)
        .collect::<Vec<_>>();
    match path.as_slice() {
        [repo, owner] if !repo.is_empty() && !owner.is_empty() && !owner.contains('@') => {
            format!("{owner}/{repo}")
        }
        _ => fallback,
    }
}

/// The trigger/mode combination rules, shared by every provider.
pub fn validate(trigger: Trigger, mode: Mode, dirty: bool) -> Result<(), String> {
    match (trigger, mode) {
        (Trigger::Release, Mode::Full) | (Trigger::Pr, _) | (Trigger::Push, _) => {}
        (Trigger::Release, _) => return Err("release requires --mode full".into()),
    }
    if trigger == Trigger::Release && dirty {
        return Err(
            "release runs require a clean tree at the exact SHA; commit or stash first".into(),
        );
    }
    Ok(())
}

/// The provider event name and payload act receives through `--eventpath`.
pub fn github_event(
    trigger: Trigger,
    mode: Mode,
    sha: &str,
    branch: Option<&str>,
    repository: &str,
    pr_number: u64,
) -> (&'static str, Value) {
    let branch = branch.unwrap_or("main");
    let repo = json!({"full_name": repository, "name": repository.rsplit('/').next()});
    match trigger {
        Trigger::Pr => {
            let labels: Vec<Value> = match mode {
                Mode::Minimal => vec![],
                Mode::Test => vec![json!({"name": "ci-test"})],
                Mode::Full => vec![json!({"name": "ci-full"})],
            };
            (
                "pull_request",
                json!({
                    "action": "synchronize",
                    "number": pr_number,
                    "repository": repo,
                    "pull_request": {
                        "number": pr_number,
                        "head": {"sha": sha, "ref": branch},
                        "base": {"ref": "main"},
                        "labels": labels,
                    },
                }),
            )
        }
        Trigger::Push => (
            "push",
            json!({
                "ref": format!("refs/heads/{branch}"),
                "after": sha,
                "head_commit": {"id": sha},
                "repository": repo,
            }),
        ),
        Trigger::Release => (
            "workflow_dispatch",
            json!({
                "ref": format!("refs/heads/{branch}"),
                "inputs": {"tier": "full", "commit_sha": sha},
                "repository": repo,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn provider_detection_covers_each_shape() {
        let tmp = TemporaryDirectory::new().unwrap();
        let root = tmp.path();
        assert!(
            detect(root, None)
                .unwrap_err()
                .contains("no CI configuration")
        );
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        assert_eq!(detect(root, None).unwrap(), Provider::Github);
        assert!(detect(root, Some(Provider::Gitlab)).is_err());
        std::fs::write(root.join(".gitlab-ci.yml"), "x: 1\n").unwrap();
        assert!(detect(root, None).unwrap_err().contains("--provider"));
        assert_eq!(
            detect(root, Some(Provider::Gitlab)).unwrap(),
            Provider::Gitlab
        );
        std::fs::remove_dir_all(root.join(".github")).unwrap();
        assert_eq!(detect(root, None).unwrap(), Provider::Gitlab);
    }

    #[test]
    fn workflow_resolution_prefers_ci_then_the_only_one() {
        let tmp = TemporaryDirectory::new().unwrap();
        let wf = tmp.path().join(".github/workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("lint.yml"), "").unwrap();
        assert_eq!(
            github_workflow(tmp.path(), None).unwrap(),
            ".github/workflows/lint.yml"
        );
        std::fs::write(wf.join("other.yml"), "").unwrap();
        assert!(
            github_workflow(tmp.path(), None)
                .unwrap_err()
                .contains("--workflow")
        );
        std::fs::write(wf.join("ci.yml"), "").unwrap();
        assert_eq!(
            github_workflow(tmp.path(), None).unwrap(),
            ".github/workflows/ci.yml"
        );
        assert_eq!(
            github_workflow(tmp.path(), Some("other.yml")).unwrap(),
            ".github/workflows/other.yml"
        );
        assert!(github_workflow(tmp.path(), Some("../x.yml")).is_err());
        // A root-level file of the same name is never chosen: the daemon
        // only accepts workflows under .github/workflows/.
        std::fs::write(tmp.path().join("ci.yml"), "on: push").unwrap();
        assert_eq!(
            github_workflow(tmp.path(), Some("ci.yml")).unwrap(),
            ".github/workflows/ci.yml"
        );
        assert_eq!(
            github_workflow(tmp.path(), Some(".github/workflows/other.yml")).unwrap(),
            ".github/workflows/other.yml"
        );
        assert!(github_workflow(tmp.path(), Some("ci.yml/../ci.yml")).is_err());
    }

    #[test]
    fn trigger_mapping_golden() {
        let sha = "a".repeat(40);
        let (event, payload) = github_event(Trigger::Pr, Mode::Test, &sha, Some("feat"), "o/r", 7);
        assert_eq!(event, "pull_request");
        assert_eq!(payload["pull_request"]["labels"][0]["name"], "ci-test");
        assert_eq!(payload["pull_request"]["head"]["sha"], sha.as_str());
        let (_, full) = github_event(Trigger::Pr, Mode::Full, &sha, None, "o/r", 7);
        assert_eq!(full["pull_request"]["labels"][0]["name"], "ci-full");
        let (_, minimal) = github_event(Trigger::Pr, Mode::Minimal, &sha, None, "o/r", 7);
        assert_eq!(minimal["pull_request"]["labels"], json!([]));
        let (event, push) = github_event(Trigger::Push, Mode::Minimal, &sha, Some("dev"), "o/r", 0);
        assert_eq!(
            (event, push["ref"].as_str()),
            ("push", Some("refs/heads/dev"))
        );
        let (event, release) = github_event(Trigger::Release, Mode::Full, &sha, None, "o/r", 0);
        assert_eq!(event, "workflow_dispatch");
        assert_eq!(release["inputs"]["commit_sha"], sha.as_str());
    }

    #[test]
    fn release_rules_and_repository_names() {
        assert!(validate(Trigger::Release, Mode::Test, false).is_err());
        assert!(
            validate(Trigger::Release, Mode::Full, true)
                .unwrap_err()
                .contains("clean")
        );
        assert!(validate(Trigger::Release, Mode::Full, false).is_ok());
        assert!(validate(Trigger::Pr, Mode::Minimal, true).is_ok());
        assert_eq!(
            repository(Some("https://github.com/zackees/bosn.git")),
            "zackees/bosn"
        );
        assert_eq!(
            repository(Some("git@github.com:zackees/bosn.git")),
            "zackees/bosn"
        );
        assert_eq!(repository(None), "local/repository");
    }
}
