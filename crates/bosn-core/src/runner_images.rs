//! Which `runs-on` labels `bosn ci run` (act2) may run, and on what image.
//!
//! Plain bosn stacks and tasks run any image or Dockerfile; only the act2
//! path is gated, because a job's runner is chosen by the workflow and bosn
//! must still prove what it ran on. The gate is this table: each label maps
//! to a runner image pinned by manifest digest, or to a Dockerfile that bosn
//! builds inside the engine and records by the sha256 of its bytes. A label
//! with no entry is `unsupported`.
//!
//! The built-in table keeps `ubuntu-latest`, `ubuntu-24.04` and
//! `ubuntu-22.04` on the pinned catthehacker 24.04 image and adds
//! `ubuntu-26.04`, built from [`UBUNTU_2604_DOCKERFILE`]. A repository adds
//! labels in [`DECLARATION_PATH`], read from the run's own source snapshot,
//! so the declaration is versioned with the workflows and is part of the
//! tree digest the run records:
//!
//! ```toml
//! [runners."my-runner"]
//! image = "ghcr.io/org/runner@sha256:<64 hex>"
//!
//! [runners."ubuntu-26.04-cuda"]
//! dockerfile = ".github/runners/cuda.Dockerfile"   # built with no context
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// The default runner image every built-in `ubuntu-*` label up to 24.04
/// maps to: the linux/amd64 manifest of `catthehacker/ubuntu:act-24.04`.
pub const DEFAULT_RUNNER_IMAGE: &str = "docker.io/catthehacker/ubuntu@sha256:4f2d5083a9d10d018c1c511eb8665cd480553c11975e78fd903a46daa830768b";

/// The labels on [`DEFAULT_RUNNER_IMAGE`], in their historical order (the
/// first is the one a gated matrix job runs on).
pub const DEFAULT_RUNNER_LABELS: [&str; 3] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"];

/// The built-in `ubuntu-26.04` runner's Dockerfile.
pub const UBUNTU_2604_DOCKERFILE: &str = include_str!("runner_images/ubuntu-26.04.Dockerfile");

/// Where a repository declares extra runners, relative to its root.
pub const DECLARATION_PATH: &str = ".github/bosn-runners.toml";

/// Largest Dockerfile bosn accepts for a runner.
pub const MAX_DOCKERFILE_BYTES: usize = 256 * 1024;

/// Where a label's image comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerSource {
    /// A registry image pinned by manifest digest (`name@sha256:<64 hex>`).
    Pinned { reference: String },
    /// A Dockerfile built inside the engine with no build context, keyed by
    /// the sha256 of its bytes.
    Dockerfile { origin: String, contents: String },
}

impl RunnerSource {
    /// The digest that names this image: the manifest digest of a pinned
    /// image, or `sha256:` of a Dockerfile's bytes.
    pub fn digest(&self) -> String {
        match self {
            Self::Pinned { reference } => reference
                .rsplit_once('@')
                .map_or_else(String::new, |(_, d)| d.to_owned()),
            Self::Dockerfile { contents, .. } => {
                format!("sha256:{:x}", Sha256::digest(contents.as_bytes()))
            }
        }
    }

    /// The engine-local tag act's `-P` mapping names. The default image keeps
    /// its historical `bosn/act-runner:<12 hex>` tag.
    pub fn tag(&self) -> String {
        let digest = self.digest();
        let short = &digest.trim_start_matches("sha256:")[..12];
        match self {
            Self::Pinned { .. } => format!("bosn/act-runner:{short}"),
            Self::Dockerfile { .. } => format!("bosn/act-runner-build:{short}"),
        }
    }

    /// Whether this is the image `act serve prepare` already loads and proves.
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Pinned { reference } if reference == DEFAULT_RUNNER_IMAGE)
    }
}

/// The label → runner image table for one run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerTable {
    entries: BTreeMap<String, RunnerSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    #[serde(default)]
    runners: BTreeMap<String, DeclaredRunner>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredRunner {
    image: Option<String>,
    dockerfile: Option<String>,
}

impl Default for RunnerTable {
    fn default() -> Self {
        Self::builtin()
    }
}

