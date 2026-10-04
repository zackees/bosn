//! Typed act2 recovery reservation transport. Normal enrollment is not active.
use super::{DockerActBackend, ENGINE_CACHE, ENGINE_WORK};
use bosn_registry::act::ActEngineRecord;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryPin {
    schema_version: u32,
    owner: String,
    generation: String,
    created_at: String,
    expires_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReservationReport {
    schema_version: u32,
    root: String,
    pin: RecoveryPin,
    published: bool,
    reused: bool,
    partial: bool,
    #[serde(default)]
    error: String,
    #[serde(default)]
    pending_stage: String,
}

fn require_reservation(text: &str, expected: &RecoveryPin, root: &str) -> Result<(), String> {
    if text.len() > 4096 {
        return Err("tool recovery receipt exceeds bound".into());
    }
    let report: ReservationReport =
        serde_json::from_str(text).map_err(|e| format!("tool recovery receipt: {e}"))?;
    if report.schema_version != 1
        || report.root != root
        || report.pin != *expected
        || !report.published
        || report.partial
        || !report.error.is_empty()
        || !report.pending_stage.is_empty()
    {
        return Err(
            "tool recovery reservation unacknowledged or differs from frozen intent".into(),
        );
    }
    let _identical_retry = report.reused;
    Ok(())
}

impl DockerActBackend {
    /// The durable intent must be committed before this call. An error never
    /// proves reservation absence and must not authorize stopping source writers.
    pub async fn reserve_tool_recovery(&self, record: &ActEngineRecord) -> Result<(), String> {
        let intent = record
            .tool_recovery
            .as_ref()
            .ok_or("tool recovery intent missing")?;
        intent.validate_record(record).map_err(|e| e.to_string())?;
        if record
            .intent
            .creation_profile
            .as_ref()
            .and_then(|p| p.cache_volume.as_ref())
            .is_none_or(|v| v.target != ENGINE_CACHE)
        {
            return Err("tool recovery cache mount differs from producer".into());
        }
        let timestamp = |seconds| -> Result<String, String> {
            time::OffsetDateTime::from_unix_timestamp(seconds)
                .map_err(|e| format!("tool recovery timestamp: {e}"))?
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|e| format!("tool recovery timestamp: {e}"))
        };
        let pin = RecoveryPin {
            schema_version: 1,
            owner: intent.owner.clone(),
            generation: intent.generation.clone(),
            created_at: timestamp(intent.created_at_seconds)?,
            expires_at: timestamp(intent.expires_at_seconds)?,
        };
        let encoded = serde_json::to_string(&pin).map_err(|e| e.to_string())?;
        // All content is canonical hex, fixed keys and formatted timestamps.
        if encoded.len() > 1024 || encoded.contains('\'') {
            return Err("tool recovery intent encoding invalid".into());
        }
        let path = format!("{ENGINE_WORK}/tool-recovery-intent.json");
        let root = format!("{ENGINE_CACHE}/toolstore-v1");
        let max = record
            .intent
            .creation_profile
            .as_ref()
            .and_then(|p| p.tool_generation.as_ref())
            .ok_or("tool recovery generation missing")?
            .max_payload_bytes;
        let encoded_len = encoded.len();
        let script = format!(
            "record={path}; expected='{encoded}'; if [ -e \"$record\" ] || [ -L \"$record\" ]; then [ -f \"$record\" ] && [ ! -L \"$record\" ] && [ \"$(wc -c <\"$record\")\" -eq {encoded_len} ] && [ \"$(cat \"$record\")\" = \"$expected\" ] || {{ echo 'tool recovery record differs or is unsafe' >&2; exit 1; }}; else (set -C; printf '%s' \"$expected\" >\"$record\"); fi; exec {ENGINE_WORK}/bin/act --cache-server-path {root} cache tool-recovery reserve --record \"$record\" --max-bytes {max} --apply"
        );
        let receipt = self
            .checked(
                "tool recovery reserve",
                Self::exec(&intent.engine_id, &script),
                Duration::from_secs(180),
            )
            .await?;
        require_reservation(&receipt, &pin, &root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_or_different_reservation_never_acknowledges_protection() {
        let pin = RecoveryPin {
            schema_version: 1,
            owner: "a".repeat(64),
            generation: "b".repeat(64),
            created_at: "2026-10-04T00:00:00Z".into(),
            expires_at: "2026-10-04T01:00:00Z".into(),
        };
        let mut receipt = serde_json::json!({"schema_version":1,"root":"/store","pin":pin,"published":true,"reused":false,"partial":false});
        assert!(require_reservation(&receipt.to_string(), &pin, "/store").is_ok());
        receipt["partial"] = true.into();
        assert!(require_reservation(&receipt.to_string(), &pin, "/store").is_err());
        receipt["partial"] = false.into();
        receipt["pin"]["generation"] = "c".repeat(64).into();
        assert!(require_reservation(&receipt.to_string(), &pin, "/store").is_err());
    }
}
