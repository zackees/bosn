//! Source-stop phase for native tool recovery.
//!
//! Before a private source disk that feeds a native overlay can be recovered,
//! two facts must be observed from Docker rather than assumed: the engine that
//! held its last reader is gone, and the volume is still this daemon's own,
//! unaltered private storage. Only the trusted runtime may turn those
//! observations into an [`ActToolSourceStopProof`]; a client exit or a failed
//! probe must never construct one.
//!
//! Recording the stop does **not** authorize deletion. Until publication and
//! reference release are integrated, [`super::remove_storage_volume`] refuses
//! while a record holds an unreleased recovery intent, so the source volume
//! survives for the recovering reader and terminal metadata stays unfinalized.

use super::*;
use bosn_registry::act::{
    ActEngineIntent, ActEngineRecord, ActToolRecoveryIntent, ActToolSourceStopProof,
};

/// A recovery reference protects the source volume only while it is live.
///
/// The lifetime is finite by construction (act2 caps it at 24 hours), so an
/// expired reference is an **abandoned** recovery: the helper can no longer
/// publish through it, and holding the private disk forever would wedge cleanup
/// and leak the volume. Retention therefore ends at expiry rather than
/// requiring a release transition that does not exist yet.
fn reference_is_live(frozen: &ActToolRecoveryIntent, now: f64) -> bool {
    (frozen.expires_at_seconds as f64) > now
}

/// A record still holding a live recovery reference keeps its source volume.
pub(crate) fn source_retained_by_recovery(record: &ActEngineRecord) -> bool {
    record
        .tool_recovery
        .as_ref()
        .is_some_and(|frozen| reference_is_live(frozen, crate::act_runtime::now()))
}

/// Prove the frozen source volume is still present and still ours.
///
/// The expected name comes from the frozen intent, not from Docker's answer, so
/// an inspect that reports a different volume is a refusal rather than a
/// substitution.
fn verify_retained_source(
    document: &[u8],
    intent: &ActEngineIntent,
    frozen: &ActToolRecoveryIntent,
    owner: &str,
) -> Result<(), ActEngineError> {
    let expected = intent
        .storage_volume_name()
        .ok_or_else(|| ActEngineError("tool recovery needs named source storage".into()))?;
    if frozen.source_volume != expected {
        return Err(ActEngineError(
            "recovery source volume does not match the engine intent".into(),
        ));
    }
    super::storage_volume::verify_owned_local_volume(
        document,
        &frozen.source_volume,
        &super::storage_volume::labels(intent, owner)?,
    )
}

/// Build the exact trusted proof from frozen identity. Engine absence is
/// established by the caller before this point.
fn proof(
    intent: &ActEngineIntent,
    frozen: &ActToolRecoveryIntent,
    engine_id: Option<&str>,
) -> Result<ActToolSourceStopProof, ActEngineError> {
    let engine_id = engine_id
        .ok_or_else(|| ActEngineError("source stop needs an observed engine identity".into()))?;
    if frozen.engine_id != engine_id {
        return Err(ActEngineError(
            "recovery intent does not bind this engine identity".into(),
        ));
    }
    if frozen.source_volume != intent.storage_volume_name().unwrap_or_default() {
        return Err(ActEngineError(
            "recovery source volume does not match the engine intent".into(),
        ));
    }
    Ok(ActToolSourceStopProof {
        engine_name: intent.engine_name(),
        engine_id: engine_id.to_owned(),
        source_volume: frozen.source_volume.clone(),
    })
}

