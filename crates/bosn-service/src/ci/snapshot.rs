//! Frozen source snapshots for CI runs.
//!
//! A snapshot is the working tree as it is right now: tracked files plus
//! untracked files, honouring `.gitignore`, with deleted tracked files left
//! out. Symlinks are copied as links (never followed), executable bits are
//! kept, and submodule checkouts are included the same way. The run then
//! reads only this copy, so editing the workspace mid-run cannot change it.
//!
//! The digest is stable across snapshots of an unchanged tree and covers
//! every path, type, mode bit and byte; a one-byte edit changes it.
//!
//! The copy is also a Git repository holding the `HEAD` commit. A dirty tree
//! is committed on top of it (a synthetic commit, #394), so the job sees the
//! work under test as a clean checkout of `HEAD`, and a workflow that cleans
//! its tree (`git restore`, `git reset --hard`) cannot silently build the
//! last commit instead. The run record still says `sha + dirty`.
//!
//! A pull request run's copy also holds its base branch ([`base`], #403).

mod base;
pub use base::{BaseRef, DEFAULT_BASE_BRANCH, valid_branch};

use std::{
    io,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use kernal_api::{SpawnSpec, StreamMode, hash::Sha256Hasher};

const GIT_DEADLINE: Duration = Duration::from_secs(120);
const GIT_OUTPUT_LIMIT: usize = 256 * 1024 * 1024;
/// Upper bound on one hashed file (larger files are refused, not truncated).
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotReceipt {
    /// `HEAD` commit of the workspace.
    pub sha: String,
    /// Branch name when `HEAD` is attached.
    pub branch: Option<String>,
    /// sha256 over the copied tree (always present).
    pub tree_digest: String,
    /// Git tree object of the effective frozen checkout commit.
    pub git_tree: String,
    /// True when the tree differs from `HEAD` (tracked edits, deletions or
    /// untracked files). Receipts then record `sha + dirty: tree_digest`.
    pub dirty: bool,
    /// The synthetic commit the run checks out when `dirty`: `HEAD` plus the
    /// uncommitted work, so a workflow that cleans its tree (`git restore`,
    /// `git reset --hard`) still builds what is under test (#394).
    pub commit: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// `origin` URL, used for the runner's repository identity.
    pub origin: Option<String>,
}

/// Who the synthetic commit is by; its dates are the base commit's.
const SYNTHETIC_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "bosn"),
    ("GIT_AUTHOR_EMAIL", "bosn@localhost"),
    ("GIT_COMMITTER_NAME", "bosn"),
    ("GIT_COMMITTER_EMAIL", "bosn@localhost"),
];
const SYNTHETIC_MESSAGE: &str = "bosn: uncommitted work under test";

fn git(dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    git_env(dir, &[], args)
}

fn git_env(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> io::Result<Vec<u8>> {
    let spec = SpawnSpec::new("git")
        .current_dir(dir)
        .stdin(StreamMode::Null)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .env("GIT_OPTIONAL_LOCKS", "0");
    let spec = env.iter().fold(spec, |spec, (k, v)| spec.env(*k, *v));
    let spec = args.iter().fold(spec, |spec, arg| spec.arg(*arg));
    let output = kernal_api::run_bounded_command(spec, GIT_DEADLINE, GIT_OUTPUT_LIMIT)
        .map_err(|e| io::Error::other(format!("git {}: {e}", args.join(" "))))?;
    if output.exit.raw_code() != 0 {
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn text(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes)
        .map(|s| s.trim().to_string())
        .map_err(|_| io::Error::other("git output is not UTF-8"))
}

/// Paths git would put in a commit of the working tree right now, relative
/// to `root`, recursing into submodule checkouts.
fn tree_paths(root: &Path, prefix: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let listed = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut seen = std::collections::BTreeSet::new();
    for raw in listed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let relative = PathBuf::from(
            std::str::from_utf8(raw).map_err(|_| io::Error::other("non-UTF-8 path in tree"))?,
        );
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(io::Error::other("git listed an unsafe path"));
        }
        if !seen.insert(relative.clone()) {
            continue;
        }
        let absolute = root.join(&relative);
        let Ok(meta) = std::fs::symlink_metadata(&absolute) else {
            continue; // deleted tracked file: not part of the working tree
        };
        if meta.is_dir() {
            // A gitlink (submodule checkout): include its own working tree.
            if absolute.join(".git").exists() {
                tree_paths(&absolute, &prefix.join(&relative), out)?;
            }
            continue;
        }
        out.push(prefix.join(relative));
    }
    Ok(())
}

