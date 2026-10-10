//! The read-only probe that decides whether a *missing* registry database
//! really means first run (#515).
//!
//! A missing `registry.sqlite3` has two causes that look identical to
//! `NotFound`: a genuinely clean machine, and the destruction of the record of
//! what this machine already owns. The second case is what turned 182 Docker
//! volumes into `ForeignRegistry` on 2026-10-05: a purge moved the database
//! aside, the daemon restarted, minted a fresh UUID, and every object it had
//! ever created became permanently unreachable by every GC path.
//!
//! Only the engine can tell the two apart. Before a new identity is minted,
//! [`serve`](super::Service::serve) asks [`RegistryIdentityProbe`] what a
//! previous registry left behind. With no database there is no "our" id, so
//! any object already carrying Bosn's registry label is by definition foreign
//! and the caller must decide explicitly rather than by default.
//!
//! Nothing here mutates engine state and nothing here decides to delete; see
//! the non-goal in `AGENTS.md` and invariants 1 and 2 of #149.

use super::*;
use bosn_engine::CensusRead;
use serde::Deserialize;

//
// A missing `registry.sqlite3` has two causes that look identical to
// `NotFound`: a genuinely clean machine, and the destruction of the record of
// what this machine already owns. The second case is what turned 182 Docker
// volumes into `ForeignRegistry` on 2026-10-05: a purge moved the database
// aside, the daemon restarted, minted a fresh UUID, and every object it had
// ever created became permanently unreachable by every GC path.
//
// Only the engine can tell the two apart. Before a new identity is minted we
// look for Docker objects already carrying Bosn's registry label. With no
// database there is no "our" id, so any such object is by definition foreign.

/// The engine-facing kinds a prior-identity probe inspects, in a fixed order so
/// two reads of one machine produce the same report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorObjectKind {
    Container,
    Volume,
    Image,
}

impl PriorObjectKind {
    const ALL: [Self; 3] = [Self::Container, Self::Volume, Self::Image];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::Image => "image",
        }
    }
}

/// One Docker object already labelled by some Bosn registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorObject {
    pub kind: PriorObjectKind,
    /// Engine-visible name: a container or volume name, or an image's first
    /// repository tag, falling back to its id.
    pub name: String,
    /// The `com.zackees.bosn.registry` value this object carries.
    pub registry_id: String,
    /// Size in bytes, or `None` when the engine reported nothing parseable.
    pub bytes: Option<i128>,
}

impl PriorObject {
    /// One report line naming the object, its size, and the registry claiming
    /// it. An operator reading a refusal must be able to identify every object
    /// without running a second command.
    #[must_use]
    pub fn report_line(&self) -> String {
        let size = match self.bytes {
            Some(bytes) => unmanaged::human_bytes(bytes.max(0)),
            None => "size unknown".to_owned(),
        };
        format!(
            "  {} {} ({size}, registry {})",
            self.kind.as_str(),
            self.name,
            self.registry_id
        )
    }
}

/// The outcome of one bounded prior-identity read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PriorIdentity {
    /// Objects carrying a Bosn registry label, sorted by kind then name.
    pub objects: Vec<PriorObject>,
    /// Reads the engine refused or answered unreadably. A non-empty list means
    /// the machine is *not known* to be clean, which is not the same as clean.
    pub unreadable: Vec<String>,
}

impl PriorIdentity {
    /// Whether this machine is provably free of anything a previous Bosn
    /// registry created. Only a complete read that found nothing qualifies; an
    /// unreadable read must never be mistaken for an empty one.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.objects.is_empty() && self.unreadable.is_empty()
    }

    /// Whether anything at all was proven to carry a registry label.
    #[must_use]
    pub fn has_prior_objects(&self) -> bool {
        !self.objects.is_empty()
    }
}

/// Read-only evidence about what a previous Bosn registry left behind.
///
/// This is a seam, not an engine-command injection point: an implementation
/// may only report objects it read, may never mutate engine state, and may
/// never report "clean" from a read it could not complete.
pub trait RegistryIdentityProbe: Send + Sync + std::fmt::Debug {
    fn probe(&self) -> PriorIdentity;
}

/// The production probe: bounded, read-only `docker` label and inspect reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct DockerIdentityProbe;

/// A probe that finds nothing, for daemons that must not touch the engine.
///
/// Production always reads the engine. A unit-test daemon running against a
/// shared Docker daemon would otherwise inherit whatever objects the developer's
/// own machine happens to carry, so tests get the empty read unless they ask
/// for another one with [`Service::with_identity_probe`] — the same treatment
/// `docker_proxy` already gets in unit tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyIdentityProbe;

