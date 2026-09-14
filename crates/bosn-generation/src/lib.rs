//! Deterministic generation identity policy.
//!
//! This crate deliberately does not open paths.  The caller supplies selected,
//! typed context observations collected by the kernel filesystem facade.  That
//! keeps build materialization distinct from workspace bind roots and prevents
//! a digest from depending on the daemon's current directory.

use std::collections::BTreeSet;

use bosn_core::{Manifest, manifest::Stack};
use kernal_api::hash::Sha256Hasher;

pub mod collector;
pub mod dockerfile;
pub mod resolver;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextEntry {
    File { path: String, bytes: Vec<u8> },
    Directory { path: String },
    Symlink { path: String, target: String },
}

/// A context observation is already filtered using Docker's selected context
/// semantics.  Paths are slash-separated labels relative to materialization
/// root; absolute paths and `..` are rejected rather than silently escaping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextObservation {
    pub materialization_root: String,
    pub entries: Vec<ContextEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalImageIdentity {
    pub reference: String,
    pub platform: Option<String>,
    /// A resolver must provide an immutable resolved identity.  `None` is
    /// allowed only for an explicitly read-only coalescing key.
    pub identity: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GenerationError {
    ContextRootMismatch {
        expected: String,
        observed: String,
    },
    InvalidContextPath(String),
    DuplicateContextPath(String),
    MissingExternalIdentity {
        reference: String,
        platform: Option<String>,
    },
    InvalidExternalIdentity {
        reference: String,
        identity: String,
    },
    DuplicateExternalImage {
        reference: String,
        platform: Option<String>,
    },
    MissingExternalImage {
        reference: String,
        platform: Option<String>,
    },
    UnexpectedExternalImage {
        reference: String,
        platform: Option<String>,
    },
}
#[derive(Debug)]
pub enum StackGenerationError {
    Collector(collector::CollectorError),
    Generation(GenerationError),
    Dockerfile(dockerfile::DockerfileError),
    RootMismatch,
    BlockingTask(String),
}
impl std::fmt::Display for StackGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Collector(e) => e.fmt(f),
            Self::Generation(e) => e.fmt(f),
            Self::Dockerfile(e) => e.fmt(f),
            Self::RootMismatch => {
                f.write_str("manifest materialization root does not name the selected root")
            }
            Self::BlockingTask(e) => write!(f, "generation blocking task failed: {e}"),
        }
    }
}

/// Run bounded filesystem observation and generation hashing on the kernel
/// blocking lane. This awaits the blocking task to settlement: dropping the
/// caller is not reported as cancellation of an in-progress filesystem walk.
/// The collector's trusted-ancestor/non-atomic-tree caveats still apply.
pub async fn stack_generation_async(
    manifest: &Manifest,
    stack: &Stack,
    root: &std::path::Path,
    limits: &collector::CollectorLimits,
    observed: &[ExternalImageIdentity],
) -> Result<String, StackGenerationError> {
    let manifest = manifest.clone();
    let stack = stack.clone();
    let root = root.to_path_buf();
    let limits = limits.clone();
    let observed = observed.to_vec();
    kernal_api::async_engine::launch_blocking(move || {
        stack_generation(&manifest, &stack, &root, &limits, &observed)
    })
    .await
    .map_err(|e| StackGenerationError::BlockingTask(e.to_string()))?
}
impl std::error::Error for StackGenerationError {}
impl From<collector::CollectorError> for StackGenerationError {
    fn from(e: collector::CollectorError) -> Self {
        Self::Collector(e)
    }
}
impl From<GenerationError> for StackGenerationError {
    fn from(e: GenerationError) -> Self {
        Self::Generation(e)
    }
}
impl From<dockerfile::DockerfileError> for StackGenerationError {
    fn from(e: dockerfile::DockerfileError) -> Self {
        Self::Dockerfile(e)
    }
}
impl std::fmt::Display for GenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContextRootMismatch { expected, observed } => write!(
                f,
                "selected context was observed at {observed:?}, not materialization root {expected:?}"
            ),
            Self::InvalidContextPath(p) => write!(f, "invalid selected context path {p:?}"),
            Self::DuplicateContextPath(p) => write!(f, "duplicate selected context path {p:?}"),
            Self::MissingExternalIdentity {
                reference,
                platform,
            } => write!(
                f,
                "external image {reference:?} for platform {platform:?} has no resolved identity"
            ),
            Self::InvalidExternalIdentity {
                reference,
                identity,
            } => write!(
                f,
                "external image {reference:?} has invalid immutable identity {identity:?}"
            ),
            Self::DuplicateExternalImage {
                reference,
                platform,
            } => write!(
                f,
                "duplicate external image {reference:?} for platform {platform:?}"
            ),
            Self::MissingExternalImage {
                reference,
                platform,
            } => write!(
                f,
                "missing resolver observation for external image {reference:?} for platform {platform:?}"
            ),
            Self::UnexpectedExternalImage {
                reference,
                platform,
            } => write!(
                f,
                "unexpected resolver observation for external image {reference:?} for platform {platform:?}"
            ),
        }
    }
}
impl std::error::Error for GenerationError {}

