//! Spare engines (#410): an owned Act engine created and prepared before any
//! run exists, then handed to exactly one run.
//!
//! A spare is an ordinary owned-engine record whose intent is marked
//! `spare` and names no run: its run-bound fields are [`ActEngineBinding::spare`]'s.
//! The daemon prepares it under its own execution claim. A run takes it over
//! with [`Immediate::claim_act_spare`], which atomically replaces that claim
//! with the run's and records the run's binding; everything after that
//! (execution, cleanup, removal proof, startup recovery) is the ordinary
//! lifecycle, keyed by the spare's own engine UUID.
use super::*;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
const ZERO_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The run-bound fields of an intent: which run an engine serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActEngineBinding {
    pub run_id: String,
    pub workspace: String,
    pub candidate_sha: String,
    pub payload_sha256: String,
    pub snapshot_sha256: String,
}

impl ActEngineBinding {
    /// What a spare's intent is bound to before any run claims it: its own
    /// engine UUID, the daemon's workspace, and no source.
    pub fn spare(run_id: &str, workspace: &str) -> Self {
        Self {
            run_id: run_id.into(),
            workspace: workspace.into(),
            candidate_sha: ZERO_SHA.into(),
            payload_sha256: ZERO_SHA256.into(),
            snapshot_sha256: ZERO_SHA256.into(),
        }
    }

    pub(super) fn validate(&self) -> Result<(), Error> {
        canonical_run(&self.run_id)?;
        if self.workspace.is_empty()
            || self.workspace.len() > 4096
            || self.workspace.contains('\0')
            || !Path::new(&self.workspace).is_absolute()
            || !hex(&self.candidate_sha, 40)
            || !hex(&self.payload_sha256, 64)
            || !hex(&self.snapshot_sha256, 64)
        {
            return Err(Error::BadRow("act run binding"));
        }
        Ok(())
    }
}

impl ActEngineIntent {
    /// The run-bound fields of this intent.
    pub fn binding(&self) -> ActEngineBinding {
        ActEngineBinding {
            run_id: self.run_id.clone(),
            workspace: self.workspace.clone(),
            candidate_sha: self.candidate_sha.clone(),
            payload_sha256: self.payload_sha256.clone(),
            snapshot_sha256: self.snapshot_sha256.clone(),
        }
    }

    /// Whether `other` would create exactly the engine this intent did: the
    /// same pinned act, engine and runner, and the same frozen creation
    /// profile. A spare is only ever claimed by a run whose intent matches.
    pub fn same_engine(&self, other: &Self) -> bool {
        self.act_version == other.act_version
            && self.act_image_digest == other.act_image_digest
            && self.engine_image_digest == other.engine_image_digest
            && self.runner_image_digest == other.runner_image_digest
            && self.creation_profile.is_some()
            && self.creation_profile == other.creation_profile
    }
}

impl Immediate<'_> {
    /// Hand a prepared spare over to one run: the spare's holder presents
    /// its claim `from`, which is replaced by the run's claim `token`, and
    /// the run's `binding` is recorded, in one transaction. A second claim
    /// (any token) is refused, as is a spare that ever executed.
    pub fn claim_act_spare(
        &mut self,
        spare: &str,
        observed: &ActEngineObservation,
        from: &str,
        token: &str,
        binding: &ActEngineBinding,
        at: f64,
    ) -> Result<ActEngineRecord, Error> {
        canonical_run(from)?;
        canonical_run(token)?;
        binding.validate()?;
        let mut record = self.verify_act_engine(spare, observed)?;
        record.check_time(at)?;
        if !record.intent.spare
            || record.schema_version != 3
            || record.state != ActEngineState::Registered
            || record.execution.is_some()
            || record.binding.is_some()
            || record.execution_claim.as_deref() != Some(from)
            || from == token
        {
            return Err(Error::BadRow("act spare claim"));
        }
        record.execution_claim = Some(token.into());
        record.binding = Some(binding.clone());
        record.updated_at = at;
        self.store_act_record(&record)?;
        Ok(record)
    }
}
