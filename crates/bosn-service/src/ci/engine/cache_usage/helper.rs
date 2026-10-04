//! Recovery of a single accounting helper after a lost create acknowledgement.

use serde::Deserialize;
use std::collections::BTreeMap;

use super::{CONTROL_DEADLINE, DockerActBackend, engine_image, owned};

const LABEL: &str = "io.bosn.cache.measurement";

pub(super) struct Identity {
    pub name: String,
    nonce: String,
}

impl Identity {
    pub async fn new() -> Result<Self, String> {
        let nonce = crate::ci::new_uuid()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            name: format!("bosn-cache-measure-{nonce}"),
            nonce,
        })
    }

    pub fn label(&self) -> String {
        format!("{LABEL}={}", self.nonce)
    }

    fn verify(&self, document: &str, volume: &str) -> Result<String, String> {
        let rows: Vec<Helper> = serde_json::from_str(document)
            .map_err(|error| format!("helper inspection invalid: {error}"))?;
        let [row] = rows.as_slice() else {
            return Err("helper inspection did not return exactly one container".into());
        };
        let [mount] = row.mounts.as_slice() else {
            return Err("helper inspection has unexpected mounts".into());
        };
        if !valid_id(&row.id)
            || row.name != format!("/{}", self.name)
            || row.config.labels.get(LABEL) != Some(&self.nonce)
            || row.config.image != engine_image()
            || !row.host_config.readonly_rootfs
            || row.host_config.privileged
            || row.host_config.network_mode != "none"
            || mount.kind != "volume"
            || mount.name != volume
            || mount.destination != "/cache"
            || mount.rw
        {
            return Err("helper identity or isolation does not match its create request".into());
        }
        Ok(row.id.clone())
    }
}

pub(super) fn valid_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Helper {
    id: String,
    name: String,
    config: Config,
    host_config: HostConfig,
    mounts: Vec<Mount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    image: String,
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HostConfig {
    readonly_rootfs: bool,
    privileged: bool,
    network_mode: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Mount {
    #[serde(rename = "Type")]
    kind: String,
    name: String,
    destination: String,
    #[serde(rename = "RW")]
    rw: bool,
}

impl DockerActBackend {
    pub(super) async fn recover_measurement(
        &self,
        identity: &Identity,
        volume: &str,
    ) -> Result<(), String> {
        let result = self
            .run(
                owned(&["container", "inspect", &identity.name]),
                CONTROL_DEADLINE,
            )
            .await
            .map_err(|error| format!("helper {} needs cleanup: {error}", identity.name))?;
        if !result.ok() {
            // Absence now cannot exclude a create still completing after a
            // deadline. Preserve the identity for a later reconciliation.
            return Err(format!(
                "helper {} needs cleanup or late-create reconciliation: {}",
                identity.name,
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
        let document = String::from_utf8_lossy(&result.stdout);
        let id = identity
            .verify(&document, volume)
            .map_err(|error| format!("helper {} needs cleanup: {error}", identity.name))?;
        self.remove_measurement(&id).await
    }

    pub(super) async fn confirm_measurement_absent(&self, id: &str) -> Result<(), String> {
        let result = self
            .run(owned(&["container", "inspect", id]), CONTROL_DEADLINE)
            .await
            .map_err(|error| format!("measurement container {id} absence is unproven: {error}"))?;
        let detail = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
        if !result.ok()
            && (detail.contains("no such container:") || detail.contains("no such object:"))
        {
            return Ok(());
        }
        Err(format!(
            "measurement container {id} needs cleanup; absence is unproven"
        ))
    }
}
