//! Bounded collection of a selected Docker build context.
//!
//! This is deliberately not a filesystem sandbox or an atomic tree snapshot.
//! Callers must trust the materialization root and its ancestors.  Bounded
//! regular-file reads reject observable final-file replacement races, but directory changes
//! and filesystems with coarse timestamps can still make a tree non-atomic.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use kernal_api::platform::fs::{self, ContextPathKind, DirectoryCursor};

use crate::{ContextEntry, ContextObservation, dockerfile};

#[derive(Clone, Debug)]
pub struct CollectorLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_path_bytes: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
}
impl Default for CollectorLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_depth: 64,
            max_path_bytes: 4_096,
            max_file_bytes: 16 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum CollectorError {
    Io(std::io::Error),
    InvalidPath(String),
    Limit(&'static str),
    SpecialFile(String),
    Mutation,
    Dockerfile(dockerfile::DockerfileError),
    Duplicate(String),
}
impl std::fmt::Display for CollectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "context I/O: {e}"),
            Self::InvalidPath(x) => write!(f, "invalid context path {x:?}"),
            Self::Limit(x) => write!(f, "context collector limit exceeded: {x}"),
            Self::SpecialFile(x) => write!(
                f,
                "selected context path is not a regular file, directory, or link: {x:?}"
            ),
            Self::Mutation => f.write_str("context root changed while it was collected"),
            Self::Dockerfile(e) => e.fmt(f),
            Self::Duplicate(x) => write!(f, "duplicate context path {x:?}"),
        }
    }
}
impl std::error::Error for CollectorError {}
impl From<std::io::Error> for CollectorError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<dockerfile::DockerfileError> for CollectorError {
    fn from(e: dockerfile::DockerfileError) -> Self {
        Self::Dockerfile(e)
    }
}

