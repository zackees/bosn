//! The base branch a `--trigger pr` run carries (#403).
//!
//! On GitHub, a pull request checkout with `fetch-depth: 0` has
//! `origin/<base>`, so a workflow can run `git merge-base origin/main HEAD`.
//! A snapshot otherwise holds only the `HEAD` commit; a PR run's snapshot
//! also holds the base branch as `refs/remotes/origin/<base>` at the commit
//! the event payload names (`pull_request.base.sha`), with the history of
//! both tips down to their merge base and no deeper.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{git, text};
use crate::ci::wire::{valid_name, valid_sha};

/// The base branch when `origin` names no default branch.
pub const DEFAULT_BASE_BRANCH: &str = "main";

/// The branch a local pull request targets, at its commit in the workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BaseRef {
    /// `pull_request.base.ref`, e.g. `main`.
    pub branch: String,
    /// `pull_request.base.sha`: the branch's commit when the run was taken.
    pub sha: String,
}

impl BaseRef {
    /// The base of a pull request from the checkout at `root`: `origin`'s
    /// default branch (`refs/remotes/origin/HEAD`), else
    /// [`DEFAULT_BASE_BRANCH`], at `origin/<branch>`, else at the local
    /// branch. `None` when the checkout has neither.
    pub fn of_workspace(root: &Path) -> Option<Self> {
        let optional = |args: &[&str]| {
            git(root, args)
                .ok()
                .and_then(|o| text(o).ok())
                .filter(|v| !v.is_empty())
        };
        let branch = optional(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .and_then(|r| r.strip_prefix("origin/").map(str::to_owned))
        .filter(|b| valid_branch(b))
        .unwrap_or_else(|| DEFAULT_BASE_BRANCH.into());
        let sha = [
            format!("refs/remotes/origin/{branch}"),
            format!("refs/heads/{branch}"),
        ]
        .iter()
        .find_map(|r| {
            optional(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{r}^{{commit}}"),
            ])
        })?;
        Some(Self { branch, sha })
    }

    /// A branch that is a safe ref path and a full commit SHA.
    pub fn is_valid(&self) -> bool {
        valid_sha(&self.sha) && valid_branch(&self.branch)
    }

    /// Where the snapshot keeps it, as a `fetch-depth: 0` checkout does.
    pub fn remote_ref(&self) -> String {
        format!("refs/remotes/origin/{}", self.branch)
    }
}

/// A branch name usable as a ref path: no traversal, no option shape, no
/// component Git refuses (`.lock`, a leading dot, an empty one).
pub fn valid_branch(branch: &str) -> bool {
    valid_name(branch, 200)
        && !branch.starts_with('-')
        && !branch.contains("..")
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

/// How deep each tip is fetched so their merge base is in the snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Depths {
    pub head: usize,
    pub base: usize,
}

impl Depths {
    /// One commit each when there is no base or no merge base (unrelated
    /// histories have none on GitHub either); otherwise each tip down to and
    /// including the merge base. Every commit between a tip and the merge
    /// base is at most that many parents away, so a shallow fetch of that
    /// depth reaches it along every path.
    pub(super) fn of(root: &Path, head: &str, base: Option<&BaseRef>) -> std::io::Result<Self> {
        let Some(base) = base else {
            return Ok(Self { head: 1, base: 1 });
        };
        let Ok(merge_base) = git(root, &["merge-base", head, &base.sha]).and_then(text) else {
            return Ok(Self { head: 1, base: 1 });
        };
        let depth = |tip: &str| -> std::io::Result<usize> {
            let count = text(git(
                root,
                &["rev-list", "--count", &format!("{merge_base}..{tip}")],
            )?)?;
            count
                .parse::<usize>()
                .map(|n| n + 1)
                .map_err(std::io::Error::other)
        };
        Ok(Self {
            head: depth(head)?,
            base: depth(&base.sha)?,
        })
    }
}