/// Observe retained source ownership and durably record the stop.
///
/// Ordinary engines have no recovery intent and skip this phase entirely. A
/// frozen intent that has not been acknowledged is a refusal, never a
/// best-effort pass. An already-recorded stop re-verifies ownership and
/// returns without rewriting the timestamp.
pub(crate) async fn stop_source_writers(
    registry: &RegistryActor,
    engine: &DockerEngine,
    record: &ActEngineRecord,
    owner: &str,
) -> Result<(), ActEngineError> {
    let Some(frozen) = record.tool_recovery.as_ref() else {
        return Ok(());
    };
    if record.tool_recovery_reserved_at.is_none() {
        return Err(ActEngineError(
            "source stop requires an acknowledged recovery reservation".into(),
        ));
    }
    let document = super::create::docker_control_budget(
        engine,
        vec![
            "volume".into(),
            "inspect".into(),
            frozen.source_volume.clone(),
        ],
        super::budgets::STORAGE_CONTROL,
    )
    .await?;
    verify_retained_source(&document, &record.intent, frozen, owner)?;
    if record.tool_recovery_source_stopped_at.is_some() {
        return Ok(());
    }
    let receipt = proof(&record.intent, frozen, record.engine_id.as_deref())?;
    registry
        .act_registry(ActRegistryCommand::ToolRecoverySourceStopped {
            run: record.intent.run_id.clone(),
            proof: receipt,
            at: crate::act_runtime::now().max(record.updated_at),
        })
        .await
        .map_err(|e| ActEngineError(format!("source stop not recorded: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::act_engine::tests::{OWNER, disk_intent};

    const ENGINE_ID: &str = "a";

    fn frozen(intent: &ActEngineIntent) -> ActToolRecoveryIntent {
        ActToolRecoveryIntent {
            schema_version: 1,
            owner: "owner".into(),
            engine_id: ENGINE_ID.repeat(64),
            source_volume: intent.storage_volume_name().unwrap(),
            cache_volume: "cache".into(),
            generation: "gen".into(),
            created_at_seconds: 10,
            expires_at_seconds: 20,
        }
    }

    fn owned(intent: &ActEngineIntent) -> serde_json::Value {
        serde_json::json!({
            "Name": intent.storage_volume_name().unwrap(),
            "Driver": "local",
            "Scope": "local",
            "Options": serde_json::Value::Null,
            "Labels": super::super::storage_volume::labels(intent, OWNER).unwrap(),
        })
    }

    fn inspect(volume: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([volume])).unwrap()
    }

    #[test]
    fn exact_retained_source_is_accepted() {
        let intent = disk_intent();
        let frozen = frozen(&intent);
        assert!(verify_retained_source(&inspect(owned(&intent)), &intent, &frozen, OWNER).is_ok());
    }

    #[test]
    fn foreign_or_mutated_source_is_refused() {
        let intent = disk_intent();
        let frozen = frozen(&intent);
        let mut renamed = owned(&intent);
        renamed["Name"] = serde_json::json!("someone-elses-volume");
        let mut foreign = owned(&intent);
        foreign["Labels"][bosn_core::LABEL_REGISTRY] = serde_json::json!("another-daemon");
        let mut networked = owned(&intent);
        networked["Driver"] = serde_json::json!("nfs");
        let mut mounted = owned(&intent);
        mounted["Options"] = serde_json::json!({"type": "nfs"});
        for (field, volume) in [
            ("name", renamed),
            ("registry", foreign),
            ("driver", networked),
            ("options", mounted),
        ] {
            assert!(
                verify_retained_source(&inspect(volume), &intent, &frozen, OWNER).is_err(),
                "{field} was not refused"
            );
        }
        // Absent, ambiguous and unparsable output are all refusals.
        assert!(verify_retained_source(b"[]", &intent, &frozen, OWNER).is_err());
        assert!(verify_retained_source(b"{", &intent, &frozen, OWNER).is_err());
        let twice =
            serde_json::to_vec(&serde_json::json!([owned(&intent), owned(&intent)])).unwrap();
        assert!(verify_retained_source(&twice, &intent, &frozen, OWNER).is_err());
        // Another daemon's identically-named volume is not ours.
        assert!(
            verify_retained_source(&inspect(owned(&intent)), &intent, &frozen, "other").is_err()
        );
    }

    #[test]
    fn a_substituted_frozen_volume_is_refused() {
        let intent = disk_intent();
        let mut frozen = frozen(&intent);
        frozen.source_volume = "someone-elses-volume".into();
        assert!(verify_retained_source(&inspect(owned(&intent)), &intent, &frozen, OWNER).is_err());
    }

    #[test]
    fn retention_ends_when_the_reference_expires() {
        let intent = disk_intent();
        let frozen = frozen(&intent);
        // Live while the finite lifetime has not elapsed...
        assert!(reference_is_live(&frozen, 19.0));
        // ...and an abandoned reference releases the private source disk rather
        // than wedging cleanup forever.
        assert!(!reference_is_live(&frozen, 20.0));
        assert!(!reference_is_live(&frozen, 100_000.0));
    }

    #[test]
    fn proof_is_built_only_from_frozen_identity() {
        let intent = disk_intent();
        let frozen = frozen(&intent);
        let engine_id = ENGINE_ID.repeat(64);
        let receipt = proof(&intent, &frozen, Some(&engine_id)).unwrap();
        assert_eq!(receipt.engine_name, intent.engine_name());
        assert_eq!(receipt.engine_id, engine_id);
        assert_eq!(receipt.source_volume, frozen.source_volume);

        // An unresolved or different engine identity is refused.
        assert!(proof(&intent, &frozen, None).is_err());
        assert!(proof(&intent, &frozen, Some(&"b".repeat(64))).is_err());
    }
}