/// What `HEAD` is and whether the working tree differs from it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Head {
    pub root: PathBuf,
    pub sha: String,
    pub branch: Option<String>,
    pub origin: Option<String>,
    pub dirty: bool,
}

/// Probe a Git checkout root without copying anything.
pub fn head(workspace: &Path) -> io::Result<Head> {
    let root = workspace.canonicalize()?;
    let top =
        PathBuf::from(text(git(&root, &["rev-parse", "--show-toplevel"])?)?).canonicalize()?;
    if top != root {
        return Err(io::Error::other("workspace must be the Git checkout root"));
    }
    let optional = |args: &[&str]| {
        git(&root, args)
            .ok()
            .and_then(|o| text(o).ok())
            .filter(|v| !v.is_empty())
    };
    let status = git(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    Ok(Head {
        sha: text(git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?)?,
        branch: optional(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
        origin: optional(&["remote", "get-url", "origin"]),
        dirty: !status.is_empty(),
        root,
    })
}

/// Copy the working tree of `workspace` (a Git checkout root) into `dest`,
/// which must not exist yet, and make it a Git repository holding only the
/// `HEAD` commit (refs, remote, a depth-1 pack and an index), so the runner
/// and the workflow's own `git` commands see the right commit. With a
/// `base` (a pull request run) it also holds that branch, and both tips'
/// history down to their merge base. A dirty tree is then committed on top
/// of `HEAD` ([`commit_working_tree`]).
pub fn snapshot(
    workspace: &Path,
    dest: &Path,
    base: Option<&BaseRef>,
) -> io::Result<SnapshotReceipt> {
    let Head {
        root,
        sha,
        branch,
        origin,
        dirty,
    } = head(workspace)?;
    let mut paths = Vec::new();
    tree_paths(&root, Path::new(""), &mut paths)?;
    paths.sort();
    std::fs::create_dir(dest)?;
    let mut hasher = Sha256Hasher::new();
    let mut files = 0u64;
    let mut bytes = 0u64;
    for relative in &paths {
        let from = root.join(relative);
        let to = dest.join(relative);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let meta = std::fs::symlink_metadata(&from)?;
        let name = relative.to_string_lossy();
        hasher.update(name.as_bytes());
        hasher.update([0]);
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&from)?;
            hasher.update(b"link\0");
            hasher.update(target.to_string_lossy().as_bytes());
            symlink(&target, &to)?;
        } else if meta.is_file() {
            if meta.len() > MAX_FILE_BYTES {
                return Err(io::Error::other(format!("{name} is too large to snapshot")));
            }
            let executable = is_executable(&meta);
            kernal_api::platform::fs::copy_file(&from, &to)?;
            // Hash the copy, not the source: the copy is what the run sees.
            let digest =
                kernal_api::hash::sha256_reader(std::fs::File::open(&to)?, MAX_FILE_BYTES)?;
            hasher.update(if executable { b"x\0" } else { b"f\0" });
            hasher.update(digest.to_hex().as_bytes());
            set_mode(&to, executable)?;
            files += 1;
            bytes += meta.len();
        } else {
            continue; // sockets, FIFOs and devices are never source
        }
        hasher.update([b'\n']);
    }
    // An edit between the status probe and the copy must not produce a
    // snapshot labelled clean: probe again after copying.
    let after = head(&root)?;
    if after.sha != sha {
        return Err(io::Error::other("HEAD moved while the snapshot was taken"));
    }
    let dirty = dirty || after.dirty;
    write_git_metadata(dest, &sha, branch.as_deref(), origin.as_deref())?;
    let depths = base::Depths::of(&root, &sha, base)?;
    let url = format!("file://{}", root.display()).replace(' ', "%20");
    write_head_objects(&url, dest, &sha, depths.head)?;
    if let Some(base) = base {
        base::fetch_base(dest, &url, base, depths.base)?;
    }
    let commit = if dirty {
        commit_working_tree(dest, &sha)?
    } else {
        None
    };
    let git_tree = effective_git_tree(dest, commit.as_deref().unwrap_or(&sha))?;
    Ok(SnapshotReceipt {
        sha,
        branch,
        tree_digest: hasher.finalize().to_hex(),
        git_tree,
        dirty,
        commit,
        files,
        bytes,
        origin,
    })
}

