//! Bridge the immutable setup label contract to the registry ownership contract.
//!
//! Existing Docker objects cannot gain labels. A matching durable registry record,
//! including the generation digest, is therefore required before a legacy object
//! can participate in managed retention. A name alone never proves ownership.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bosn_core::{ResourceKind, ResourceLabels, Retention};
use bosn_registry::{ReadOnlyRegistry, Resource};

mod intents;
mod pending_images;
mod uses;

pub(crate) struct RegisteredOwnership {
    pub(super) owner: String,
    resources: Vec<Resource>,
    pending: Vec<intents::PendingVolume>,
    pending_images: Vec<(bosn_registry::ImageCreationIntent, bool)>,
    protected: BTreeSet<String>,
    shared: BTreeMap<(ResourceKind, String), SharedUse>,
}

#[derive(Default)]
struct SharedUse {
    latest: f64,
    pinned: bool,
    protected: bool,
}

impl RegisteredOwnership {
    pub(crate) fn load(state_dir: &Path) -> Result<Self, String> {
        let mut ownership = Self::load_local(state_dir)?;
        ownership.merge_peers(super::peers::read_ownership(state_dir)?);
        Ok(ownership)
    }

    pub(super) fn load_local(state_dir: &Path) -> Result<Self, String> {
        super::budget::check()?;
        let registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
            .map_err(|error| format!("retention registry unavailable: {error}"))?;
        let owner = registry.registry_id().map_err(|error| error.to_string())?;
        let mut resources = Vec::new();
        let mut offset = 0;
        loop {
            super::budget::check()?;
            let page = registry
                .resources(offset, 64)
                .map_err(|error| error.to_string())?;
            resources.extend(page.items);
            check_record_count(resources.len())?;
            let Some(next) = page.next_offset else { break };
            offset = next;
        }
        let protected = protections(&registry)?;
        uses::apply(&registry, &mut resources)?;
        let pending = intents::load(&registry, &resources)?;
        let pending_images: Vec<_> = registry
            .image_creation_intents()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|intent| (intent, false))
            .collect();
        check_record_count(
            resources
                .len()
                .saturating_add(protected.len())
                .saturating_add(pending.len())
                .saturating_add(pending_images.len()),
        )?;
        Ok(Self {
            owner,
            resources,
            pending,
            pending_images,
            protected,
            shared: BTreeMap::new(),
        })
    }

    fn merge_peers(&mut self, peers: Vec<Self>) {
        for peer in peers {
            for resource in &peer.resources {
                let key = if resource.kind == ResourceKind::Image {
                    &resource.generation
                } else {
                    &resource.name
                };
                let shared = self.shared.entry((resource.kind, key.clone())).or_default();
                shared.pinned |= resource.retention == Retention::Pinned;
                shared.protected |=
                    peer.protected_name(&resource.name) || !resource.last_used.is_finite();
                shared.latest = shared.latest.max(resource.last_used);
            }
            self.pending.extend(peer.pending);
            self.pending_images.extend(peer.pending_images);
        }
    }

    pub(super) fn protect_all(&mut self) {
        for (_, protected) in &mut self.pending_images {
            *protected = true;
        }
        self.protected
            .extend(self.resources.iter().map(|resource| resource.id.clone()));
    }

    pub(super) fn records_object(&self, kind: ResourceKind, name: &str) -> bool {
        self.resources.iter().any(|resource| {
            resource.kind == kind
                && if kind == ResourceKind::Image {
                    resource.generation == name
                } else {
                    resource.name == name
                }
        })
    }

    pub(super) fn record_count(&self) -> usize {
        self.resources
            .len()
            .saturating_add(self.protected.len())
            .saturating_add(self.pending.len())
            .saturating_add(self.pending_images.len())
    }

    fn retention_and_use(&self, resource: &Resource) -> (Retention, f64) {
        let key = if resource.kind == ResourceKind::Image {
            &resource.generation
        } else {
            &resource.name
        };
        match self.shared.get(&(resource.kind, key.clone())) {
            Some(shared) => (
                if shared.pinned {
                    Retention::Pinned
                } else {
                    resource.retention
                },
                resource.last_used.max(shared.latest),
            ),
            None => (resource.retention, resource.last_used),
        }
    }

    pub(super) fn idle_manifests(&self, now: f64, ttl: f64) -> Vec<String> {
        let mut eligible: Vec<_> = self
            .resources
            .iter()
            .filter(|resource| {
                let (retention, last_used) = self.retention_and_use(resource);
                resource.kind == ResourceKind::Container
                    && resource.id.starts_with("manifest-container:")
                    && retention != Retention::Pinned
                    && !self.protected.contains(&resource.id)
                    && !self.protected.contains(&resource.name)
                    && !self.protected_name(&resource.name)
                    && last_used.is_finite()
                    && now - last_used >= ttl
            })
            .map(|resource| (self.retention_and_use(resource).1, resource.name.clone()))
            .collect();
        eligible.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        eligible.into_iter().map(|(_, name)| name).collect()
    }

    pub(super) fn protected_name(&self, name: &str) -> bool {
        self.protected.contains(name)
            || self
                .shared
                .iter()
                .any(|((_, key), usage)| key == name && usage.protected)
            || self
                .resources
                .iter()
                .any(|resource| resource.name == name && self.protected.contains(&resource.id))
    }

    pub(super) fn apply_usage(
        &self,
        kind: ResourceKind,
        key: &str,
        labels: &mut BTreeMap<String, String>,
        age: &mut Option<f64>,
        in_use: &mut bool,
    ) {
        *in_use |= self.protected_name(key);
        if kind == ResourceKind::Image {
            *in_use |= self.pending_images.iter().any(|(intent, protected)| {
                *protected
                    && match intent.source {
                        bosn_registry::ImageIntentSource::Build => {
                            intent.ownership_proof().ok().is_some_and(|proof| {
                                labels.get(bosn_setup::IMAGE_INTENT_LABEL) == Some(&proof)
                            })
                        }
                        bosn_registry::ImageIntentSource::Pull => pending_images::protects_pull(
                            &super::DockerEngine::docker(),
                            intent,
                            key,
                        ),
                    }
            });
        }
        if kind == ResourceKind::Volume {
            *in_use |= self
                .pending
                .iter()
                .any(|pending| pending.protects(key, labels));
        }
        let mut latest = 0.0_f64;
        let mut pinned = false;
        for resource in self.resources.iter().filter(|resource| {
            resource.kind == kind
                && if kind == ResourceKind::Image {
                    resource.generation == key
                } else {
                    resource.name == key
                }
        }) {
            let (retention, last_used) = self.retention_and_use(resource);
            pinned |= retention == Retention::Pinned;
            *in_use |= self.protected_name(&resource.name) || !resource.last_used.is_finite();
            latest = latest.max(last_used);
        }
        if let Some(shared) = self.shared.get(&(kind, key.to_owned())) {
            latest = latest.max(shared.latest);
            pinned |= shared.pinned;
            *in_use |= shared.protected;
        }
        if pinned {
            labels.insert(bosn_core::LABEL_RETENTION.into(), "pinned".into());
        }
        *age = age.map(|value| value.min((super::now_seconds() - latest).max(0.0)));
    }

    pub(super) fn image_ids(&self) -> BTreeSet<String> {
        self.resources
            .iter()
            .filter(|resource| {
                resource.kind == ResourceKind::Image
                    && (resource.id == format!("setup-image:{}", resource.generation)
                        || resource.id == format!("manifest-image:{}", resource.generation))
            })
            .map(|resource| resource.generation.clone())
            .collect()
    }

    pub(super) fn normalize_image(
        &self,
        image_id: &str,
        labels: &BTreeMap<String, String>,
    ) -> Option<(BTreeMap<String, String>, f64, bool)> {
        if labels.contains_key(bosn_core::LABEL_REGISTRY)
            || labels.contains_key(bosn_core::LABEL_KIND)
        {
            return None;
        }
        let matches: Vec<_> = self
            .resources
            .iter()
            .filter(|resource| {
                resource.kind == ResourceKind::Image
                    && resource.generation == image_id
                    && (resource.id == format!("setup-image:{image_id}")
                        || resource.id == format!("manifest-image:{image_id}"))
            })
            .collect();
        let resource = *matches.first()?;
        if matches.iter().any(|row| !row.last_used.is_finite()) {
            return None;
        }
        let shared = self.shared.get(&(ResourceKind::Image, image_id.to_owned()));
        let pinned = labels.get(bosn_core::LABEL_RETENTION).map(String::as_str) == Some("pinned")
            || matches.iter().any(|row| row.retention == Retention::Pinned)
            || shared.is_some_and(|usage| usage.pinned);
        let last_used = matches
            .iter()
            .map(|row| row.last_used)
            .filter(|value| value.is_finite())
            .reduce(f64::max)?
            .max(shared.map_or(0.0, |usage| usage.latest));
        let protected = matches.iter().any(|row| self.protected_name(&row.name))
            || shared.is_some_and(|usage| usage.protected);
        let canonical = ResourceLabels::new(
            &self.owner,
            ResourceKind::Image,
            &resource.stack,
            image_id,
            resource.scope,
            &resource.workspace,
            &resource.created_at.to_string(),
            Some(if pinned {
                Retention::Pinned
            } else {
                resource.retention
            }),
        )
        .ok()?;
        let mut normalized = labels.clone();
        normalized.extend(
            canonical
                .to_map()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Some((normalized, last_used, protected))
    }

    pub(super) fn receipt_labels(&self, name: &str, mut labels: ResourceLabels) -> ResourceLabels {
        if labels.registry == self.owner
            && let Some(row) = self.resources.iter().find(|row| {
                row.kind == labels.kind
                    && row.stack == labels.stack
                    && row.generation == labels.generation
                    && row.scope == labels.scope
                    && row.workspace == labels.workspace
                    && (row.name == name || row.kind == ResourceKind::Image)
            })
        {
            // Docker's creation label predates the ownership checkpoint; capture
            // the registry incarnation independently before removing the object.
            labels.created = row.created_at.to_string();
        }
        labels
    }

    pub(crate) fn normalize(
        &self,
        kind: ResourceKind,
        engine_name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Option<(BTreeMap<String, String>, f64)> {
        // Never reinterpret complete or partial canonical ownership as legacy.
        if labels.contains_key(bosn_core::LABEL_REGISTRY)
            || labels.contains_key(bosn_core::LABEL_KIND)
            || labels
                .get("com.zackees.bosn.setup-managed")
                .map(String::as_str)
                != Some("v1")
        {
            return None;
        }
        let name = labels.get("com.zackees.bosn.setup-container")?;
        if name != engine_name {
            return None;
        }
        let digest = labels.get("com.zackees.bosn.setup-content-sha256")?;
        let mut matches = self.resources.iter().filter(|resource| {
            resource.kind == kind
                && resource.name == *name
                && resource.generation.strip_prefix("sha256:") == Some(digest.as_str())
                && match kind {
                    ResourceKind::Container => {
                        resource.id.starts_with("setup-container:")
                            || resource.id.starts_with("manifest-container:")
                    }
                    ResourceKind::Volume => resource.id.starts_with("manifest-volume:"),
                    _ => false,
                }
        });
        let resource = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let (retention, last_used) = self.retention_and_use(resource);
        let canonical = ResourceLabels::new(
            &self.owner,
            kind,
            &resource.stack,
            &resource.generation,
            resource.scope,
            &resource.workspace,
            &resource.created_at.to_string(),
            Some(
                if labels.get(bosn_core::LABEL_RETENTION).map(String::as_str) == Some("pinned") {
                    Retention::Pinned
                } else {
                    retention
                },
            ),
        )
        .ok()?;
        let mut normalized = labels.clone();
        normalized.extend(
            canonical
                .to_map()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Some((normalized, last_used))
    }
}

fn protections(registry: &ReadOnlyRegistry) -> Result<BTreeSet<String>, String> {
    let mut protected = BTreeSet::new();
    let mut offset = 0;
    loop {
        super::budget::check()?;
        let page = registry
            .leases(offset, 64)
            .map_err(|error| error.to_string())?;
        protected.extend(page.items.into_iter().map(|lease| lease.resource_id));
        check_record_count(protected.len())?;
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    offset = 0;
    loop {
        super::budget::check()?;
        let page = registry
            .execution_sessions(offset, 64)
            .map_err(|error| error.to_string())?;
        protected.extend(page.items.into_iter().map(|session| session.container_id));
        check_record_count(protected.len())?;
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    Ok(protected)
}

pub(super) fn check_record_count(count: usize) -> Result<(), String> {
    if count > 65_536 {
        Err("retention ownership inventory exceeded 65536 records; reconcile registry history before retrying".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_inventory_refuses_overflow_instead_of_truncating_protection() {
        assert!(check_record_count(65_536).is_ok());
        assert!(check_record_count(65_537).is_err());
        assert!(check_record_count(usize::MAX).is_err());
    }

    fn fixture() -> (RegisteredOwnership, BTreeMap<String, String>) {
        let resource = Resource {
            id: "manifest-volume:volume".into(),
            kind: ResourceKind::Volume,
            name: "volume".into(),
            stack: "test".into(),
            generation: format!("sha256:{}", "a".repeat(64)),
            scope: bosn_core::Scope::Stack,
            workspace: "/workspace".into(),
            created_at: 10.0,
            last_used: 20.0,
            state: bosn_core::ResourceState::Active,
            retention: Retention::Warm,
        };
        let labels = BTreeMap::from([
            ("com.zackees.bosn.setup-managed".into(), "v1".into()),
            (
                "com.zackees.bosn.setup-container".into(),
                resource.name.clone(),
            ),
            (
                "com.zackees.bosn.setup-content-sha256".into(),
                "a".repeat(64),
            ),
        ]);
        (
            RegisteredOwnership {
                owner: "11111111-2222-4333-8444-555555555555".into(),
                resources: vec![resource],
                pending: Vec::new(),
                pending_images: Vec::new(),
                protected: BTreeSet::new(),
                shared: BTreeMap::new(),
            },
            labels,
        )
    }

    #[test]
    fn opted_out_peer_preparation_protects_shared_image_before_ownership_checkpoint() {
        let (mut current, _) = fixture();
        let (mut peer, _) = fixture();
        peer.resources.clear();
        let intent = bosn_registry::ImageCreationIntent {
            reference: format!("bosn-setup:{}", "a".repeat(64)),
            content_sha256: "a".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: bosn_registry::ImageIntentOwner::Setup,
            source: bosn_registry::ImageIntentSource::Build,
            created_at: 1.0,
        };
        let proof = intent.ownership_proof().unwrap();
        peer.pending_images.push((intent, false));
        peer.protect_all();
        assert_eq!(peer.record_count(), 1);
        current.merge_peers(vec![peer]);
        for matches in [false, true] {
            let mut labels = BTreeMap::from([(
                bosn_setup::IMAGE_INTENT_LABEL.into(),
                if matches {
                    proof.clone()
                } else {
                    "unrelated".into()
                },
            )]);
            let mut in_use = false;
            current.apply_usage(
                ResourceKind::Image,
                "sha256:image",
                &mut labels,
                &mut Some(100.0),
                &mut in_use,
            );
            assert_eq!(in_use, matches);
        }
    }

    #[test]
    fn abandoned_volume_intent_requires_matching_row_and_actual_labels() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let (ownership, labels) = fixture();
        let resource = ownership.resources[0].clone();
        let mut registry = bosn_registry::Registry::create_writer(
            temporary.path().join("registry.sqlite3"),
            &ownership.owner,
        )
        .unwrap();
        let intent = bosn_registry::VolumeCreationIntent {
            name: resource.name.clone(),
            labels: labels.clone(),
            stack: resource.stack.clone(),
            generation: resource.generation.clone(),
            scope: resource.scope,
            workspace: resource.workspace.clone(),
        };
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.put_volume_creation_intent(&intent).unwrap();
        transaction.commit().unwrap();
        let read =
            bosn_registry::Registry::open_read_only(temporary.path().join("registry.sqlite3"))
                .unwrap();
        let pending = intents::load(&read, &[]).unwrap();
        assert!(pending[0].protects(&resource.name, &labels));
        let pending = intents::load(&read, std::slice::from_ref(&resource)).unwrap();
        assert!(!pending[0].protects(&resource.name, &labels));
        let mut wrong_labels = labels.clone();
        wrong_labels.insert(
            "com.zackees.bosn.setup-content-sha256".into(),
            "different".into(),
        );
        assert!(pending[0].protects(&resource.name, &wrong_labels));
        let mut wrong_row = resource.clone();
        wrong_row.workspace = "/different/workspace".into();
        assert!(intents::load(&read, &[wrong_row]).unwrap()[0].protects(&resource.name, &labels));
        let mut peer = ownership;
        peer.pending = pending;
        let (mut current, _) = fixture();
        current.merge_peers(vec![peer]);
        let mut age = Some(1000.0);
        let mut in_use = false;
        current.apply_usage(
            ResourceKind::Volume,
            &resource.name,
            &mut wrong_labels,
            &mut age,
            &mut in_use,
        );
        assert!(in_use, "unverified peer intent must remain protective");
    }

    #[test]
    fn legacy_ownership_requires_the_exact_engine_name_and_registered_digest() {
        let (ownership, mut labels) = fixture();
        assert!(
            ownership
                .normalize(ResourceKind::Volume, "different", &labels)
                .is_none()
        );
        assert!(
            ownership
                .normalize(ResourceKind::Container, "volume", &labels)
                .is_none()
        );
        labels.insert(
            "com.zackees.bosn.setup-content-sha256".into(),
            "b".repeat(64),
        );
        assert!(
            ownership
                .normalize(ResourceKind::Volume, "volume", &labels)
                .is_none()
        );
    }

    #[test]
    fn an_unregistered_or_ambiguous_object_cannot_be_reclaimed() {
        let (mut ownership, labels) = fixture();
        ownership.resources.push(ownership.resources[0].clone());
        assert!(
            ownership
                .normalize(ResourceKind::Volume, "volume", &labels)
                .is_none()
        );
        ownership.resources.clear();
        assert!(
            ownership
                .normalize(ResourceKind::Volume, "volume", &labels)
                .is_none()
        );
    }

    #[test]
    fn a_protected_resource_cannot_gain_legacy_reclamation_authority() {
        let (mut ownership, labels) = fixture();
        ownership.protected.insert("volume".into());
        assert!(ownership.protected_name("volume"));
        assert!(
            ownership
                .normalize(ResourceKind::Volume, "volume", &labels)
                .is_some()
        );
    }

    #[test]
    fn legacy_mapping_never_overwrites_canonical_ownership() {
        let (ownership, mut labels) = fixture();
        labels.insert(bosn_core::LABEL_REGISTRY.into(), "foreign".into());
        assert!(
            ownership
                .normalize(ResourceKind::Volume, "volume", &labels)
                .is_none()
        );
    }

    #[test]
    fn legacy_mapping_preserves_an_explicit_engine_pin() {
        let (ownership, mut labels) = fixture();
        labels.insert(bosn_core::LABEL_RETENTION.into(), "pinned".into());
        let (normalized, _) = ownership
            .normalize(ResourceKind::Volume, "volume", &labels)
            .expect("verified legacy resource");
        assert_eq!(
            normalized.get(bosn_core::LABEL_RETENTION).unwrap(),
            "pinned"
        );
    }

    #[test]
    fn image_proof_requires_exact_digest_and_preserves_shared_pins_and_activity() {
        let (mut ownership, _) = fixture();
        let image = &mut ownership.resources[0];
        image.kind = ResourceKind::Image;
        image.id = format!("manifest-image:{}", image.generation);
        image.name = image.id.clone();
        let digest = image.generation.clone();
        let mut second = image.clone();
        assert!(
            ownership
                .normalize_image(&digest, &BTreeMap::new())
                .is_some()
        );
        assert!(
            ownership
                .normalize_image(&format!("sha256:{}", "b".repeat(64)), &BTreeMap::new())
                .is_none()
        );
        second.id = format!("setup-image:{digest}");
        second.name = second.id.clone();
        second.retention = Retention::Pinned;
        second.last_used = 100.0;
        ownership.protected.insert(second.id.clone());
        ownership.resources.push(second);
        let (labels, last_used, protected) = ownership
            .normalize_image(&digest, &BTreeMap::new())
            .unwrap();
        assert_eq!(labels.get(bosn_core::LABEL_RETENTION).unwrap(), "pinned");
        assert_eq!(last_used, 100.0);
        assert!(protected);
        let foreign = BTreeMap::from([(bosn_core::LABEL_REGISTRY.into(), "foreign".into())]);
        assert!(ownership.normalize_image(&digest, &foreign).is_none());
        ownership.resources[1].last_used = f64::NAN;
        assert!(
            ownership
                .normalize_image(&digest, &BTreeMap::new())
                .is_none()
        );
    }

    #[test]
    fn another_registry_pin_or_lease_protects_a_shared_volume() {
        let (mut ownership, labels) = fixture();
        let (mut peer, _) = fixture();
        peer.resources[0].retention = Retention::Pinned;
        peer.resources[0].last_used = 100.0;
        peer.protected.insert(peer.resources[0].id.clone());
        ownership.merge_peers(vec![peer]);
        let (normalized, last_used) = ownership
            .normalize(ResourceKind::Volume, "volume", &labels)
            .unwrap();
        assert_eq!(
            normalized.get(bosn_core::LABEL_RETENTION).unwrap(),
            "pinned"
        );
        assert_eq!(last_used, 100.0);
        assert!(ownership.protected_name("volume"));
    }

    #[test]
    fn keepalive_retirement_prioritizes_oldest_shared_use_and_preserves_pins() {
        let (mut ownership, _) = fixture();
        let resource = &mut ownership.resources[0];
        resource.kind = ResourceKind::Container;
        resource.id = "manifest-container:app:recent".into();
        resource.name = "recent".into();
        resource.last_used = 30.0;
        let mut oldest = resource.clone();
        oldest.id = "manifest-container:app:oldest".into();
        oldest.name = "oldest".into();
        oldest.last_used = 10.0;
        let mut pinned = oldest.clone();
        pinned.id = "manifest-container:app:pinned".into();
        pinned.name = "pinned".into();
        pinned.last_used = 0.0;
        pinned.retention = Retention::Pinned;
        ownership.resources.extend([oldest, pinned]);
        assert_eq!(ownership.idle_manifests(100.0, 50.0), ["oldest", "recent"]);
    }

    #[test]
    fn recent_use_in_another_registry_prevents_shared_keepalive_retirement() {
        let (mut ownership, _) = fixture();
        ownership.resources[0].kind = ResourceKind::Container;
        ownership.resources[0].id = "manifest-container:app:volume".into();
        assert_eq!(ownership.idle_manifests(100.0, 50.0), ["volume"]);
        let (mut peer, _) = fixture();
        peer.resources[0].kind = ResourceKind::Container;
        peer.resources[0].last_used = 80.0;
        ownership.merge_peers(vec![peer]);
        assert!(ownership.idle_manifests(100.0, 50.0).is_empty());
    }
}