/// Content identity.  Bind-source *contents*, tasks, and workdir are excluded;
/// env, guest/create-time fields and volume retention are included.
pub fn content_digest(
    manifest: &Manifest,
    stack: &Stack,
    context: &ContextObservation,
) -> Result<String, GenerationError> {
    if context.materialization_root != manifest.roots.materialization_root {
        return Err(GenerationError::ContextRootMismatch {
            expected: manifest.roots.materialization_root.clone(),
            observed: context.materialization_root.clone(),
        });
    }
    let mut h = Sha256Hasher::new();
    field(&mut h, b"bosn-generation-content-v1");
    stack_fields(&mut h, stack);
    // `materialization_root` is provenance checked by the caller/collector;
    // its spelling is deliberately not content identity.
    let mut entries: Vec<_> = context.entries.iter().collect();
    entries.sort_by(|a, b| entry_path(a).cmp(entry_path(b)));
    let mut seen = BTreeSet::new();
    for entry in entries {
        let path = entry_path(entry);
        if !valid_relative(path) {
            return Err(GenerationError::InvalidContextPath(path.into()));
        }
        if !seen.insert(path.to_owned()) {
            return Err(GenerationError::DuplicateContextPath(path.into()));
        }
        h.update(b"\0path-record\0");
        match entry {
            ContextEntry::File { path, bytes } => {
                field(&mut h, b"file");
                field(&mut h, path.as_bytes());
                field(&mut h, bytes);
            }
            ContextEntry::Directory { path } => {
                field(&mut h, b"dir");
                field(&mut h, path.as_bytes());
                field(&mut h, b"");
            }
            ContextEntry::Symlink { path, target } => {
                field(&mut h, b"link");
                field(&mut h, path.as_bytes());
                field(&mut h, target.as_bytes());
            }
        }
    }
    Ok(format!("sha256:{}", h.finalize()))
}

/// Combine an already-validated content identity with resolver observations.
/// Kept private so callers cannot bypass [`final_generation`].
fn resolved_generation(
    content: &str,
    images: &[ExternalImageIdentity],
) -> Result<String, GenerationError> {
    let mut h = Sha256Hasher::new();
    field(&mut h, b"bosn-generation-resolved-v1");
    field(&mut h, content.as_bytes());
    let images = canonical_images(images, true)?;
    for image in &images {
        let identity =
            image
                .identity
                .as_ref()
                .ok_or_else(|| GenerationError::MissingExternalIdentity {
                    reference: image.reference.clone(),
                    platform: image.platform.clone(),
                })?;
        field(&mut h, image.reference.as_bytes());
        field(&mut h, image.platform.as_deref().unwrap_or("").as_bytes());
        field(&mut h, identity.as_bytes());
    }
    Ok(format!("sha256:{}", h.finalize()))
}

