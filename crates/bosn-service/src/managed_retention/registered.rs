//! Bridge the setup label contract to the registry ownership contract (#545).
//!
//! Setup containers and manifest volumes are created with the immutable setup label set
//! (`setup-managed`, `setup-content-sha256`, `setup-container`), not the complete ownership set
//! managed retention requires, and an existing Docker object can never gain a label. Their
//! ownership proof is therefore the registry record written when Bosn created them: an object
//! enters managed retention only when its setup labels and a durable record in **this** registry
//! agree on kind, exact name and content digest. A name alone is never evidence.
//!
//! Salvaged from draft PR #546 (`RegisteredOwnership::normalize`), without its peer catalog,
//! pending-image intents or cache coordination.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bosn_core::{ResourceKind, ResourceLabels, Retention};
use bosn_registry::Resource;

/// The setup-managed marker every setup container and manifest volume carries.
pub(super) const LABEL_SETUP_MANAGED: &str = "com.zackees.bosn.setup-managed";
const SETUP_MANAGED_VALUE: &str = "v1";
const LABEL_SETUP_DIGEST: &str = "com.zackees.bosn.setup-content-sha256";
const LABEL_SETUP_NAME: &str = "com.zackees.bosn.setup-container";
/// Records read per page, and the most this pass will hold in memory.
const PAGE: usize = 256;
const MAX_RECORDS: usize = 100_000;

/// This registry's durable ownership facts, read once per pass.
pub(crate) struct RegisteredOwnership {
    owner: String,
    resources: Vec<Resource>,
    /// Resource ids and names a lease, execution session or creation intent still names.
    protected: BTreeSet<String>,
}

/// What a registry record proved about one setup-labelled object.
#[derive(Debug, PartialEq)]
pub(crate) struct Normalized {
    /// The engine labels plus the complete ownership set derived from the record.
    pub labels: BTreeMap<String, String>,
    /// Seconds since the registry last saw the object used.
    pub idle_seconds: f64,
    /// A lease, execution session or creation intent still names it.
    pub protected: bool,
}

impl RegisteredOwnership {
    /// Read this state directory's registry. `Err` is an incomplete read: the caller must then
    /// treat every setup-labelled object as unproven rather than guess.
    pub(crate) fn load(state_dir: &Path) -> Result<Self, String> {
        let registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
            .map_err(|error| format!("registry unavailable: {error}"))?;
        let owner = registry.registry_id().map_err(|error| error.to_string())?;
        let resources = read_all(|offset| registry.resources(offset, PAGE))?;
        let mut protected = BTreeSet::new();
        protected.extend(
            read_all(|offset| registry.leases(offset, PAGE))?
                .into_iter()
                .map(|lease| lease.resource_id),
        );
        protected.extend(
            read_all(|offset| registry.execution_sessions(offset, PAGE))?
                .into_iter()
                .map(|session| session.container_id),
        );
        protected.extend(
            read_all(|offset| registry.volume_creation_intents(offset, PAGE))?
                .into_iter()
                .map(|intent| intent.name),
        );
        Ok(Self {
            owner,
            resources,
            protected,
        })
    }

    /// Prove a setup-labelled object against its registry record, or `None` when it is not one.
    ///
    /// Complete or partial canonical ownership is never reinterpreted: such an object already
    /// speaks for itself. Exactly one record must match kind, name and digest.
    pub(crate) fn normalize(
        &self,
        kind: ResourceKind,
        engine_name: &str,
        labels: &BTreeMap<String, String>,
        now: f64,
    ) -> Option<Normalized> {
        if labels.contains_key(bosn_core::LABEL_REGISTRY)
            || labels.contains_key(bosn_core::LABEL_KIND)
            || labels.get(LABEL_SETUP_MANAGED).map(String::as_str) != Some(SETUP_MANAGED_VALUE)
            || labels.get(LABEL_SETUP_NAME).map(String::as_str) != Some(engine_name)
        {
            return None;
        }
        let digest = labels.get(LABEL_SETUP_DIGEST)?;
        let mut matches = self.resources.iter().filter(|resource| {
            resource.kind == kind
                && resource.name == engine_name
                && resource.generation.strip_prefix("sha256:") == Some(digest.as_str())
                && setup_record_id(kind, &resource.id)
        });
        let resource = matches.next()?;
        if matches.next().is_some() || !resource.last_used.is_finite() {
            return None;
        }
        let canonical = ResourceLabels::new(
            &self.owner,
            kind,
            &resource.stack,
            &resource.generation,
            resource.scope,
            &resource.workspace,
            &resource.created_at.to_string(),
            Some(record_retention(kind, resource, labels)),
        )
        .ok()?;
        let mut normalized = labels.clone();
        normalized.extend(
            canonical
                .to_map()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Some(Normalized {
            labels: normalized,
            idle_seconds: (now - resource.last_used).max(0.0),
            protected: self.protected.contains(&resource.id)
                || self.protected.contains(&resource.name),
        })
    }
}

/// Only the record namespaces setup ensure and manifest ensure write.
fn setup_record_id(kind: ResourceKind, id: &str) -> bool {
    match kind {
        ResourceKind::Container => {
            id.starts_with("setup-container:") || id.starts_with("manifest-container:")
        }
        ResourceKind::Volume => id.starts_with("manifest-volume:"),
        _ => false,
    }
}

/// The retention the record proves.
///
/// A volume's retention is the manifest's declaration, a human promise, and is honoured. A
/// container row is written `Pinned` unconditionally by every setup/manifest ensure as a
/// bookkeeping constant (no command lets anyone pin or unpin it), so it is not a promise; a
/// container is protected by liveness, leases and sessions instead. An explicit pin label on the
/// object itself always wins.
fn record_retention(
    kind: ResourceKind,
    resource: &Resource,
    labels: &BTreeMap<String, String>,
) -> Retention {
    if labels.get(bosn_core::LABEL_RETENTION).map(String::as_str) == Some("pinned") {
        return Retention::Pinned;
    }
    match kind {
        ResourceKind::Volume => resource.retention,
        _ => Retention::Warm,
    }
}

fn read_all<T>(
    mut page: impl FnMut(usize) -> Result<bosn_registry::Page<T>, bosn_registry::Error>,
) -> Result<Vec<T>, String> {
    let mut items = Vec::new();
    let mut offset = 0;
    loop {
        let next = page(offset).map_err(|error| error.to_string())?;
        items.extend(next.items);
        if items.len() > MAX_RECORDS {
            return Err(format!("registry holds more than {MAX_RECORDS} records"));
        }
        let Some(following) = next.next_offset else {
            return Ok(items);
        };
        offset = following;
    }
}

#[cfg(test)]
mod tests;