/// Collect `dockerfile`'s selected files rooted at the explicit materialization
/// directory. `provenance` is checked later against the manifest but is not
/// derived from workspace mounts or the daemon current directory.
pub fn collect_context(
    root: &Path,
    dockerfile: Option<&str>,
    limits: &CollectorLimits,
) -> Result<ContextObservation, CollectorError> {
    collect_context_inner(root, dockerfile, limits, None)
}
#[cfg(test)]
fn collect_context_checkpoint(
    root: &Path,
    dockerfile: Option<&str>,
    limits: &CollectorLimits,
    checkpoint: &dyn Fn(),
) -> Result<ContextObservation, CollectorError> {
    collect_context_inner(root, dockerfile, limits, Some(checkpoint))
}
fn collect_context_inner(
    root: &Path,
    dockerfile: Option<&str>,
    limits: &CollectorLimits,
    checkpoint: Option<&dyn Fn()>,
) -> Result<ContextObservation, CollectorError> {
    let root = fs::canonical_context_path(root)?;
    let initial_root_identity = root_identity(&root)?;
    let provenance = root
        .to_str()
        .ok_or_else(|| CollectorError::InvalidPath("non-UTF-8 materialization root".into()))?
        .into();
    if fs::context_path_metadata_no_follow(&root)?.kind != ContextPathKind::Directory {
        return Err(CollectorError::InvalidPath(
            "materialization root is not a directory".into(),
        ));
    }
    let Some(dockerfile) = dockerfile else {
        return Ok(ContextObservation {
            materialization_root: provenance,
            entries: Vec::new(),
        });
    };
    if !relative(dockerfile) {
        return Err(CollectorError::InvalidPath(dockerfile.into()));
    }
    let docker_path = join_relative(&root, dockerfile);
    let docker_bytes = fs::read_context_regular_file_bounded(
        &docker_path,
        limits.max_file_bytes.min(limits.max_total_bytes),
    )?
    .bytes;
    let specific = format!("{dockerfile}.dockerignore");
    let root_ignore_present = regular(&root, ".dockerignore")?;
    let ignore_name = if regular(&root, &specific)? {
        Some(specific.clone())
    } else if root_ignore_present {
        Some(".dockerignore".into())
    } else {
        None
    };
    let policy_remaining = limits
        .max_total_bytes
        .checked_sub(docker_bytes.len())
        .ok_or(CollectorError::Limit("total bytes"))?;
    let ignore_bytes = ignore_name
        .as_ref()
        .map(|p| {
            fs::read_context_regular_file_bounded(
                &join_relative(&root, p),
                limits.max_file_bytes.min(policy_remaining),
            )
            .map(|x| x.bytes)
        })
        .transpose()?;

    // First pass is names and non-following kinds only.  It is bounded before
    // retention and does not open ordinary file contents.
    let mut nodes = BTreeMap::new();
    let mut pending = vec![(root.clone(), 0_usize)];
    while let Some((dir, depth)) = pending.pop() {
        let mut cursor = DirectoryCursor::open(&dir)?;
        while let Some(entry) = cursor.next_entry()? {
            let path = entry.path().to_path_buf();
            let rel = label(
                path.strip_prefix(&root)
                    .map_err(|_| CollectorError::Mutation)?,
            )?;
            if !relative(&rel) {
                return Err(CollectorError::InvalidPath(rel));
            }
            if rel.len() > limits.max_path_bytes {
                return Err(CollectorError::Limit("path length"));
            }
            if nodes.len() >= limits.max_entries {
                return Err(CollectorError::Limit("entry count"));
            }
            if nodes
                .insert(rel.clone(), (path.clone(), entry.kind()))
                .is_some()
            {
                return Err(CollectorError::Duplicate(rel));
            }
            if entry.kind() == ContextPathKind::Directory {
                if depth.checked_add(1).ok_or(CollectorError::Limit("depth"))? > limits.max_depth {
                    return Err(CollectorError::Limit("depth"));
                }
                pending.push((path.to_path_buf(), depth + 1));
            }
        }
    }
    let mut policy = BTreeMap::new();
    policy.insert(dockerfile.to_owned(), docker_bytes.clone());
    if let (Some(name), Some(bytes)) = (&ignore_name, &ignore_bytes) {
        policy.insert(name.clone(), bytes.clone());
    }
    let mut selector = Vec::with_capacity(nodes.len() + 2);
    for (path, (_, kind)) in &nodes {
        selector.push(match kind {
            ContextPathKind::Directory => ContextEntry::Directory { path: path.clone() },
            ContextPathKind::Symlink => ContextEntry::Symlink {
                path: path.clone(),
                target: String::new(),
            },
            _ => ContextEntry::File {
                path: path.clone(),
                bytes: Vec::new(),
            },
        });
    }
    replace_bytes(&mut selector, dockerfile, docker_bytes);
    if let (Some(name), Some(bytes)) = (&ignore_name, ignore_bytes) {
        replace_bytes(&mut selector, name, bytes);
    }
    let selected = dockerfile::select_context(&selector, dockerfile)?;
    let selected: BTreeSet<_> = selected.iter().map(entry_path).collect();
    if let Some(checkpoint) = checkpoint {
        checkpoint();
    }
    if regular(&root, &specific)? != (ignore_name.as_deref() == Some(specific.as_str()))
        || regular(&root, ".dockerignore")? != root_ignore_present
    {
        return Err(CollectorError::Mutation);
    }
    let mut entries = Vec::new();
    let mut total = 0_usize;
    for (path, (disk, kind)) in nodes {
        if !selected.contains(path.as_str()) {
            continue;
        }
        if fs::context_path_metadata_no_follow(&disk)?.kind != kind {
            return Err(CollectorError::Mutation);
        }
        match kind {
            ContextPathKind::Directory => entries.push(ContextEntry::Directory { path }),
            ContextPathKind::Symlink => {
                let target = fs::read_context_link(&disk)?
                    .to_str()
                    .ok_or_else(|| CollectorError::InvalidPath("non-UTF-8 link target".into()))?
                    .into();
                entries.push(ContextEntry::Symlink { path, target });
            }
            ContextPathKind::RegularFile => {
                let metadata = fs::context_path_metadata_no_follow(&disk)?;
                let len = usize::try_from(metadata.len.unwrap_or(u64::MAX))
                    .map_err(|_| CollectorError::Limit("file size"))?;
                if len > limits.max_file_bytes {
                    return Err(CollectorError::Limit("file size"));
                }
                let remaining = limits
                    .max_total_bytes
                    .checked_sub(total)
                    .ok_or(CollectorError::Limit("total bytes"))?;
                let read_limit = limits.max_file_bytes.min(remaining);
                let observed = fs::read_context_regular_file_bounded(&disk, read_limit)?;
                let bytes = if let Some(original) = policy.get(&path) {
                    if observed.bytes != *original {
                        return Err(CollectorError::Mutation);
                    }
                    original.clone()
                } else {
                    observed.bytes
                };
                total = total
                    .checked_add(bytes.len())
                    .ok_or(CollectorError::Limit("total bytes"))?;
                if total > limits.max_total_bytes {
                    return Err(CollectorError::Limit("total bytes"));
                }
                entries.push(ContextEntry::File { path, bytes });
            }
            ContextPathKind::Other => return Err(CollectorError::SpecialFile(path)),
        }
    }
    if fs::canonical_context_path(&root)? != root || root_identity(&root)? != initial_root_identity
    {
        return Err(CollectorError::Mutation);
    }
    Ok(ContextObservation {
        materialization_root: provenance,
        entries,
    })
}