/// Check exact, immutable resolver observations against Dockerfile-derived
/// requirements. This validates receipts only; engine inspection is still the
/// authority that Docker supplied them.
pub fn validate_resolved_images(
    required: &[ExternalImageIdentity],
    observed: &[ExternalImageIdentity],
) -> Result<Vec<ExternalImageIdentity>, GenerationError> {
    let required = canonical_required_images(required)?;
    let observed = canonical_images(observed, true)?;
    for image in &required {
        if !observed.iter().any(|x| same_image(x, image)) {
            return Err(GenerationError::MissingExternalImage {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
    }
    for image in &observed {
        if !required.iter().any(|x| same_image(x, image)) {
            return Err(GenerationError::UnexpectedExternalImage {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
    }
    Ok(observed)
}

/// The only final-generation entry point: resolver observations must cover the
/// exact Dockerfile-derived requirements before they become generation input.
pub fn final_generation(
    content: &str,
    required: &[ExternalImageIdentity],
    observed: &[ExternalImageIdentity],
) -> Result<String, GenerationError> {
    if !valid_sha256_digest(content) {
        return Err(GenerationError::InvalidExternalIdentity {
            reference: "content".into(),
            identity: content.into(),
        });
    }
    let observed = validate_resolved_images(required, observed)?;
    resolved_generation(content, &observed)
}

/// Product entry point: materialize only the selected stack root, derive its
/// Dockerfile/image requirements, then authorize one immutable generation.
pub fn stack_generation(
    manifest: &Manifest,
    stack: &Stack,
    root: &std::path::Path,
    limits: &collector::CollectorLimits,
    observed: &[ExternalImageIdentity],
) -> Result<String, StackGenerationError> {
    let expected = kernal_api::platform::fs::canonical_context_path(std::path::Path::new(
        &manifest.roots.materialization_root,
    ))
    .map_err(collector::CollectorError::from)?;
    let actual = kernal_api::platform::fs::canonical_context_path(root)
        .map_err(collector::CollectorError::from)?;
    if expected != actual {
        return Err(StackGenerationError::RootMismatch);
    }
    let context = collector::collect_context(root, stack.dockerfile.as_deref(), limits)?;
    stack_generation_from_context(manifest, stack, &context, observed)
}

/// Derive one generation from a single already-collected context observation.
///
/// This is the companion to [`stack_generation`], for a caller which must
/// materialize the exact same selected bytes after authorizing their
/// generation.  It keeps collection and materialization from observing two
/// different workspace snapshots while retaining the same external-image
/// policy as the ordinary entry point.
pub fn stack_generation_from_context(
    manifest: &Manifest,
    stack: &Stack,
    context: &ContextObservation,
    observed: &[ExternalImageIdentity],
) -> Result<String, StackGenerationError> {
    let mut normalized_manifest = manifest.clone();
    normalized_manifest.roots.materialization_root = context.materialization_root.clone();
    let content = content_digest(&normalized_manifest, stack, context)?;
    let required = if let Some(dockerfile) = &stack.dockerfile {
        let bytes = context
            .entries
            .iter()
            .find_map(|x| match x {
                ContextEntry::File { path, bytes } if path == dockerfile => Some(bytes),
                _ => None,
            })
            .ok_or_else(|| GenerationError::InvalidContextPath(dockerfile.clone()))?;
        dockerfile::external_images(std::str::from_utf8(bytes).map_err(|_| {
            StackGenerationError::Dockerfile(dockerfile::DockerfileError::InvalidUtf8(
                dockerfile.clone(),
            ))
        })?)?
    } else {
        stack
            .image
            .as_ref()
            .map(|reference| {
                vec![ExternalImageIdentity {
                    reference: reference.clone(),
                    platform: None,
                    identity: None,
                }]
            })
            .unwrap_or_default()
    };
    Ok(final_generation(&content, &required, observed)?)
}

fn same_image(a: &ExternalImageIdentity, b: &ExternalImageIdentity) -> bool {
    a.reference == b.reference && a.platform == b.platform
}

fn canonical_images(
    images: &[ExternalImageIdentity],
    require_identity: bool,
) -> Result<Vec<ExternalImageIdentity>, GenerationError> {
    let mut images = images.to_vec();
    images.sort_by(|a, b| (&a.reference, &a.platform).cmp(&(&b.reference, &b.platform)));
    for pair in images.windows(2) {
        if same_image(&pair[0], &pair[1]) {
            return Err(GenerationError::DuplicateExternalImage {
                reference: pair[0].reference.clone(),
                platform: pair[0].platform.clone(),
            });
        }
    }
    for image in &images {
        if image.reference.trim().is_empty() {
            return Err(GenerationError::MissingExternalImage {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
        match &image.identity {
            Some(identity) if valid_sha256_digest(identity) => {}
            Some(identity) => {
                return Err(GenerationError::InvalidExternalIdentity {
                    reference: image.reference.clone(),
                    identity: identity.clone(),
                });
            }
            None if require_identity => {
                return Err(GenerationError::MissingExternalIdentity {
                    reference: image.reference.clone(),
                    platform: image.platform.clone(),
                });
            }
            None => {}
        }
    }
    Ok(images)
}

fn canonical_required_images(
    images: &[ExternalImageIdentity],
) -> Result<Vec<ExternalImageIdentity>, GenerationError> {
    let mut images = images.to_vec();
    images.sort_by(|a, b| (&a.reference, &a.platform).cmp(&(&b.reference, &b.platform)));
    for image in &images {
        if image.reference.trim().is_empty() {
            return Err(GenerationError::MissingExternalImage {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
        if let Some(identity) = &image.identity
            && !valid_sha256_digest(identity)
        {
            return Err(GenerationError::InvalidExternalIdentity {
                reference: image.reference.clone(),
                identity: identity.clone(),
            });
        }
    }
    images.dedup_by(|a, b| same_image(a, b));
    Ok(images)
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read-only coalescing key: unresolved images have a deliberate sentinel,
/// never an accidental empty resolved identity.
pub fn coalescing_generation(content: &str, images: &[ExternalImageIdentity]) -> String {
    let mut h = Sha256Hasher::new();
    field(&mut h, b"bosn-generation-coalescing-v1");
    field(&mut h, content.as_bytes());
    let mut images = images.to_vec();
    images.sort_by(|a, b| {
        (&a.reference, &a.platform, &a.identity).cmp(&(&b.reference, &b.platform, &b.identity))
    });
    for image in &images {
        field(&mut h, image.reference.as_bytes());
        field(&mut h, image.platform.as_deref().unwrap_or("").as_bytes());
        field(
            &mut h,
            if image.identity.is_some() {
                b"identity-present"
            } else {
                b"identity-missing"
            },
        );
        field(
            &mut h,
            image
                .identity
                .as_deref()
                .unwrap_or("<unresolved>")
                .as_bytes(),
        );
    }
    format!("sha256:{}", h.finalize())
}

fn stack_fields(h: &mut Sha256Hasher, s: &Stack) {
    field(h, b"stack-v1");
    field(h, s.name.as_bytes());
    field(h, s.dockerfile.as_deref().unwrap_or("").as_bytes());
    field(h, s.image.as_deref().unwrap_or("").as_bytes());
    field(h, s.family.as_deref().unwrap_or("").as_bytes());
    let mut volumes = s.volumes.clone();
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    count(h, b"volumes", volumes.len());
    for v in &volumes {
        let destination = v
            .destination
            .clone()
            .unwrap_or_else(|| format!("/bosn/{}", v.name));
        field(h, v.name.as_bytes());
        field(h, v.scope.as_str().as_bytes());
        field(h, destination.as_bytes());
        field(h, v.retention.as_str().as_bytes());
    }
    let mut mounts = s.mounts.clone();
    mounts.sort_by(|a, b| {
        (&a.name, &a.source, &a.destination, a.readonly).cmp(&(
            &b.name,
            &b.source,
            &b.destination,
            b.readonly,
        ))
    });
    count(h, b"mounts", mounts.len());
    for m in &mounts {
        field(h, m.name.as_bytes());
        field(h, m.source.as_bytes());
        field(h, m.destination.as_bytes());
        field(h, if m.readonly { b"1" } else { b"0" });
    }
    let mut tmpfs = s.tmpfs.clone();
    tmpfs.sort_by(|a, b| a.value.cmp(&b.value));
    count(h, b"tmpfs", tmpfs.len());
    for t in &tmpfs {
        field(h, t.value.as_bytes());
    }
    count(h, b"env", s.env.len());
    for (k, v) in &s.env {
        field(h, k.as_bytes());
        field(h, v.as_bytes());
    }
    field(h, s.kind.as_deref().unwrap_or("").as_bytes());
    field(
        h,
        if s.acknowledge_macos_license {
            b"1"
        } else {
            b"0"
        },
    );
    if let Some(g) = &s.guest {
        field(h, b"guest-present");
        for v in [
            g.ssh_port.to_string(),
            g.ssh_user.clone(),
            g.ssh_host.clone(),
            g.web_port.to_string(),
            g.ready_timeout.to_string(),
            g.ready_poll_interval.to_string(),
            g.version.clone(),
            g.ram_size.clone(),
            g.disk_size.clone(),
            g.cpu_cores.map(|x| x.to_string()).unwrap_or_default(),
            g.payload.clone().unwrap_or_default(),
            g.payload_destination.clone(),
        ] {
            field(h, v.as_bytes());
        }
    } else {
        field(h, b"guest-absent");
    }
}
fn count(h: &mut Sha256Hasher, tag: &[u8], count: usize) {
    field(h, tag);
    h.update((count as u64).to_be_bytes());
}
fn field(h: &mut Sha256Hasher, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}
fn entry_path(e: &ContextEntry) -> &str {
    match e {
        ContextEntry::File { path, .. }
        | ContextEntry::Directory { path }
        | ContextEntry::Symlink { path, .. } => path,
    }
}
fn valid_relative(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.split('/').any(|x| x.is_empty() || x == "." || x == "..")
        && !p.contains('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_records_are_ordered_and_typed() {
        let a = ContextObservation {
            materialization_root: "build".into(),
            entries: vec![
                ContextEntry::Directory {
                    path: "empty".into(),
                },
                ContextEntry::Symlink {
                    path: "link".into(),
                    target: "a".into(),
                },
                ContextEntry::File {
                    path: "a".into(),
                    bytes: b"x".to_vec(),
                },
            ],
        };
        let mut b = a.clone();
        b.entries.reverse();
        assert_eq!(
            content_digest(&manifest(), manifest().stack("s").unwrap(), &a),
            content_digest(&manifest(), manifest().stack("s").unwrap(), &b)
        );
    }
    #[test]
    fn external_platform_is_identity_significant() {
        let a = final_generation(
            &format!("sha256:{}", "c".repeat(64)),
            &[ExternalImageIdentity {
                reference: "alpine".into(),
                platform: Some("linux/amd64".into()),
                identity: None,
            }],
            &[ExternalImageIdentity {
                reference: "alpine".into(),
                platform: Some("linux/amd64".into()),
                identity: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        )
        .unwrap();
        let b = final_generation(
            &format!("sha256:{}", "c".repeat(64)),
            &[ExternalImageIdentity {
                reference: "alpine".into(),
                platform: Some("linux/arm64".into()),
                identity: None,
            }],
            &[ExternalImageIdentity {
                reference: "alpine".into(),
                platform: Some("linux/arm64".into()),
                identity: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        )
        .unwrap();
        assert_ne!(a, b);
    }
    #[test]
    fn resolver_requires_exact_complete_unique_immutable_receipts() {
        let required = vec![ExternalImageIdentity {
            reference: "base".into(),
            platform: Some("linux/amd64".into()),
            identity: None,
        }];
        let valid = ExternalImageIdentity {
            reference: "base".into(),
            platform: Some("linux/amd64".into()),
            identity: Some(format!("sha256:{}", "b".repeat(64))),
        };
        assert_eq!(
            validate_resolved_images(&required, std::slice::from_ref(&valid)).unwrap(),
            vec![valid.clone()]
        );
        assert!(matches!(
            validate_resolved_images(&required, &[]),
            Err(GenerationError::MissingExternalImage { .. })
        ));
        assert!(matches!(
            validate_resolved_images(
                &required,
                &[ExternalImageIdentity {
                    identity: Some("sha256:short".into()),
                    ..valid.clone()
                }]
            ),
            Err(GenerationError::InvalidExternalIdentity { .. })
        ));
        assert!(matches!(
            validate_resolved_images(&required, &[valid.clone(), valid]),
            Err(GenerationError::DuplicateExternalImage { .. })
        ));
    }
    #[test]
    fn stack_entry_point_binds_real_root_and_requires_immutable_image_receipt() {
        let root = tempfile::tempdir().unwrap();
        let root_name = root.path().to_str().unwrap();
        let manifest = bosn_core::parse_manifest_toml(
            "[stack.s]\nimage='alpine'",
            bosn_core::ManifestRoots::new("m", root_name, "workspace"),
        )
        .unwrap();
        let stack = manifest.stack("s").unwrap();
        let limits = collector::CollectorLimits::default();
        let observed = ExternalImageIdentity {
            reference: "alpine".into(),
            platform: None,
            identity: Some(format!("sha256:{}", "a".repeat(64))),
        };
        assert!(
            stack_generation(
                &manifest,
                stack,
                root.path(),
                &limits,
                std::slice::from_ref(&observed)
            )
            .is_ok()
        );
        assert!(stack_generation(&manifest, stack, root.path(), &limits, &[]).is_err());
        assert!(
            stack_generation(
                &manifest,
                stack,
                root.path(),
                &limits,
                &[observed.clone(), observed]
            )
            .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn stack_entry_normalizes_a_manifest_symlink_root_before_digesting() {
        let root = tempfile::tempdir().unwrap();
        let holder = tempfile::tempdir().unwrap();
        let link = holder.path().join("materialized");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        let manifest = bosn_core::parse_manifest_toml(
            "[stack.s]\nimage='alpine'",
            bosn_core::ManifestRoots::new("m", link.to_str().unwrap(), "workspace"),
        )
        .unwrap();
        let observed = ExternalImageIdentity {
            reference: "alpine".into(),
            platform: None,
            identity: Some(format!("sha256:{}", "a".repeat(64))),
        };
        assert!(
            stack_generation(
                &manifest,
                manifest.stack("s").unwrap(),
                root.path(),
                &collector::CollectorLimits::default(),
                &[observed]
            )
            .is_ok()
        );
    }
    #[test]
    fn stack_generation_real_roots_selected_and_resolver_receipts() {
        fn setup() -> (tempfile::TempDir, Manifest) {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(
                root.path().join("Dockerfile"),
                "FROM busybox\nCOPY keep /x\n",
            )
            .unwrap();
            std::fs::write(root.path().join("keep"), "a").unwrap();
            std::fs::write(root.path().join("skip"), "z").unwrap();
            let manifest = bosn_core::parse_manifest_toml(
                "[stack.s]\ndockerfile='Dockerfile'",
                bosn_core::ManifestRoots::new("m", root.path().to_str().unwrap(), "workspace"),
            )
            .unwrap();
            (root, manifest)
        }
        let (one, first) = setup();
        let (two, second) = setup();
        let receipt = ExternalImageIdentity {
            reference: "busybox".into(),
            platform: None,
            identity: Some(format!("sha256:{}", "b".repeat(64))),
        };
        let limits = collector::CollectorLimits::default();
        let a = stack_generation(
            &first,
            first.stack("s").unwrap(),
            one.path(),
            &limits,
            std::slice::from_ref(&receipt),
        )
        .unwrap();
        let b = stack_generation(
            &second,
            second.stack("s").unwrap(),
            two.path(),
            &limits,
            std::slice::from_ref(&receipt),
        )
        .unwrap();
        assert_eq!(a, b);
        std::fs::write(one.path().join("skip"), "changed").unwrap();
        assert_eq!(
            a,
            stack_generation(
                &first,
                first.stack("s").unwrap(),
                one.path(),
                &limits,
                std::slice::from_ref(&receipt)
            )
            .unwrap()
        );
        assert!(matches!(
            stack_generation(&first, first.stack("s").unwrap(), one.path(), &limits, &[]),
            Err(StackGenerationError::Generation(
                GenerationError::MissingExternalImage { .. }
            ))
        ));
        assert!(matches!(
            stack_generation(
                &first,
                first.stack("s").unwrap(),
                one.path(),
                &limits,
                &[ExternalImageIdentity {
                    reference: "busybox".into(),
                    platform: None,
                    identity: Some("sha256:x".into())
                }]
            ),
            Err(StackGenerationError::Generation(
                GenerationError::InvalidExternalIdentity { .. }
            ))
        ));
        std::fs::write(one.path().join("keep"), "b").unwrap();
        assert_ne!(
            a,
            stack_generation(
                &first,
                first.stack("s").unwrap(),
                one.path(),
                &limits,
                &[receipt]
            )
            .unwrap()
        );
    }
    #[test]
    fn stack_generation_create_time_fields_roll_but_workdir_and_tasks_do_not() {
        let root = tempfile::tempdir().unwrap();
        let root_name = root.path().to_str().unwrap();
        let manifest = bosn_core::parse_manifest_toml("[stack.s]\nimage='alpine'\nworkdir='/one'\n[stack.s.env]\nA='one'\n[stack.s.volumes.v]\n[task.t]\nstack='s'\ncmd='one'", bosn_core::ManifestRoots::new("m", root_name, "workspace-one")).unwrap();
        let receipt = ExternalImageIdentity {
            reference: "alpine".into(),
            platform: None,
            identity: Some(format!("sha256:{}", "a".repeat(64))),
        };
        let limits = collector::CollectorLimits::default();
        let base = stack_generation(
            &manifest,
            manifest.stack("s").unwrap(),
            root.path(),
            &limits,
            std::slice::from_ref(&receipt),
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("bind"), "one").unwrap();
        let material = tempfile::tempdir().unwrap();
        std::fs::write(
            material.path().join("Dockerfile"),
            "FROM alpine\nCOPY keep /x\n",
        )
        .unwrap();
        std::fs::write(material.path().join("keep"), "x").unwrap();
        let bound = bosn_core::parse_manifest_toml(&format!("[stack.s]\ndockerfile='Dockerfile'\n[stack.s.mounts.bind]\nsource='{}'\ndestination='/bind'", workspace.path().display()), bosn_core::ManifestRoots::new("m", material.path().to_str().unwrap(), workspace.path().to_str().unwrap())).unwrap();
        let alpine = ExternalImageIdentity {
            reference: "alpine".into(),
            platform: None,
            identity: Some(format!("sha256:{}", "a".repeat(64))),
        };
        let before_bind = stack_generation(
            &bound,
            bound.stack("s").unwrap(),
            material.path(),
            &limits,
            std::slice::from_ref(&alpine),
        )
        .unwrap();
        std::fs::write(workspace.path().join("bind"), "two").unwrap();
        assert_eq!(
            before_bind,
            stack_generation(
                &bound,
                bound.stack("s").unwrap(),
                material.path(),
                &limits,
                &[alpine]
            )
            .unwrap()
        );
        let mut task = manifest.clone();
        task.tasks.get_mut("t").unwrap().cmd = "two".into();
        assert_eq!(
            base,
            stack_generation(
                &task,
                task.stack("s").unwrap(),
                root.path(),
                &limits,
                std::slice::from_ref(&receipt)
            )
            .unwrap()
        );
        let mut workdir = manifest.stack("s").unwrap().clone();
        workdir.workdir = Some("/two".into());
        assert_eq!(
            base,
            stack_generation(
                &manifest,
                &workdir,
                root.path(),
                &limits,
                std::slice::from_ref(&receipt)
            )
            .unwrap()
        );
        let mut env = manifest.stack("s").unwrap().clone();
        env.env.insert("A".into(), "two".into());
        assert_ne!(
            base,
            stack_generation(
                &manifest,
                &env,
                root.path(),
                &limits,
                std::slice::from_ref(&receipt)
            )
            .unwrap()
        );
        let mut retention = manifest.stack("s").unwrap().clone();
        retention.volumes[0].retention = bosn_core::Retention::Pinned;
        assert_ne!(
            base,
            stack_generation(
                &manifest,
                &retention,
                root.path(),
                &limits,
                std::slice::from_ref(&receipt)
            )
            .unwrap()
        );
        let mut guest = manifest.stack("s").unwrap().clone();
        guest.guest = Some(bosn_core::manifest::Guest {
            ssh_port: 22,
            ssh_user: "u".into(),
            ssh_host: "h".into(),
            web_port: 80,
            ready_timeout: 1,
            ready_poll_interval: 1,
            version: "v".into(),
            ram_size: "1G".into(),
            disk_size: "1G".into(),
            cpu_cores: None,
            payload: None,
            payload_destination: "/x".into(),
        });
        assert_ne!(
            base,
            stack_generation(
                &manifest,
                &guest,
                root.path(),
                &limits,
                std::slice::from_ref(&receipt)
            )
            .unwrap()
        );
    }
    #[test]
    fn unresolved_is_only_valid_for_read_only_coalescing() {
        let image = ExternalImageIdentity {
            reference: "mutable".into(),
            platform: None,
            identity: None,
        };
        assert!(
            final_generation(
                &format!("sha256:{}", "c".repeat(64)),
                std::slice::from_ref(&image),
                std::slice::from_ref(&image)
            )
            .is_err()
        );
        assert_ne!(
            coalescing_generation("sha256:c", &[image]),
            coalescing_generation("sha256:c", &[])
        );
    }
    #[test]
    fn docker_references_include_multistage_and_copy_from() {
        let refs = dockerfile::external_images(
            "FROM --platform=linux/amd64 base AS build\nFROM build\nCOPY --from=busybox /x /x\n",
        )
        .unwrap();
        assert_eq!(
            refs.iter()
                .map(|x| x.reference.as_str())
                .collect::<Vec<_>>(),
            ["base", "busybox"]
        );
    }
    fn manifest() -> Manifest {
        bosn_core::parse_manifest_toml(
            "[stack.s]\nimage='x'\n[stack.s.env]\nA='b'",
            bosn_core::ManifestRoots::new("m", "build", "workspace"),
        )
        .unwrap()
    }
}
