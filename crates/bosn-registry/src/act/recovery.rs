//! Frozen recovery intent. Reserving the lower, stopping source writers and
//! acknowledging publication/release are separate runtime operations.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActToolRecoveryIntent {
    pub schema_version: u32,
    pub owner: String,
    pub engine_id: String,
    pub source_volume: String,
    pub cache_volume: String,
    pub generation: String,
    pub created_at_seconds: i64,
    pub expires_at_seconds: i64,
}

impl ActToolRecoveryIntent {
    fn from_record(record: &ActEngineRecord, created: i64, expires: i64) -> Result<Self, Error> {
        if created < 0 || expires <= created || expires - created > 86400 || expires > (1_i64 << 53)
        {
            return Err(Error::BadRow("tool recovery finite lifetime"));
        }
        let profile = record
            .intent
            .creation_profile
            .as_ref()
            .ok_or(Error::BadRow("tool recovery profile"))?;
        let generation = profile
            .tool_generation
            .as_ref()
            .ok_or(Error::BadRow("tool recovery lower"))?;
        let cache = profile
            .cache_volume
            .as_ref()
            .ok_or(Error::BadRow("tool recovery cache volume"))?;
        let source = record.intent.storage_volume_name().ok_or(Error::BadRow(
            "tool recovery needs persistent named source storage",
        ))?;
        let engine = record
            .engine_id
            .as_ref()
            .ok_or(Error::BadRow("tool recovery engine identity"))?;
        let mut identity = b"bosn.act.tool-recovery-owner.v1\0".to_vec();
        identity.extend(
            serde_json::to_vec(&(
                &record.registry_id,
                &record.intent.run_id,
                engine,
                profile.digest()?,
            ))
            .map_err(|_| Error::BadRow("tool recovery owner encoding"))?,
        );
        Ok(Self {
            schema_version: 1,
            owner: kernal_api::hash::sha256_bytes(&identity).to_hex(),
            engine_id: engine.clone(),
            source_volume: source,
            cache_volume: cache.name.clone(),
            generation: generation.id.clone(),
            created_at_seconds: created,
            expires_at_seconds: expires,
        })
    }

    pub(super) fn validate_record(&self, record: &ActEngineRecord) -> Result<(), Error> {
        if *self != Self::from_record(record, self.created_at_seconds, self.expires_at_seconds)?
            || self.created_at_seconds as f64 > record.updated_at
            || (self.created_at_seconds as f64) < record.intent.created_at.floor()
        {
            return Err(Error::BadRow("tool recovery frozen identity"));
        }
        Ok(())
    }
}

impl Immediate<'_> {
    /// Persist exact intent before making the external reservation call. Retry
    /// must reuse its timestamps, never extend a possibly published owner.
    /// This method does not acknowledge protection or authorize source removal.
    pub fn begin_act_tool_recovery(
        &mut self,
        run: &str,
        token: &str,
        created: i64,
        expires: i64,
    ) -> Result<(), Error> {
        let mut record = self
            .act_record(run)?
            .ok_or(Error::BadRow("act intent missing"))?;
        if record.state != ActEngineState::Registered
            || record.execution_claim.as_deref() != Some(token)
            || record.execution.is_some()
        {
            return Err(Error::BadRow(
                "tool recovery requires exclusive unexecuted claim",
            ));
        }
        let intent = ActToolRecoveryIntent::from_record(&record, created, expires)?;
        if let Some(existing) = &record.tool_recovery {
            return if *existing == intent {
                Ok(())
            } else {
                Err(Error::ResourceIdentityConflict)
            };
        }
        // Pin timestamps have whole-second resolution. Preserve the more
        // precise monotonic registry transition time within the same second.
        record.updated_at = record.updated_at.max(created as f64);
        record.tool_recovery = Some(intent);
        self.store_act_record(&record)
    }
}