impl RunnerTable {
    /// The built-in table: today's three labels on the pinned 24.04 image,
    /// plus `ubuntu-26.04` built from its pinned Dockerfile.
    pub fn builtin() -> Self {
        let mut entries = BTreeMap::new();
        for label in DEFAULT_RUNNER_LABELS {
            entries.insert(
                label.to_owned(),
                RunnerSource::Pinned {
                    reference: DEFAULT_RUNNER_IMAGE.to_owned(),
                },
            );
        }
        entries.insert(
            "ubuntu-26.04".to_owned(),
            RunnerSource::Dockerfile {
                origin: "built-in".to_owned(),
                contents: UBUNTU_2604_DOCKERFILE.to_owned(),
            },
        );
        Self { entries }
    }

    /// The built-in table plus a repository's declaration. `read` returns a
    /// repository file's text by its path relative to the root (`None` when
    /// it does not exist).
    pub fn with_declaration(
        text: &str,
        read: impl Fn(&str) -> Result<Option<String>, String>,
    ) -> Result<Self, String> {
        let refuse = |why: String| format!("{DECLARATION_PATH}: {why}");
        let declaration: Declaration = toml::from_str(text).map_err(|e| refuse(e.to_string()))?;
        let mut table = Self::builtin();
        for (label, runner) in declaration.runners {
            if !valid_label(&label) {
                return Err(refuse(format!(
                    "runner label {label:?} is not a valid label"
                )));
            }
            if table.entries.contains_key(&label) {
                return Err(refuse(format!(
                    "runner {label:?} is built in; declare a new label instead"
                )));
            }
            let source = match (runner.image, runner.dockerfile) {
                (Some(reference), None) => {
                    if !pinned_reference(&reference) {
                        return Err(refuse(format!(
                            "runner {label:?}: image must be pinned by digest (name@sha256:<64 hex>), got {reference:?}"
                        )));
                    }
                    RunnerSource::Pinned { reference }
                }
                (None, Some(path)) => {
                    if !relative_path(&path) {
                        return Err(refuse(format!(
                            "runner {label:?}: dockerfile must be a relative path inside the repository, got {path:?}"
                        )));
                    }
                    let contents = read(&path)?.ok_or_else(|| {
                        refuse(format!(
                            "runner {label:?}: dockerfile {path} does not exist"
                        ))
                    })?;
                    if contents.len() > MAX_DOCKERFILE_BYTES {
                        return Err(refuse(format!(
                            "runner {label:?}: dockerfile {path} is over {MAX_DOCKERFILE_BYTES} bytes"
                        )));
                    }
                    RunnerSource::Dockerfile {
                        origin: path,
                        contents,
                    }
                }
                _ => {
                    return Err(refuse(format!(
                        "runner {label:?} needs exactly one of `image` or `dockerfile`"
                    )));
                }
            };
            table.entries.insert(label, source);
        }
        Ok(table)
    }

    /// The image a label runs on; `None` means the label is unsupported.
    pub fn resolve(&self, label: &str) -> Option<&RunnerSource> {
        self.entries.get(label)
    }

    /// Every allowed label, sorted.
    pub fn labels(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Every entry, sorted by label.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &RunnerSource)> {
        self.entries.iter().map(|(l, s)| (l.as_str(), s))
    }
}

