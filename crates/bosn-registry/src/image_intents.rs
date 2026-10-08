//! Bounded durable preparation intents, stored in v5's existing metadata table.

use super::*;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "image-preparation:";
const MAX_INTENTS: usize = 1024;
const MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCreationIntent {
    pub reference: String,
    pub content_sha256: String,
    pub workspace: String,
    pub stack: String,
    pub owner: ImageIntentOwner,
    pub source: ImageIntentSource,
    pub created_at: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ImageIntentOwner {
    Setup,
    Manifest,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ImageIntentSource {
    Build,
    Pull,
}

impl ImageCreationIntent {
    fn validate(&self) -> Result<(), Error> {
        let hash = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let reference_valid = match self.source {
            ImageIntentSource::Build => {
                self.reference == format!("bosn-setup:{}", self.content_sha256)
            }
            ImageIntentSource::Pull => self
                .reference
                .rsplit_once("@sha256:")
                .is_some_and(|(repository, digest)| !repository.is_empty() && hash(digest)),
        };
        if !hash(&self.content_sha256)
            || !reference_valid
            || self.reference.len() > 2048
            || self
                .reference
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            || self.workspace.is_empty()
            || self.stack.is_empty()
            || !self.created_at.is_finite()
        {
            return Err(Error::BadRow("image preparation intent"));
        }
        Ok(())
    }

    /// Stable proof carried by an image built under this exact preparation identity.
    /// The caller must also verify the reference, metadata, registry protection,
    /// and current engine liveness; this digest alone never authorizes removal.
    pub fn ownership_proof(&self) -> Result<String, Error> {
        self.validate()?;
        self.key().map(|key| key[PREFIX.len()..].to_owned())
    }

    fn key(&self) -> Result<String, Error> {
        let identity =
            serde_json::to_vec(&(&self.reference, &self.workspace, &self.stack, self.owner))
                .map_err(|_| Error::BadRow("image preparation identity"))?;
        Ok(format!(
            "{PREFIX}{}",
            kernal_api::hash::sha256_bytes(&identity).to_hex()
        ))
    }
}

/// Engine evidence validated against one durable preparation intent.
/// Construction must use a fresh inspection of the intent's exact reference.
pub struct VerifiedImageExport {
    intent: ImageCreationIntent,
    image_id: String,
    registry_id: String,
}
impl ImageCreationIntent {
    pub fn verify_build_export(
        &self,
        image_id: &str,
        labels: &BTreeMap<String, String>,
        registry_id: &str,
    ) -> Result<VerifiedImageExport, Error> {
        self.validate()?;
        if self.source != ImageIntentSource::Build
            || labels.get("com.zackees.bosn.image-preparation") != Some(&self.ownership_proof()?)
        {
            return Err(Error::BadRow("image export preparation proof"));
        }
        self.verify_export_identity(image_id, labels, registry_id)
    }

    /// A mutable tag or image config digest alone cannot prove a completed pull.
    /// The freshly inspected image must advertise the exact planned repo digest.
    pub fn verify_pull_export(
        &self,
        image_id: &str,
        repo_digests: &[String],
        labels: &BTreeMap<String, String>,
        registry_id: &str,
    ) -> Result<VerifiedImageExport, Error> {
        self.validate()?;
        if !self.matches_pull_repository(repo_digests)? {
            return Err(Error::BadRow("image pull repository digest proof"));
        }
        self.verify_export_identity(image_id, labels, registry_id)
    }

    /// Compare immutable repository identity without granting ownership.
    pub fn matches_pull_repository(&self, repo_digests: &[String]) -> Result<bool, Error> {
        self.validate()?;
        Ok(self.source == ImageIntentSource::Pull
            && repo_digests.iter().any(|reference| {
                normalized_repository_digest(reference)
                    == normalized_repository_digest(&self.reference)
            }))
    }

    fn verify_export_identity(
        &self,
        image_id: &str,
        labels: &BTreeMap<String, String>,
        registry_id: &str,
    ) -> Result<VerifiedImageExport, Error> {
        let digest = image_id
            .strip_prefix("sha256:")
            .ok_or(Error::BadRow("image export identity"))?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::BadRow("image export preparation proof"));
        }
        if labels.contains_key(bosn_core::LABEL_REGISTRY)
            || labels.contains_key(bosn_core::LABEL_KIND)
        {
            let owner = bosn_core::ResourceLabels::parse(labels)
                .map_err(|_| Error::BadRow("image export ownership"))?;
            if owner.registry != registry_id
                || owner.kind != ResourceKind::Image
                || owner.stack != self.stack
                || owner.workspace != self.workspace
                || owner.scope != Scope::Machine
                || (owner.generation != image_id
                    && owner.generation != format!("sha256:{}", self.content_sha256))
            {
                return Err(Error::BadRow("image export foreign ownership"));
            }
        }
        if labels
            .get(bosn_core::LABEL_RETENTION)
            .is_some_and(|value| value != "warm" && value != "ephemeral")
        {
            return Err(Error::BadRow("image export retention protection"));
        }
        Ok(VerifiedImageExport {
            intent: self.clone(),
            image_id: image_id.into(),
            registry_id: registry_id.into(),
        })
    }
}

