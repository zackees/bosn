//! Closed native tool-store receipts and durable enrollment authority.
use super::{ENGINE_CACHE, cache_usage::helper::valid_id};
use crate::ci::cache_policy::CachePolicy;
use serde::{Deserialize, Serialize};

pub(super) const STORE: &str = "/bosn/cache/toolstore-v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    pub schema_version: u32,
    pub nonce: String,
    pub act_sha256: String,
    pub recipe_sha256: String,
    pub policy: CachePolicy,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Proof {
    pub schema_version: u32,
    pub intent: Intent,
    pub initial_generation: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Install {
    pub path: String,
    pub object_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub schema_version: u32,
    pub installs: Vec<Install>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub schema_version: u32,
    pub id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectionState {
    pub schema_version: u32,
    pub id: String,
    pub installs: Vec<Install>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    pub schema_version: u32,
    pub source: String,
    #[serde(default)]
    pub destination: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub completion: String,
    pub published: bool,
    pub reused: bool,
    pub partial: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub pending_stage: String,
    pub bytes: Option<i64>,
    pub entries: Option<usize>,
}

impl Snapshot {
    pub fn validate(
        &self,
        source: &str,
        generation: bool,
        policy: CachePolicy,
    ) -> Result<(), String> {
        self.validate_shape(source, generation, policy, true)
    }

    pub fn validate_plan(&self, source: &str, policy: CachePolicy) -> Result<(), String> {
        if self.reused {
            return Err("tool plan cannot claim a reused publication".into());
        }
        self.validate_shape(source, false, policy, false)
    }

    fn validate_shape(
        &self,
        source: &str,
        generation: bool,
        policy: CachePolicy,
        published: bool,
    ) -> Result<(), String> {
        let destination = if generation {
            format!("{STORE}/.tool-generations-v1/{}", self.id)
        } else {
            format!("{STORE}/{}", self.id)
        };
        let completion = if generation {
            self.completion == "generation-v1"
        } else {
            matches!(self.completion.as_str(), "inner-v1" | "sibling-v1")
        };
        if self.schema_version != 1
            || self.source != source
            || !valid_id(&self.id)
            || self.destination != destination
            || !completion
            || self.published != published
            || self.partial
            || !self.error.is_empty()
            || !self.pending_stage.is_empty()
            || !self
                .bytes
                .is_some_and(|bytes| (0..=policy.repository_max_bytes).contains(&bytes))
            || self.entries.is_none_or(|entries| entries > 100000)
        {
            return Err("tool publication receipt does not prove a complete bounded object".into());
        }
        let _ = self.reused;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    pub schema_version: u32,
    pub generation: Snapshot,
    pub selected: bool,
    pub partial: bool,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub pending_selection: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Usage {
    pub schema_version: u32,
    pub root: String,
    pub observed_at: String,
    pub filesystem_device: Option<u64>,
    pub path_entries: usize,
    pub unique_inodes: usize,
    pub apparent_bytes: Option<i64>,
    pub allocated_bytes: Option<i64>,
    pub unique_file_bytes: Option<i64>,
    pub referenced_file_bytes: Option<i64>,
    pub partial: bool,
    #[serde(default)]
    pub error: String,
}

impl Usage {
    pub fn allocated(&self) -> Result<i64, String> {
        if self.schema_version != 1
            || self.root != STORE
            || self.partial
            || !self.error.is_empty()
            || self.filesystem_device.is_none()
            || time::OffsetDateTime::parse(
                &self.observed_at,
                &time::format_description::well_known::Rfc3339,
            )
            .is_err()
            || self.path_entries > 1000000
            || self.unique_inodes > self.path_entries
            || [
                self.apparent_bytes,
                self.unique_file_bytes,
                self.referenced_file_bytes,
            ]
            .iter()
            .any(|bytes| !bytes.is_some_and(|bytes| bytes >= 0))
        {
            return Err("tool allocation inventory is incomplete or invalid".into());
        }
        self.allocated_bytes
            .filter(|bytes| *bytes >= 0)
            .ok_or_else(|| "tool allocated bytes are unknown".into())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stages {
    pub schema_version: u32,
    pub retired_stages: Option<Vec<String>>,
    pub after: Usage,
    pub partial: bool,
    #[serde(default)]
    pub error: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retention {
    pub schema_version: u32,
    pub stage_retention: Stages,
    pub before: Usage,
    pub after: Usage,
    pub retired_generations: Option<Vec<String>>,
    pub protected_generations: Option<Vec<String>>,
    pub retired_objects: Option<Vec<String>>,
    pub protected_objects: Option<Vec<String>>,
    pub protected_overflow: bool,
    pub partial: bool,
    #[serde(default)]
    pub error: String,
}

pub(super) fn install_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 900
        && !path.bytes().any(|b| b.is_ascii_control())
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !path.starts_with('/')
        && !path.starts_with(ENGINE_CACHE)
}