impl RegistryIdentityProbe for EmptyIdentityProbe {
    fn probe(&self) -> PriorIdentity {
        PriorIdentity::default()
    }
}

pub(crate) fn default_identity_probe() -> Arc<dyn RegistryIdentityProbe> {
    if cfg!(test) {
        Arc::new(EmptyIdentityProbe)
    } else {
        Arc::new(DockerIdentityProbe)
    }
}

impl RegistryIdentityProbe for DockerIdentityProbe {
    fn probe(&self) -> PriorIdentity {
        let engine = DockerEngine::docker();
        let options = RunOptions::bounded(
            unmanaged::CENSUS_READ_DEADLINE,
            unmanaged::CENSUS_READ_OUTPUT_LIMIT,
        );
        let mut prior = PriorIdentity::default();
        for kind in PriorObjectKind::ALL {
            observe_kind(&engine, kind, options, &mut prior);
        }
        prior
            .objects
            .sort_by(|a, b| (a.kind.as_str(), &a.name).cmp(&(b.kind.as_str(), &b.name)));
        prior
    }
}

/// Docker renders `Labels` as `null` on an unlabelled object, not `{}`.
fn null_as_empty_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<BTreeMap<String, String>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Parsed eagerly into typed structs at the boundary; never probed as untyped
/// JSON. Each mirrors the shape its `docker inspect` variant emits.
#[derive(Deserialize)]
struct ContainerIdentity {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "SizeRw", default)]
    size_rw: Option<i128>,
    #[serde(rename = "Config", default)]
    config: Option<LabelledConfig>,
}

#[derive(Deserialize)]
struct ImageIdentity {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "RepoTags", default)]
    repo_tags: Vec<String>,
    #[serde(rename = "Size", default)]
    size: Option<i128>,
    #[serde(rename = "Config", default)]
    config: Option<LabelledConfig>,
}

#[derive(Deserialize)]
struct LabelledConfig {
    #[serde(rename = "Labels", default, deserialize_with = "null_as_empty_map")]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct VolumeIdentity {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Labels", default, deserialize_with = "null_as_empty_map")]
    labels: BTreeMap<String, String>,
    #[serde(rename = "UsageData", default)]
    usage: Option<VolumeUsage>,
}

#[derive(Deserialize)]
struct VolumeUsage {
    #[serde(rename = "Size", default)]
    size: Option<i128>,
}