/// Docker's documented default host/namespace; explicit registries stay distinct.
fn normalized_repository_digest(reference: &str) -> Option<String> {
    let (repository, digest) = reference.rsplit_once("@sha256:")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let (host, path) = match repository.split_once('/') {
        Some((host, path)) if host.contains('.') || host.contains(':') || host == "localhost" => {
            (host, path)
        }
        _ => ("docker.io", repository),
    };
    let path = if host == "docker.io" && !path.contains('/') {
        format!("library/{path}")
    } else {
        path.to_owned()
    };
    Some(format!("{host}/{path}@sha256:{digest}"))
}

impl Immediate<'_> {
    /// Consume only the still-current attempt after recording its proven image.
    pub fn reconcile_image_export(&mut self, proof: VerifiedImageExport) -> Result<bool, Error> {
        let owner = self.transaction.query(
            "SELECT value FROM meta WHERE key='registry_id'",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 128,
            },
        )?;
        if owner.len() != 1 || text(&owner[0], 0)? != proof.registry_id {
            return Err(Error::BadRow("image recovery registry identity"));
        }
        let intent = proof.intent;
        let expected = serde_json::to_string(&intent)
            .map_err(|_| Error::BadRow("image preparation intent"))?;
        let rows = self.transaction.query(
            "SELECT value FROM meta WHERE key=?",
            &[Value::Text(intent.key()?)],
            QueryLimits {
                max_rows: 1,
                max_bytes: MAX_BYTES,
            },
        )?;
        if rows.is_empty() || text(&rows[0], 0)? != expected {
            return Ok(false);
        }
        let namespace = match intent.owner {
            ImageIntentOwner::Setup => "setup-image",
            ImageIntentOwner::Manifest => "manifest-image",
        };
        let identity = format!("{namespace}:{}", proof.image_id);
        let proposed = Resource {
            id: identity.clone(),
            kind: ResourceKind::Image,
            name: identity.clone(),
            stack: intent.stack.clone(),
            generation: proof.image_id.clone(),
            scope: Scope::Machine,
            workspace: intent.workspace.clone(),
            created_at: intent.created_at,
            last_used: intent.created_at,
            state: ResourceState::Active,
            retention: Retention::Warm,
        };
        let existing = self.transaction.query("SELECT id,kind,name,stack,generation,scope,workspace,created_at,last_used,state,retention FROM resources WHERE kind='image' AND name=?",
            &[Value::Text(identity.clone())], QueryLimits { max_rows: 1, max_bytes: MAX_BYTES })?;
        let mut recorded = match existing.first() {
            Some(row) => resource(row)?,
            None => proposed,
        };
        let usage = self.transaction.query(
            "SELECT MAX(last_used) FROM resource_uses WHERE resource_id=?",
            &[Value::Text(identity.clone())],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )?;
        let latest_use = if matches!(usage[0].get(0), Some(Value::Null)) {
            intent.created_at
        } else {
            real(&usage[0], 0)?
        };
        if !recorded.last_used.is_finite() || !latest_use.is_finite() {
            return Err(Error::BadRow("image recovery last use"));
        }
        recorded.last_used = recorded.last_used.max(intent.created_at).max(latest_use);
        self.put_resource_preserving_pin(&recorded)?;
        self.put_resource_use(&ResourceUse {
            resource_id: identity,
            workspace: intent.workspace.clone(),
            stack: intent.stack.clone(),
            generation: proof.image_id,
            last_used: recorded.last_used,
            state: ResourceState::Active,
        })?;
        self.delete_image_creation_intent(&intent)?;
        Ok(true)
    }

    pub fn put_image_creation_intent(&mut self, intent: &ImageCreationIntent) -> Result<(), Error> {
        intent.validate()?;
        let key = intent.key()?;
        let value =
            serde_json::to_string(intent).map_err(|_| Error::BadRow("image preparation intent"))?;
        if value.len() > MAX_BYTES {
            return Err(Error::BadRow("image preparation intent size"));
        }
        let existing = self.transaction.query(
            "SELECT 1 FROM meta WHERE key=?",
            &[Value::Text(key.clone())],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )?;
        if existing.is_empty() {
            let rows = self.transaction.query(
                "SELECT key FROM meta WHERE key GLOB 'image-preparation:*' LIMIT 1024",
                &[],
                QueryLimits {
                    max_rows: MAX_INTENTS,
                    max_bytes: 131_072,
                },
            )?;
            if rows.len() >= MAX_INTENTS {
                return Err(Error::BadRow("image preparation intent ceiling"));
            }
        }
        self.set_meta(&key, &value)
    }

    pub fn delete_image_creation_intent(
        &mut self,
        intent: &ImageCreationIntent,
    ) -> Result<(), Error> {
        intent.validate()?;
        // An older completion cannot consume a newer attempt's intent.
        let value =
            serde_json::to_string(intent).map_err(|_| Error::BadRow("image preparation intent"))?;
        self.transaction.execute(
            "DELETE FROM meta WHERE key=? AND value=?",
            &[Value::Text(intent.key()?), Value::Text(value)],
        )?;
        Ok(())
    }
}