fn regular(root: &Path, path: &str) -> Result<bool, CollectorError> {
    match fs::context_path_metadata_no_follow(&join_relative(root, path)) {
        Ok(x) => Ok(x.kind == ContextPathKind::RegularFile),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn join_relative(root: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(root.to_path_buf(), |out, x| out.join(x))
}
fn relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|x| x.is_empty() || x == "." || x == "..")
        && !path.contains(['\\', ':'])
}
fn label(path: &Path) -> Result<String, CollectorError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(CollectorError::InvalidPath(path.display().to_string()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| CollectorError::InvalidPath("non-UTF-8 path component".into()))?;
        if component.contains(['\\', ':']) {
            return Err(CollectorError::InvalidPath(component.into()));
        }
        parts.push(component);
    }
    let result = parts.join("/");
    if relative(&result) {
        Ok(result)
    } else {
        Err(CollectorError::InvalidPath(result))
    }
}
#[cfg(windows)]
fn root_identity(_: &Path) -> Result<Option<fs::FileIdentity>, CollectorError> {
    Ok(None)
}
#[cfg(not(windows))]
fn root_identity(path: &Path) -> Result<Option<fs::FileIdentity>, CollectorError> {
    Ok(fs::path_identity(path)?)
}
fn entry_path(e: &ContextEntry) -> String {
    match e {
        ContextEntry::File { path, .. }
        | ContextEntry::Directory { path }
        | ContextEntry::Symlink { path, .. } => path.clone(),
    }
}
fn replace_bytes(entries: &mut [ContextEntry], path: &str, bytes: Vec<u8>) {
    if let Some(ContextEntry::File {
        bytes: existing, ..
    }) = entries.iter_mut().find(|x| entry_path(x) == path)
    {
        *existing = bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let p = root.join(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, bytes).unwrap();
    }
    fn collect(root: &Path) -> Result<ContextObservation, CollectorError> {
        collect_context(root, Some("Dockerfile"), &CollectorLimits::default())
    }
    fn paths(value: &ContextObservation) -> Vec<String> {
        value.entries.iter().map(entry_path).collect()
    }
    #[test]
    fn real_tree_selection_ignores_large_unselected_and_honors_descendant_negation() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "Dockerfile", b"FROM x\nCOPY . /x\n");
        write(root.path(), ".dockerignore", b"ignored/\n!ignored/keep\n");
        write(root.path(), "ignored/huge", &[b'x'; 128]);
        write(root.path(), "ignored/keep", b"yes");
        let limits = CollectorLimits {
            max_file_bytes: 64,
            ..CollectorLimits::default()
        };
        let got = collect_context(root.path(), Some("Dockerfile"), &limits).unwrap();
        assert!(paths(&got).contains(&"ignored/keep".into()));
        assert!(!paths(&got).contains(&"ignored/huge".into()));
    }
    #[test]
    fn dockerfile_specific_ignore_and_links_are_observed() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "Dockerfile", b"FROM x\nCOPY hit /x\n");
        write(root.path(), ".dockerignore", b"hit\n");
        write(root.path(), "Dockerfile.dockerignore", b"!hit\n");
        write(root.path(), "hit", b"x");
        #[cfg(unix)]
        std::os::unix::fs::symlink("hit", root.path().join("link")).unwrap();
        let got = collect(root.path()).unwrap();
        assert!(paths(&got).contains(&"hit".into()));
        assert!(paths(&got).contains(&"Dockerfile.dockerignore".into()));
    }
    #[test]
    fn image_only_never_traverses_or_requires_a_dockerfile() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "unrelated", b"x");
        assert!(
            collect_context(root.path(), None, &CollectorLimits::default())
                .unwrap()
                .entries
                .is_empty()
        );
    }
    #[cfg(unix)]
    #[test]
    fn selected_fifo_is_rejected_without_reading_it() {
        use std::os::unix::fs::FileTypeExt as _;
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "Dockerfile", b"FROM x\nCOPY . /x\n");
        let fifo = root.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
        assert!(matches!(collect(root.path()), Err(CollectorError::SpecialFile(x)) if x == "fifo"));
    }
    #[test]
    fn real_tree_limits_cover_entries_depth_paths_files_total_and_policy() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "Dockerfile", b"FROM x\nCOPY . /x\n");
        write(root.path(), "a/b/c", &[b'x'; 32]);
        let base = CollectorLimits::default();
        let entries = CollectorLimits {
            max_entries: 1,
            ..base.clone()
        };
        assert!(matches!(
            collect_context(root.path(), Some("Dockerfile"), &entries),
            Err(CollectorError::Limit("entry count"))
        ));
        let depth = CollectorLimits {
            max_depth: 1,
            ..base.clone()
        };
        assert!(matches!(
            collect_context(root.path(), Some("Dockerfile"), &depth),
            Err(CollectorError::Limit("depth"))
        ));
        let path = CollectorLimits {
            max_path_bytes: 2,
            ..base.clone()
        };
        assert!(matches!(
            collect_context(root.path(), Some("Dockerfile"), &path),
            Err(CollectorError::Limit("path length"))
        ));
        let file = CollectorLimits {
            max_file_bytes: 24,
            ..base.clone()
        };
        assert!(matches!(
            collect_context(root.path(), Some("Dockerfile"), &file),
            Err(CollectorError::Limit("file size"))
        ));
        let total = CollectorLimits {
            max_total_bytes: 4,
            ..base
        };
        assert!(matches!(
            collect_context(root.path(), Some("Dockerfile"), &total),
            Err(CollectorError::Io(_))
        ));
        let aggregate = tempfile::tempdir().unwrap();
        write(aggregate.path(), "Dockerfile", b"FROM x\nCOPY a b /x\n");
        write(aggregate.path(), "a", b"1234");
        write(aggregate.path(), "b", b"5678");
        let exact = CollectorLimits {
            max_total_bytes: 27,
            ..CollectorLimits::default()
        };
        assert!(collect_context(aggregate.path(), Some("Dockerfile"), &exact).is_ok());
        let short = CollectorLimits {
            max_total_bytes: 26,
            ..CollectorLimits::default()
        };
        assert!(collect_context(aggregate.path(), Some("Dockerfile"), &short).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn checkpoint_rejects_policy_and_selected_kind_mutations() {
        use std::os::unix::fs::symlink;
        let case = |mutate: &dyn Fn(&Path)| {
            let root = tempfile::tempdir().unwrap();
            write(root.path(), "Dockerfile", b"FROM x\nCOPY . /x\n");
            write(root.path(), "dir/file", b"x");
            symlink("dir/file", root.path().join("link")).unwrap();
            let result = collect_context_checkpoint(
                root.path(),
                Some("Dockerfile"),
                &CollectorLimits::default(),
                &|| mutate(root.path()),
            );
            assert!(matches!(
                result,
                Err(CollectorError::Mutation) | Err(CollectorError::Io(_))
            ));
        };
        case(&|root| write(root, "Dockerfile", b"FROM changed\nCOPY . /x\n"));
        case(&|root| write(root, "Dockerfile.dockerignore", b"*\n"));
        case(&|root| write(root, ".dockerignore", b"*\n"));
        case(&|root| {
            fs::remove_dir_all(root.join("dir")).unwrap();
            symlink("link", root.join("dir")).unwrap();
        });
        case(&|root| {
            fs::remove_file(root.join("link")).unwrap();
            write(root, "link", b"x");
        });
    }
}