/// Read one kind: list everything carrying the registry label, then inspect
/// exactly those ids. Every failure is recorded in `unreadable` rather than
/// collapsing into "no objects".
fn observe_kind(
    engine: &DockerEngine,
    kind: PriorObjectKind,
    options: RunOptions,
    prior: &mut PriorIdentity,
) {
    let key = bosn_core::LABEL_REGISTRY;
    let listed = match kind {
        PriorObjectKind::Container => engine
            .container_ids_with_label(key, options)
            .map_err(|error| format!("docker ps --filter label failed: {error}")),
        PriorObjectKind::Volume => engine
            .volume_names_with_label(key, options)
            .map_err(|error| format!("docker volume ls --filter label failed: {error}")),
        PriorObjectKind::Image => engine
            .image_ids_with_label(key, options)
            .map_err(|error| format!("docker image ls --filter label failed: {error}")),
    };
    let ids = match listed.as_ref().map_or_else(
        |detail| Err(detail.clone()),
        |read| match read {
            CensusRead::Document(text) => Ok(text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<String>>()),
            CensusRead::Unavailable { detail } => Err(detail.clone()),
        },
    ) {
        Ok(ids) => ids,
        Err(detail) => {
            prior.unreadable.push(detail);
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    let inspected = match kind {
        PriorObjectKind::Container => engine.inspect_containers(&ids, options),
        PriorObjectKind::Volume => engine.inspect_volumes(&ids, options),
        PriorObjectKind::Image => engine.inspect_images(&ids, options),
    };
    let document = match inspected {
        Ok(CensusRead::Document(text)) => text,
        Ok(CensusRead::Unavailable { detail }) => {
            prior.unreadable.push(detail);
            return;
        }
        Err(error) => {
            prior
                .unreadable
                .push(format!("{} inspect failed: {error}", kind.as_str()));
            return;
        }
    };
    parse_kind(prior, kind, &document, key);
}

/// Fold one kind's inspect document into the prior identity. Split out from the
/// engine reads so the typed parsing is testable without a Docker daemon.
pub(crate) fn parse_kind(
    prior: &mut PriorIdentity,
    kind: PriorObjectKind,
    document: &str,
    key: &str,
) {
    match kind {
        PriorObjectKind::Container => record(
            prior,
            document,
            "docker inspect --type container",
            |row: ContainerIdentity| PriorObject {
                kind,
                name: pick_name(row.name, row.id),
                registry_id: label_value(row.config, key),
                bytes: row.size_rw,
            },
        ),
        PriorObjectKind::Volume => {
            record(
                prior,
                document,
                "docker volume inspect",
                |row: VolumeIdentity| PriorObject {
                    kind,
                    name: pick_name(row.name, String::new()),
                    registry_id: if is_shared_ci_cache(&row.labels) {
                        SHARED_CI_CACHE.to_owned()
                    } else {
                        label_value_from(row.labels, key)
                    },
                    bytes: row.usage.and_then(|usage| usage.size),
                },
            );
            // The machine-wide CI cache volume (#544) is shared by every state directory on
            // the host, not owned by the registry that happened to create it. Counting it made
            // every fresh state directory on a machine that had ever run CI refuse to start
            // (#545 live check). Only that exact volume, by name and machine scope, is skipped.
            prior
                .objects
                .retain(|object| object.registry_id != SHARED_CI_CACHE);
        }
        PriorObjectKind::Image => record(
            prior,
            document,
            "docker image inspect",
            |row: ImageIdentity| {
                let tag = row
                    .repo_tags
                    .iter()
                    .find(|tag| tag.as_str() != "<none>:<none>")
                    .cloned()
                    .unwrap_or_default();
                PriorObject {
                    kind,
                    name: pick_name(tag, row.id),
                    registry_id: label_value(row.config, key),
                    bytes: row.size,
                }
            },
        ),
    }
}

/// Sentinel for the skipped shared CI cache volume; never a registry id (not a UUID).
const SHARED_CI_CACHE: &str = "<machine-wide CI cache>";

fn is_shared_ci_cache(labels: &BTreeMap<String, String>) -> bool {
    labels.get(bosn_core::LABEL_SCOPE).map(String::as_str) == Some("machine")
        && labels.get(bosn_core::LABEL_STACK).map(String::as_str) == Some("ci-cache")
}

/// Deserialize one inspect document and fold its rows into the prior identity.
/// A document this build cannot read is recorded as unreadable, never as empty.
fn record<T, F>(prior: &mut PriorIdentity, document: &str, what: &str, into: F)
where
    T: for<'de> Deserialize<'de>,
    F: Fn(T) -> PriorObject,
{
    match serde_json::from_str::<Vec<T>>(document) {
        Ok(rows) => prior.objects.extend(rows.into_iter().map(into)),
        Err(error) => prior
            .unreadable
            .push(format!("{what} is unreadable: {error}")),
    }
}

/// The registry id an object carries. A label that is absent or empty is
/// reported as such rather than omitted: an object the engine labelled with
/// something unreadable is evidence, not absence of evidence.
fn label_value(config: Option<LabelledConfig>, key: &str) -> String {
    label_value_from(config.map(|config| config.labels).unwrap_or_default(), key)
}

fn label_value_from(labels: BTreeMap<String, String>, key: &str) -> String {
    match labels.get(key) {
        Some(value) if !value.is_empty() => value.clone(),
        _ => "<no readable registry label>".to_owned(),
    }
}

/// Prefer the engine's human name, falling back to an identifier so an object
/// is never reported as a nameless blank line.
fn pick_name(name: String, id: String) -> String {
    let name = name.trim_start_matches('/').to_owned();
    if !name.is_empty() {
        return name;
    }
    if id.is_empty() {
        "<unnamed>".to_owned()
    } else {
        id
    }
}

/// The refusal message for a missing registry whose objects outlived it.
#[must_use]
pub fn prior_registry_refusal(prior: &PriorIdentity, db: &Path) -> String {
    let mut message = format!(
        "bosn: refusing to mint a new registry identity. {} is missing, but {} Docker object(s) \
         already carry a Bosn registry label, so this machine is not a clean first run. Minting a \
         new id would make all of them unreachable by every GC path.\n",
        db.display(),
        prior.objects.len()
    );
    for object in &prior.objects {
        message.push_str(&object.report_line());
        message.push('\n');
    }
    message.push_str(
        "Restore the registry database from backup (it is the only record of what this machine \
         owns), or remove the objects above deliberately, then start bosn again.",
    );
    message
}