impl Registry {
    pub fn image_creation_intents(&self) -> Result<Vec<ImageCreationIntent>, Error> {
        read_intents(&self.connection)
    }
}
impl ReadOnlyRegistry {
    pub fn image_creation_intents(&self) -> Result<Vec<ImageCreationIntent>, Error> {
        read_intents(&self.connection)
    }
}
fn read_intents(connection: &Connection) -> Result<Vec<ImageCreationIntent>, Error> {
    let rows = connection.query(
        "SELECT key,value FROM meta WHERE key GLOB 'image-preparation:*' ORDER BY key",
        &[],
        QueryLimits {
            max_rows: MAX_INTENTS,
            max_bytes: MAX_INTENTS * MAX_BYTES,
        },
    )?;
    rows.iter()
        .map(|row| {
            let value = text(row, 1)?;
            if value.len() > MAX_BYTES {
                return Err(Error::BadRow("image preparation intent size"));
            }
            let intent: ImageCreationIntent = serde_json::from_str(&value)
                .map_err(|_| Error::BadRow("image preparation intent"))?;
            intent.validate()?;
            if intent.key()? != text(row, 0)? {
                return Err(Error::BadRow("image preparation identity"));
            }
            Ok(intent)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_export_requires_exact_repository_digest_and_respects_protection() {
        let owner = "11111111-2222-4333-8444-555555555555";
        let intent = ImageCreationIntent {
            reference: format!("alpine@sha256:{}", "a".repeat(64)),
            content_sha256: "b".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: ImageIntentOwner::Setup,
            source: ImageIntentSource::Pull,
            created_at: 1.0,
        };
        let identity = format!("sha256:{}", "c".repeat(64));
        let labels = BTreeMap::new();
        for references in [
            vec![],
            vec!["alpine:latest".into()],
            vec![format!("different@sha256:{}", "a".repeat(64))],
        ] {
            assert!(
                intent
                    .verify_pull_export(&identity, &references, &labels, owner)
                    .is_err()
            );
        }
        let references = vec![intent.reference.clone()];
        assert!(
            intent
                .verify_pull_export(&identity, &references, &labels, owner)
                .is_ok()
        );
        assert!(
            intent
                .verify_pull_export("malformed", &references, &labels, owner)
                .is_err()
        );
        for protected in [
            BTreeMap::from([(bosn_core::LABEL_RETENTION.into(), "pinned".into())]),
            BTreeMap::from([(bosn_core::LABEL_REGISTRY.into(), "foreign".into())]),
        ] {
            assert!(
                intent
                    .verify_pull_export(&identity, &references, &protected, owner)
                    .is_err()
            );
        }
    }

    #[test]
    fn preparation_intent_survives_reopen_and_stale_completion() {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let database = directory.path().join("registry.sqlite3");
        let mut registry =
            Registry::create_writer(&database, "11111111-2222-4333-8444-555555555555").unwrap();
        let old = ImageCreationIntent {
            reference: format!("bosn-setup:{}", "a".repeat(64)),
            content_sha256: "a".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: ImageIntentOwner::Manifest,
            source: ImageIntentSource::Build,
            created_at: 1.0,
        };
        let mut newer = old.clone();
        newer.created_at = 2.0;
        assert_eq!(
            old.ownership_proof().unwrap(),
            newer.ownership_proof().unwrap()
        );
        let mut other_workspace = old.clone();
        other_workspace.workspace = "/different-workspace".into();
        assert_ne!(
            old.ownership_proof().unwrap(),
            other_workspace.ownership_proof().unwrap()
        );
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.put_image_creation_intent(&old).unwrap();
        transaction.put_image_creation_intent(&newer).unwrap();
        transaction.delete_image_creation_intent(&old).unwrap();
        transaction.commit().unwrap();
        drop(registry);
        let mut registry = Registry::open_writer(&database).unwrap();
        assert_eq!(
            registry.image_creation_intents().unwrap(),
            vec![newer.clone()]
        );
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.delete_image_creation_intent(&newer).unwrap();
        let mut invalid = old.clone();
        invalid.reference = "unrelated:latest".into();
        assert!(transaction.put_image_creation_intent(&invalid).is_err());
        transaction.commit().unwrap();
        assert!(registry.image_creation_intents().unwrap().is_empty());
    }
    #[test]
    fn image_export_recovery_requires_proof_and_preserves_recent_pins() {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut registry =
            Registry::create_writer(directory.path().join("registry.sqlite3"), owner).unwrap();
        let intent = ImageCreationIntent {
            reference: format!("bosn-setup:{}", "a".repeat(64)),
            content_sha256: "a".repeat(64),
            workspace: "/workspace".into(),
            stack: "app".into(),
            owner: ImageIntentOwner::Manifest,
            source: ImageIntentSource::Build,
            created_at: 1.0,
        };
        let image_id = format!("sha256:{}", "b".repeat(64));
        assert!(
            intent
                .verify_build_export(&image_id, &BTreeMap::new(), owner)
                .is_err()
        );
        let labels = BTreeMap::from([(
            "com.zackees.bosn.image-preparation".into(),
            intent.ownership_proof().unwrap(),
        )]);
        let resource = Resource {
            id: format!("manifest-image:{image_id}"),
            kind: ResourceKind::Image,
            name: format!("manifest-image:{image_id}"),
            stack: "app".into(),
            generation: image_id.clone(),
            scope: Scope::Machine,
            workspace: "/workspace".into(),
            created_at: 1.0,
            last_used: 20.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        };
        let mut transaction = registry.begin_immediate().unwrap();
        transaction.put_resource(&resource).unwrap();
        transaction.put_image_creation_intent(&intent).unwrap();
        let proof = intent
            .verify_build_export(&image_id, &labels, owner)
            .unwrap();
        assert!(transaction.reconcile_image_export(proof).unwrap());
        transaction.commit().unwrap();
        let restored = registry
            .resource_by_kind_name(ResourceKind::Image, &resource.name)
            .unwrap()
            .unwrap();
        assert_eq!(restored.retention, Retention::Pinned);
        assert_eq!(restored.last_used, 20.0);
        assert!(registry.image_creation_intents().unwrap().is_empty());
    }
}