/// Read the tree of an explicit effective commit, never infer it from HEAD.
/// This is source identity only, not an approved cache-writer grant.
pub(crate) fn effective_git_tree(root: &Path, commit: &str) -> io::Result<String> {
    if !super::wire::valid_sha(commit) {
        return Err(io::Error::other("invalid effective checkout commit"));
    }
    let tree = text(git(
        root,
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
    )?)?;
    if !super::wire::valid_sha(&tree) {
        return Err(io::Error::other("unsupported Git tree identity"));
    }
    Ok(tree)
}

/// The `.git` refs, `HEAD` and remote act's revision/ref/remote probes read;
/// [`write_head_objects`] then adds the `HEAD` commit itself.
///
/// `refs/bosn/base` always names the commit the snapshot was taken from
/// (`git diff refs/bosn/base` shows the uncommitted work under test). It
/// also keeps `refs/` from being empty: act copies files, not empty
/// directories, and without `refs/` a detached `HEAD`'s copy is not a Git
/// repository at all (#393).
fn write_git_metadata(
    dest: &Path,
    sha: &str,
    branch: Option<&str>,
    origin: Option<&str>,
) -> io::Result<()> {
    let git = dest.join(".git");
    for dir in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
        "refs/bosn",
    ] {
        std::fs::create_dir_all(git.join(dir))?;
    }
    std::fs::write(git.join("refs/bosn/base"), format!("{sha}\n"))?;
    match branch {
        Some(branch) if !branch.contains("..") && !branch.starts_with('/') => {
            let reference = git.join("refs/heads").join(branch);
            if let Some(parent) = reference.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(reference, format!("{sha}\n"))?;
            std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
        }
        _ => std::fs::write(git.join("HEAD"), format!("{sha}\n"))?,
    }
    let mut config = String::from("[core]\n\trepositoryformatversion = 0\n\tbare = false\n");
    if let Some(origin) = origin.filter(|o| !o.contains(['\n', '"', '\\'])) {
        config.push_str(&format!(
            "[remote \"origin\"]\n\turl = {origin}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
        ));
    }
    std::fs::write(git.join("config"), config)
}

/// Fetch the `HEAD` commit's objects, `depth` commits deep (1 unless a pull
/// request run needs the merge base: no history the run was not given), and
/// build an index matching it, so a workflow's
/// `git rev-parse`, `git diff` and `git status` see a real checkout with the
/// uncommitted work on top. Always one pack (`fetch.unpackLimit=1`): act
/// copies files, not empty directories, and one pack copies faster than
/// thousands of loose objects.
fn write_head_objects(url: &str, dest: &Path, sha: &str, depth: usize) -> io::Result<()> {
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
            "HEAD",
        ],
    )?;
    let fetched = text(git(dest, &["rev-parse", "FETCH_HEAD"])?)?;
    // FETCH_HEAD names the host path; the run does not need it.
    let _ = std::fs::remove_file(dest.join(".git/FETCH_HEAD"));
    if fetched != sha {
        return Err(io::Error::other("HEAD moved while the snapshot was taken"));
    }
    git(dest, &["read-tree", "HEAD"]).map(|_| ())
}

