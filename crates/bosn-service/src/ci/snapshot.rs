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
    /// True when the tree differs from `HEAD` (tracked edits, deletions or
    /// untracked files). Receipts then record `sha + dirty: tree_digest`.
    pub dirty: bool,
    pub files: u64,
    pub bytes: u64,
    /// `origin` URL, used for the runner's repository identity.
    pub origin: Option<String>,
}

fn git(dir: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let spec = args.iter().fold(
        SpawnSpec::new("git")
            .current_dir(dir)
            .stdin(StreamMode::Null)
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped)
            .env("GIT_OPTIONAL_LOCKS", "0"),
        |spec, arg| spec.arg(*arg),
    );
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
/// which must not exist yet, and write minimal Git metadata naming `HEAD` so
/// the runner sees the right commit and repository.
pub fn snapshot(workspace: &Path, dest: &Path) -> io::Result<SnapshotReceipt> {
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
    write_git_metadata(dest, &sha, branch.as_deref(), origin.as_deref())?;
    Ok(SnapshotReceipt {
        sha,
        branch,
        tree_digest: hasher.finalize().to_hex(),
        dirty,
        files,
        bytes,
        origin,
    })
}

/// Just enough of a `.git` for act's revision/ref/remote probes. It holds no
/// objects, so nothing in the run can read history it was not given.
fn write_git_metadata(
    dest: &Path,
    sha: &str,
    branch: Option<&str>,
    origin: Option<&str>,
) -> io::Result<()> {
    let git = dest.join(".git");
    for dir in ["objects/info", "objects/pack", "refs/heads", "refs/tags"] {
        std::fs::create_dir_all(git.join(dir))?;
    }
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
mod tests {
    use super::*;
    use kernal_api::platform::fs::TemporaryDirectory;

    fn sh(dir: &Path, script: &str) {
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
        let clean = snapshot(&ws, &tmp.path().join("s0")).unwrap();
        assert!(!clean.dirty);
        let again = snapshot(&ws, &tmp.path().join("s1")).unwrap();
        assert_eq!(
            clean.tree_digest, again.tree_digest,
            "stable when unchanged"
        );

        sh(
            &ws,
            "printf 'b\\n' >> tracked.txt && rm deleted.txt && printf 'new\\n' > untracked.txt && \
             mkdir -p target && printf 'build\\n' > target/out.o",
        );
        let dirty = snapshot(&ws, &tmp.path().join("s2")).unwrap();
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
            std::fs::read_to_string(s.join(".git/refs/heads/main"))
                .unwrap()
                .trim(),
            dirty.sha
        );

        // One byte changes the digest; editing the workspace after the
        // snapshot does not change the snapshot.
        sh(&ws, "printf 'c' >> untracked.txt");
        let edited = snapshot(&ws, &tmp.path().join("s3")).unwrap();
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
        let receipt = snapshot(&ws, &tmp.path().join("snap")).unwrap();
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
        assert!(snapshot(&ws.join("nested"), &tmp.path().join("x")).is_err());
    }
}