/// Fetch `base` from the workspace at `url` into the snapshot at `dest` as
/// [`BaseRef::remote_ref`], `depth` commits deep.
pub(super) fn fetch_base(
    dest: &Path,
    url: &str,
    base: &BaseRef,
    depth: usize,
) -> std::io::Result<()> {
    let refspec = format!("{}:{}", base.sha, base.remote_ref());
    git(
        dest,
        &[
            "-c",
            "fetch.unpackLimit=1",
            "fetch",
            "--quiet",
            "--depth",
            &depth.to_string(),
            "--no-tags",
            url,
            &refspec,
        ],
    )?;
    let _ = std::fs::remove_file(dest.join(".git/FETCH_HEAD"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{git_in, sh};
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    /// `main` gets one commit after `feature` is cut, then exists only as
    /// `origin/main`; returns the workspace and the merge base.
    fn diverged(tmp: &Path) -> (std::path::PathBuf, String) {
        let ws = tmp.join("ws");
        std::fs::create_dir(&ws).unwrap();
        sh(
            &ws,
            "git init -q -b main . && printf 'a\\n' > a.txt && git add -A && git commit -qm init && \
             git commit -q --allow-empty -m second && \
             git remote add origin https://github.com/example/repo.git",
        );
        let merge_base = git_in(&ws, &["rev-parse", "HEAD"]);
        sh(
            &ws,
            "git checkout -q -b feature && git commit -q --allow-empty -m f1 && \
             git commit -q --allow-empty -m f2 && git checkout -q main && \
             git commit -q --allow-empty -m moved && git update-ref refs/remotes/origin/main main && \
             git checkout -q feature && git branch -q -D main",
        );
        (ws, merge_base)
    }

    #[test]
    fn the_base_is_origins_default_branch_else_main() {
        let tmp = TemporaryDirectory::new().unwrap();
        let (ws, _) = diverged(tmp.path());
        let base = BaseRef::of_workspace(&ws).expect("origin/main exists");
        assert_eq!(base.branch, "main");
        assert_eq!(base.sha, git_in(&ws, &["rev-parse", "origin/main"]));
        sh(
            &ws,
            "git update-ref refs/remotes/origin/trunk origin/main~1 && \
             git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/trunk",
        );
        let trunk = BaseRef::of_workspace(&ws).unwrap();
        assert_eq!(trunk.branch, "trunk", "origin/HEAD names the default");
        assert_eq!(trunk.sha, git_in(&ws, &["rev-parse", "origin/main~1"]));
        sh(
            &ws,
            "git symbolic-ref --delete refs/remotes/origin/HEAD && \
             git update-ref -d refs/remotes/origin/main && git update-ref -d refs/remotes/origin/trunk",
        );
        assert_eq!(BaseRef::of_workspace(&ws), None, "no base to carry");
    }

    /// #403: `git merge-base origin/main HEAD` works in the snapshot, also
    /// after act copies it without empty directories, for a clean and a
    /// dirty tree.
    #[cfg(unix)]
    #[test]
    fn a_pr_snapshot_carries_origin_main_and_history_to_the_merge_base() {
        let tmp = TemporaryDirectory::new().unwrap();
        let (ws, merge_base) = diverged(tmp.path());
        let base = BaseRef::of_workspace(&ws).unwrap();
        for (name, setup) in [("clean", "true"), ("dirty", "printf 'edit\\n' >> a.txt")] {
            sh(&ws, setup);
            let dest = tmp.path().join(name);
            let receipt = super::super::snapshot(&ws, &dest, Some(&base)).unwrap();
            let job = tmp.path().join(format!("{name}-job"));
            super::super::tests::copy_files_only(&dest, &job);
            let git = |args: &[&str]| git_in(&job, args);
            assert_eq!(git(&["rev-parse", "origin/main"]), base.sha, "{name}");
            assert_eq!(
                git(&["merge-base", "origin/main", "HEAD"]),
                merge_base,
                "{name}"
            );
            assert_eq!(
                git(&["rev-list", "--count", "HEAD"]),
                if receipt.dirty { "4" } else { "3" },
                "{name}: history down to the merge base, no deeper"
            );
            assert_eq!(git(&["rev-list", "--count", "origin/main"]), "2", "{name}");
            assert_eq!(git(&["status", "--porcelain"]), "", "{name}");
        }
    }

    #[test]
    fn a_push_snapshot_holds_only_head() {
        let tmp = TemporaryDirectory::new().unwrap();
        let (ws, _) = diverged(tmp.path());
        let dest = tmp.path().join("s");
        super::super::snapshot(&ws, &dest, None).unwrap();
        assert_eq!(git_in(&dest, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git_in(&dest, &["for-each-ref", "refs/remotes"]), "");
    }

    #[test]
    fn branch_names_are_safe_ref_paths() {
        for good in ["main", "release/1.2", "feat-x_y"] {
            assert!(valid_branch(good), "{good}");
        }
        for bad in [
            "", "-x", "a..b", ".hidden", "a/.b", "x.lock", "a//b", "a b", "/abs",
        ] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }
}
