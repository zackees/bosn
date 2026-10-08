//! Recovery of a single accounting helper after a lost create acknowledgement.

use serde::Deserialize;
use std::collections::BTreeMap;

use super::{CONTROL_DEADLINE, DockerActBackend, engine_image, owned};

const LABEL: &str = "io.bosn.cache.measurement";
const MAINTENANCE_LABEL: &str = "io.bosn.cache.maintenance";

pub(in crate::ci::engine) struct Identity {
    pub name: String,
    pub(in crate::ci::engine) nonce: String,
    image: String,
    ownership: BTreeMap<String, String>,
    maintenance: bool,
}

impl Identity {
    pub async fn new() -> Result<Self, String> {
        let nonce = crate::ci::new_uuid()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            name: format!("bosn-cache-measure-{nonce}"),
            nonce,
            image: engine_image(),
            ownership: BTreeMap::new(),
            maintenance: false,
        })
    }

    fn label_key(&self) -> &str {
        if self.maintenance {
            MAINTENANCE_LABEL
        } else {
            LABEL
        }
    }

    pub fn label(&self) -> String {
        format!("{}={}", self.label_key(), self.nonce)
    }

    pub(in crate::ci::engine) fn from_intent(
        intent: &bosn_registry::cache_helper::CacheHelperIntent,
    ) -> Result<Self, String> {
        intent.validate().map_err(|error| error.to_string())?;
        let labels = bosn_core::ResourceLabels::new(
            &intent.registry_id,
            bosn_core::ResourceKind::Container,
            if intent.role.is_some() {
                "ci-cache-maintenance"
            } else {
                "ci-cache-measurement"
            },
            &intent.nonce,
            bosn_core::Scope::Spec,
            "machine",
            &intent.created_at.to_string(),
            Some(bosn_core::Retention::Warm),
        )
        .map_err(|_| "cache helper ownership labels invalid".to_string())?;
        Ok(Self {
            name: intent.name(),
            nonce: intent.nonce.clone(),
            maintenance: intent.role.is_some(),
            image: intent.image.clone(),
            ownership: labels
                .to_map()
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        })
    }
    pub(in crate::ci::engine) fn track(
        &mut self,
        owner: &str,
        volume: &str,
    ) -> Result<bosn_registry::cache_helper::CacheHelperIntent, String> {
        let intent = bosn_registry::cache_helper::CacheHelperIntent {
            registry_id: owner.into(),
            nonce: self.nonce.clone(),
            image: self.image.clone(),
            volume: volume.into(),
            created_at: crate::ci::lifecycle::now_seconds(),
            role: None,
        };
        *self = Self::from_intent(&intent)?;
        Ok(intent)
    }
    pub(in crate::ci::engine) fn ownership_args(&self) -> Vec<String> {
        self.ownership
            .iter()
            .flat_map(|(key, value)| ["--label".into(), format!("{key}={value}")])
            .collect()
    }
    pub(in crate::ci::engine) fn verify(
        &self,
        document: &str,
        volume: &str,
    ) -> Result<String, String> {
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
            || row.config.labels.get(self.label_key()) != Some(&self.nonce)
            || row.config.image != self.image
            || self
                .ownership
                .iter()
                .any(|(key, value)| row.config.labels.get(key) != Some(value))
            || !row.host_config.readonly_rootfs
            || row.host_config.privileged
            || row.host_config.network_mode != "none"
            || row.host_config.cap_drop != ["ALL"]
            || (!row.host_config.cap_add.is_empty()
                && (!self.maintenance || row.host_config.cap_add != ["CAP_DAC_OVERRIDE"]))
            || mount.kind != "volume"
            || mount.name != volume
            || mount.destination
                != if self.maintenance {
                    "/bosn/cache"
                } else {
                    "/cache"
                }
            || mount.rw != self.maintenance
        {
            return Err("helper identity or isolation does not match its create request".into());
        }
        Ok(row.id.clone())
    }
}

pub(in crate::ci::engine) fn valid_id(value: &str) -> bool {
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
    #[serde(deserialize_with = "nullable_caps")]
    cap_add: Vec<String>,
    cap_drop: Vec<String>,
}

fn nullable_caps<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    Option::<Vec<String>>::deserialize(deserializer).map(Option::unwrap_or_default)
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
    pub(in crate::ci::engine) async fn recover_measurement(
        &self,
        identity: &Identity,
        volume: &str,
        tracker: Option<&super::journal::Tracker<'_>>,
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
        if let Some(tracker) = tracker {
            tracker.register(&id).await?;
        }
        self.remove_measurement(&id).await?;
        if let Some(tracker) = tracker {
            tracker.finish(&id).await?;
        }
        Ok(())
    }

    pub(in crate::ci::engine) async fn confirm_measurement_absent(
        &self,
        id: &str,
    ) -> Result<(), String> {
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

#[cfg(test)]
mod role_tests {
    use super::*;
    use bosn_registry::cache_helper::{CacheHelperIntent, CacheHelperRole};

    #[test]
    fn maintenance_recovery_requires_its_own_nonce_scope_and_writable_mount() {
        let mut intent = CacheHelperIntent {
            registry_id: "11111111-2222-4333-8444-555555555555".into(),
            nonce: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            volume: "bosn-ci-cache-v1".into(),
            image: engine_image(),
            created_at: 1.0,
            role: Some(CacheHelperRole::MaintenanceV1),
        };
        let maintenance = Identity::from_intent(&intent).unwrap();
        let mut labels = maintenance.ownership.clone();
        labels.insert(maintenance.label_key().into(), maintenance.nonce.clone());
        let mut document = serde_json::json!({
            "Id": "1".repeat(64), "Name": format!("/{}", maintenance.name),
            "Config": {"Image": intent.image, "Labels": labels},
            "HostConfig": {"ReadonlyRootfs": true, "Privileged": false, "NetworkMode": "none", "CapAdd": null, "CapDrop": ["ALL"]},
            "Mounts": [{"Type": "volume", "Name": intent.volume, "Destination": "/bosn/cache", "RW": true}]
        });
        let inspect = |value: &serde_json::Value| serde_json::to_string(&vec![value]).unwrap();
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_ok()
        );
        // Historical helpers without added capabilities remain removable.
        document["HostConfig"]["CapAdd"] = serde_json::json!(["CAP_DAC_OVERRIDE"]);
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_ok()
        );
        document["HostConfig"]["CapAdd"] = serde_json::json!(["CAP_SYS_ADMIN"]);
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_err()
        );
        document["HostConfig"]
            .as_object_mut()
            .unwrap()
            .remove("CapAdd");
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_err()
        );
        document["HostConfig"]["CapAdd"] = serde_json::json!(["CAP_DAC_OVERRIDE"]);
        intent.role = None;
        let measurement = Identity::from_intent(&intent).unwrap();
        assert!(
            measurement
                .verify(&inspect(&document), &intent.volume)
                .is_err()
        );
        document["Mounts"][0]["RW"] = false.into();
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_err()
        );
        document["Mounts"][0]["RW"] = true.into();
        document["Mounts"][0]["Destination"] = "/cache".into();
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_err()
        );
        document["Mounts"][0]["Destination"] = "/bosn/cache".into();
        document["Mounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "Type": "bind", "Name": "", "Destination": "/foreign", "RW": true
            }));
        assert!(
            maintenance
                .verify(&inspect(&document), &intent.volume)
                .is_err()
        );
    }
}