/// Commit the copied working tree on top of `base` and move `HEAD` (its
/// branch when attached) to it, so the job sees the work under test as a
/// clean checkout. Identity and dates are fixed (the dates are `base`'s), so
/// the same tree on the same base is always the same commit and identical
/// dirty runs still coalesce. `None` when the tree is `base`'s after all.
fn commit_working_tree(dest: &Path, base: &str) -> io::Result<Option<String>> {
    git(dest, &["add", "--all"])?;
    let tree = text(git(dest, &["write-tree"])?)?;
    if tree == text(git(dest, &["rev-parse", &format!("{base}^{{tree}}")])?)? {
        return Ok(None);
    }
    let date = format!(
        "@{} +0000",
        text(git(dest, &["show", "-s", "--format=%ct", base])?)?
    );
    let env: Vec<(&str, &str)> = SYNTHETIC_IDENTITY
        .into_iter()
        .chain([("GIT_AUTHOR_DATE", &*date), ("GIT_COMMITTER_DATE", &*date)])
        .collect();
    let commit = text(git_env(
        dest,
        &env,
        &[
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            base,
            "-m",
            SYNTHETIC_MESSAGE,
        ],
    )?)?;
    git(dest, &["update-ref", "HEAD", &commit, base])?;
    Ok(Some(commit))
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    // Windows hosts copy the link text; the Linux runner sees a plain file.
    std::fs::write(link, target.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    pub(crate) fn sh(dir: &Path, script: &str) {
        let out = kernal_api::run_bounded_command(
            SpawnSpec::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(dir)
                .stdin(StreamMode::Null)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t"),
            Duration::from_secs(60),
            1 << 20,
        )
        .unwrap();
        assert_eq!(
            out.exit.raw_code(),
            0,
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo(tmp: &Path) -> PathBuf {
        let ws = tmp.join("ws");
        std::fs::create_dir(&ws).unwrap();
        sh(
            &ws,
            "git init -q -b main . && printf 'target/\\n' > .gitignore && \
             printf 'a\\n' > tracked.txt && printf 'gone\\n' > deleted.txt && \
             printf '#!/bin/sh\\n' > run.sh && chmod +x run.sh && \
             ln -s tracked.txt inside-link && ln -s /etc/hostname outside-link && \
             git add -A && git commit -qm init && \
             git remote add origin https://github.com/example/repo.git",
        );
        ws
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_copies_the_working_tree_as_is() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        let clean = snapshot(&ws, &tmp.path().join("s0"), None).unwrap();
        assert!(!clean.dirty);
        let again = snapshot(&ws, &tmp.path().join("s1"), None).unwrap();
        assert_eq!(
            clean.tree_digest, again.tree_digest,
            "stable when unchanged"
        );

        sh(
            &ws,
            "printf 'b\\n' >> tracked.txt && rm deleted.txt && printf 'new\\n' > untracked.txt && \
             mkdir -p target && printf 'build\\n' > target/out.o",
        );
        let dirty = snapshot(&ws, &tmp.path().join("s2"), None).unwrap();
        let s = tmp.path().join("s2");
        assert!(dirty.dirty);
        assert_ne!(dirty.tree_digest, clean.tree_digest);
        assert_eq!(
            std::fs::read_to_string(s.join("tracked.txt")).unwrap(),
            "a\nb\n"
        );
        assert!(
            s.join("untracked.txt").exists(),
            "untracked work is included"
        );
        assert!(
            !s.join("deleted.txt").exists(),
            "deleted tracked file is gone"
        );
        assert!(
            !s.join("target").exists(),
            "ignored build output is excluded"
        );
        assert_eq!(
            std::fs::read_link(s.join("inside-link")).unwrap(),
            Path::new("tracked.txt")
        );
        assert_eq!(
            std::fs::read_link(s.join("outside-link")).unwrap(),
            Path::new("/etc/hostname"),
            "links are copied, never followed"
        );
        let mode = std::fs::metadata(s.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bit kept");
        assert_eq!(dirty.branch.as_deref(), Some("main"));
        assert_eq!(
            dirty.origin.as_deref(),
            Some("https://github.com/example/repo.git")
        );
        assert_eq!(
            Some(
                std::fs::read_to_string(s.join(".git/refs/heads/main"))
                    .unwrap()
                    .trim()
            ),
            dirty.commit.as_deref(),
            "the branch names the commit under test"
        );

        // One byte changes the digest; editing the workspace after the
        // snapshot does not change the snapshot.
        sh(&ws, "printf 'c' >> untracked.txt");
        let edited = snapshot(&ws, &tmp.path().join("s3"), None).unwrap();
        assert_ne!(edited.tree_digest, dirty.tree_digest);
        assert_eq!(
            std::fs::read_to_string(s.join("untracked.txt")).unwrap(),
            "new\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn submodule_checkout_is_included() {
        let tmp = TemporaryDirectory::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        sh(
            &sub,
            "git init -q -b main . && printf 's\\n' > s.txt && git add -A && git commit -qm s",
        );
        let ws = repo(tmp.path());
        sh(
            &ws,
            &format!(
                "git -c protocol.file.allow=always submodule add -q {} vendor/sub && git commit -qm sub",
                sub.display()
            ),
        );
        let receipt = snapshot(&ws, &tmp.path().join("snap"), None).unwrap();
        assert!(!receipt.dirty);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("snap/vendor/sub/s.txt")).unwrap(),
            "s\n"
        );
    }

    #[test]
    fn non_root_workspace_is_refused() {
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        std::fs::create_dir(ws.join("nested")).unwrap();
        assert!(snapshot(&ws.join("nested"), &tmp.path().join("x"), None).is_err());
    }

    /// `git` in `dir`, which must succeed; stdout with the end trimmed
    /// (porcelain status starts with a meaningful space).
    pub(crate) fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = kernal_api::run_bounded_command(
            args.iter()
                .fold(SpawnSpec::new("git").current_dir(dir), |s, a| s.arg(*a))
                .stdin(StreamMode::Null)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped),
            Duration::from_secs(30),
            1 << 20,
        )
        .unwrap();
        assert_eq!(
            out.exit.raw_code(),
            0,
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    /// What act puts in the job container: files and links, without the
    /// empty directories.
    #[cfg(unix)]
    pub(crate) fn copy_files_only(from: &Path, to: &Path) {
        for entry in std::fs::read_dir(from).unwrap().map(Result::unwrap) {
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                copy_files_only(&source, &target);
                continue;
            }
            std::fs::create_dir_all(to).unwrap();
            if kind.is_symlink() {
                symlink(&std::fs::read_link(&source).unwrap(), &target).unwrap();
            } else {
                std::fs::copy(&source, &target).unwrap();
            }
        }
    }

    #[test]
    fn the_snapshot_is_a_real_repository_holding_only_the_head_commit() {
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        sh(&ws, "printf 'b\\n' > tracked.txt && git commit -qam second");
        let dest = tmp.path().join("s");
        let receipt = snapshot(&ws, &dest, None).unwrap();
        let git = |args: &[&str]| git_in(&dest, args);
        assert!(!receipt.dirty);
        assert_eq!(receipt.commit, None, "a clean tree is the commit itself");
        assert_eq!(
            git(&["rev-parse", "HEAD"]),
            receipt.sha,
            "workflows check HEAD"
        );
        git(&["cat-file", "-e", "HEAD^{tree}"]);
        assert_eq!(
            git(&["rev-list", "--count", "HEAD"]),
            "1",
            "no history beyond HEAD"
        );
        assert_eq!(git(&["status", "--porcelain"]), "");
        assert_eq!(
            git(&["rev-parse", "refs/bosn/base"]),
            receipt.sha,
            "the commit the snapshot was taken from is named"
        );
        // act copies files, not empty directories: objects must hold a file.
        let objects = std::fs::read_dir(dest.join(".git/objects/pack"))
            .unwrap()
            .count();
        assert!(objects > 0, "the HEAD commit's objects are packed");
    }

    /// #394: a workflow that cleans its tree (`git restore`, `git reset
    /// --hard`) must still build the uncommitted work under test.
    #[test]
    fn a_dirty_snapshot_is_a_synthetic_commit_on_top_of_head() {
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        sh(
            &ws,
            "printf 'edit\\n' >> tracked.txt && rm deleted.txt && printf 'new\\n' > untracked.txt",
        );
        let dest = tmp.path().join("s");
        let receipt = snapshot(&ws, &dest, None).unwrap();
        let git = |args: &[&str]| git_in(&dest, args);
        assert!(receipt.dirty, "the receipt still says sha + dirty");
        let commit = receipt.commit.clone().expect("a dirty tree is committed");
        assert_ne!(commit, receipt.sha);
        assert_eq!(
            git(&["rev-parse", "HEAD"]),
            commit,
            "the job sees it as HEAD"
        );
        assert_eq!(git(&["rev-parse", "HEAD^"]), receipt.sha, "on top of HEAD");
        assert_eq!(git(&["rev-parse", "refs/heads/main"]), commit);
        assert_eq!(git(&["rev-parse", "refs/bosn/base"]), receipt.sha);
        assert_eq!(git(&["status", "--porcelain"]), "", "a clean checkout");
        assert_eq!(
            git(&["diff", "--name-status", "refs/bosn/base", "HEAD"]),
            "D\tdeleted.txt\nM\ttracked.txt\nA\tuntracked.txt",
            "the commit holds exactly the uncommitted work"
        );
        git(&["restore", "--staged", "--worktree", "--", "."]);
        git(&["reset", "--hard", "--quiet"]);
        assert_eq!(
            std::fs::read_to_string(dest.join("tracked.txt")).unwrap(),
            "a\nedit\n",
            "cleaning the tree keeps the work under test"
        );
        assert!(dest.join("untracked.txt").exists());
        let again = snapshot(&ws, &tmp.path().join("s2"), None).unwrap();
        assert_eq!(
            again.commit, receipt.commit,
            "the same tree gives the same commit, so identical runs coalesce"
        );
    }

    /// #393: a detached `HEAD` (a worktree created at a SHA) gives the job
    /// the same repository, `HEAD` naming the commit directly, even after
    /// act drops the empty directories.
    #[cfg(unix)]
    #[test]
    fn a_detached_head_survives_a_copy_without_empty_directories() {
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        for (name, setup) in [
            ("clean", "git checkout -q --detach"),
            ("dirty", "printf 'edit\\n' >> tracked.txt"),
        ] {
            sh(&ws, setup);
            let dest = tmp.path().join(name);
            let receipt = snapshot(&ws, &dest, None).unwrap();
            assert_eq!(receipt.branch, None, "{name}");
            let job = tmp.path().join(format!("{name}-job"));
            copy_files_only(&dest, &job);
            let head = receipt.commit.as_deref().unwrap_or(&receipt.sha);
            assert_eq!(git_in(&job, &["rev-parse", "HEAD"]), head, "{name}");
            assert_eq!(
                std::fs::read_to_string(job.join(".git/HEAD"))
                    .unwrap()
                    .trim(),
                head,
                "{name}: HEAD stays detached"
            );
            assert_eq!(git_in(&job, &["status", "--porcelain"]), "", "{name}");
        }
    }

    /// Opt-in (writes about 4 GiB to the temp directory):
    /// `cargo test -p bosn-service --lib -- --ignored a_2_gib_file`.
    #[test]
    #[ignore = "writes about 4 GiB; run on request"]
    fn a_2_gib_file_is_copied_whole_and_one_byte_changes_the_digest() {
        use std::io::{Seek, SeekFrom, Write};
        const SIZE: u64 = 2 << 30;
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        let big = ws.join("big.bin");
        std::fs::File::create(&big).unwrap().set_len(SIZE).unwrap();
        let first = snapshot(&ws, &tmp.path().join("s0"), None).unwrap();
        assert!(first.dirty, "an untracked file makes the tree dirty");
        assert!(first.bytes >= SIZE, "{} bytes", first.bytes);
        let copied = tmp.path().join("s0").join("big.bin");
        assert_eq!(std::fs::metadata(&copied).unwrap().len(), SIZE);
        std::fs::remove_dir_all(tmp.path().join("s0")).unwrap();

        let again = snapshot(&ws, &tmp.path().join("s1"), None).unwrap();
        assert_eq!(
            first.tree_digest, again.tree_digest,
            "stable when unchanged"
        );
        std::fs::remove_dir_all(tmp.path().join("s1")).unwrap();

        let mut file = std::fs::OpenOptions::new().write(true).open(&big).unwrap();
        file.seek(SeekFrom::Start(SIZE - 1)).unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        let edited = snapshot(&ws, &tmp.path().join("s2"), None).unwrap();
        assert_ne!(
            first.tree_digest, edited.tree_digest,
            "the last byte counts"
        );
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_receipt_binds_clean_git_tree() {
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        let clean_root = tmp.path().join("clean");
        let clean = snapshot(&ws, &clean_root, None).unwrap();
        let original_tree = text(git(&clean_root, &["rev-parse", "HEAD^{tree}"]).unwrap()).unwrap();
        assert_eq!(clean.git_tree, original_tree);
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_receipt_binds_dirty_git_tree() {
        let tmp = TemporaryDirectory::new().unwrap();
        let ws = repo(tmp.path());
        let clean_root = tmp.path().join("clean");
        let clean = snapshot(&ws, &clean_root, None).unwrap();
        let original_tree = text(git(&clean_root, &["rev-parse", "HEAD^{tree}"]).unwrap()).unwrap();
        std::fs::write(ws.join("tracked.txt"), "dirty bytes").unwrap();
        let dirty_root = tmp.path().join("dirty");
        let dirty = snapshot(&ws, &dirty_root, None).unwrap();
        assert_eq!(dirty.sha, clean.sha);
        assert!(dirty.dirty);
        assert!(dirty.commit.is_some());
        let effective_tree =
            text(git(&dirty_root, &["rev-parse", "HEAD^{tree}"]).unwrap()).unwrap();
        assert_eq!(dirty.git_tree, effective_tree);
        assert_ne!(dirty.git_tree, original_tree);
    }
}