/// What a job on an unallowed label is told: the label and how to allow it.
pub fn unsupported_message(label: &str) -> String {
    format!(
        "runs-on {label} has no bosn runner image; allow it in {DECLARATION_PATH} with \
         [runners.\"{label}\"] and either image = \"<name>@sha256:<digest>\" or \
         dockerfile = \"<path in the repository>\""
    )
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 128
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn pinned_reference(reference: &str) -> bool {
    let Some((name, digest)) = reference.rsplit_once('@') else {
        return false;
    };
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    !name.is_empty()
        && !name.contains(char::is_whitespace)
        && hex.len() == 64
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|part| !part.is_empty() && part != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn no_files(_: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    #[test]
    fn the_default_table_is_unchanged() {
        let table = RunnerTable::builtin();
        for label in DEFAULT_RUNNER_LABELS {
            let source = table.resolve(label).unwrap();
            assert!(source.is_default(), "{label}");
            assert_eq!(source.tag(), "bosn/act-runner:4f2d5083a9d1");
        }
        assert_eq!(
            table.labels(),
            [
                "ubuntu-22.04",
                "ubuntu-24.04",
                "ubuntu-26.04",
                "ubuntu-latest"
            ]
        );
    }

    #[test]
    fn ubuntu_2604_is_built_from_a_pinned_dockerfile() {
        let table = RunnerTable::builtin();
        let RunnerSource::Dockerfile { contents, .. } = table.resolve("ubuntu-26.04").unwrap()
        else {
            panic!("ubuntu-26.04 is a Dockerfile runner");
        };
        let from = contents.lines().find(|l| l.starts_with("FROM ")).unwrap();
        assert!(
            from.starts_with("FROM docker.io/library/ubuntu@sha256:"),
            "{from}"
        );
        for needed in [
            "git",
            "zstd",
            "sudo",
            "curl",
            "python3",
            "NODE_SHA256",
            "--snapshot",
        ] {
            assert!(contents.contains(needed), "{needed}");
        }
        let tag = table.resolve("ubuntu-26.04").unwrap().tag();
        assert!(tag.starts_with("bosn/act-runner-build:"), "{tag}");
    }

    #[test]
    fn a_declared_label_is_accepted() {
        let text = format!(
            "[runners.\"gpu-box\"]\nimage = \"ghcr.io/o/r@{DIGEST}\"\n\
             [runners.\"custom\"]\ndockerfile = \".github/runners/c.Dockerfile\"\n"
        );
        let table = RunnerTable::with_declaration(&text, |path| {
            assert_eq!(path, ".github/runners/c.Dockerfile");
            Ok(Some("FROM scratch\n".into()))
        })
        .unwrap();
        assert_eq!(
            table.resolve("gpu-box"),
            Some(&RunnerSource::Pinned {
                reference: format!("ghcr.io/o/r@{DIGEST}")
            })
        );
        let custom = table.resolve("custom").unwrap();
        assert_eq!(
            custom.digest(),
            format!("sha256:{:x}", Sha256::digest(b"FROM scratch\n"))
        );
        assert!(table.resolve("ubuntu-latest").unwrap().is_default());
    }

    #[test]
    fn an_unknown_label_is_still_unsupported() {
        let table = RunnerTable::with_declaration("", no_files).unwrap();
        for label in [
            "windows-2022",
            "macos-15",
            "ubuntu-20.04",
            "ubuntu-26.04-arm",
        ] {
            assert_eq!(table.resolve(label), None, "{label}");
        }
    }

    #[test]
    fn declarations_keep_pinning() {
        for text in [
            "[runners.x]\nimage = \"ubuntu:26.04\"\n",
            "[runners.x]\nimage = \"ubuntu@sha256:abc\"\n",
            "[runners.x]\ndockerfile = \"../outside\"\n",
            "[runners.x]\ndockerfile = \"/etc/passwd\"\n",
            "[runners.x]\ndockerfile = \"missing.Dockerfile\"\n",
            "[runners.x]\n",
            "[runners.\"ubuntu-latest\"]\nimage = \"a@sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n",
            "[runners.\"bad label\"]\nimage = \"a@sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n",
            "[runners.x]\nimage = \"a@sha256:1111111111111111111111111111111111111111111111111111111111111111\"\nextra = 1\n",
        ] {
            let error = RunnerTable::with_declaration(text, no_files).unwrap_err();
            assert!(error.starts_with(DECLARATION_PATH), "{text}: {error}");
        }
    }

    #[test]
    fn the_unsupported_message_names_the_label_and_the_remedy() {
        let message = unsupported_message("ubuntu-26.04-arm");
        assert!(message.contains("runs-on ubuntu-26.04-arm"));
        assert!(message.contains(DECLARATION_PATH));
        assert!(message.contains("[runners.\"ubuntu-26.04-arm\"]"));
        assert!(message.contains("@sha256:"));
        assert!(message.contains("dockerfile"));
    }
}
