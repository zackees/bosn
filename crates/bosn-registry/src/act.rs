//! Trusted persistence seam for daemon-owned isolated Act engines.
//! Observations and removal proofs are supplied only after the daemon has made
//! real engine probes. These types are not client authority or Docker commands.
//! Each canonical run UUID has one immutable event kind and self-contained,
//! versioned state snapshots. Recovery selects latest active states in SQL;
//! completed lifetime history never imposes an active-recovery ceiling.
use super::*;
use bosn_core::ResourceLabels;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "act.engine.v1:";
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActEngineIntent {
    pub run_id: String,
    pub workspace: String,
    pub candidate_sha: String,
    pub payload_sha256: String,
    pub snapshot_sha256: String,
    pub act_version: String,
    pub act_image_digest: String,
    pub engine_image_digest: String,
    pub runner_image_digest: String,
    pub created_at: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActEngineState {
    Pending,
    Registered,
    CleanupRequired,
    Terminal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActRunOutcome {
    Passed,
    Failed,
    Cancelled,
    Interrupted,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActEngineRecord {
    pub schema_version: u32,
    pub registry_id: String,
    pub intent: ActEngineIntent,
    pub state: ActEngineState,
    pub engine_id: Option<String>,
    pub execution: Option<ActRunOutcome>,
    #[serde(default)]
    pub execution_claim: Option<String>,
    pub outcome: Option<ActRunOutcome>,
    pub updated_at: f64,
    pub removal: Option<ActEngineRemovalProof>,
}
/// Keyset page ordered by immutable canonical run UUID, independent of state.
#[derive(Clone, Debug, PartialEq)]
pub struct ActEngineRecoveryPage {
    pub items: Vec<ActEngineRecord>,
    pub next_run_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActEngineObservation {
    pub name: String,
    pub engine_id: String,
    pub image_digest: String,
    pub labels: BTreeMap<String, String>,
}
/// Receipt from the trusted runtime's successful absence probe, after removal.
/// The runtime must not construct this when inspect failed or was unavailable.
/// A registered engine requires its exact immutable ID as well as its name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActEngineRemovalProof {
    pub name: String,
    pub engine_id: Option<String>,
}
fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canonical_run(run: &str) -> Result<(), Error> {
    if !is_uuid(run) || run != run.to_ascii_lowercase() {
        return Err(Error::BadRow("act run UUID"));
    }
    Ok(())
}
impl ActEngineIntent {
    pub fn engine_name(&self) -> String {
        format!("bosn-act-{}", self.run_id)
    }
    fn validate(&self) -> Result<(), Error> {
        canonical_run(&self.run_id)?;
        if self.workspace.is_empty()
            || self.workspace.len() > 4096
            || self.workspace.contains('\0')
            || !Path::new(&self.workspace).is_absolute()
            || !hex(&self.candidate_sha, 40)
            || !hex(&self.payload_sha256, 64)
            || !hex(&self.snapshot_sha256, 64)
            || !self.created_at.is_finite()
            || self.created_at < 0.0
            || self.act_version.is_empty()
            || self.act_version.len() > 32
            || !self
                .act_version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            || [
                &self.act_image_digest,
                &self.engine_image_digest,
                &self.runner_image_digest,
            ]
            .iter()
            .any(|v| !v.strip_prefix("sha256:").is_some_and(|v| hex(v, 64)))
        {
            return Err(Error::BadRow("act immutable intent"));
        }
        Ok(())
    }
    pub fn required_labels(&self, owner: &str) -> Result<BTreeMap<String, String>, Error> {
        self.validate()?;
        if !is_uuid(owner) {
            return Err(Error::BadRow("act registry UUID"));
        }
        let mut labels: BTreeMap<String, String> = ResourceLabels::new(
            owner,
            ResourceKind::Container,
            "act",
            &self.run_id,
            Scope::Spec,
            &self.workspace,
            &self.created_at.to_string(),
            Some(Retention::Warm),
        )
        .map_err(|_| Error::BadRow("act ownership labels"))?
        .to_map()
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();
        for (key, value) in [
            ("namespace", "act-engine-v1"),
            ("run-id", self.run_id.as_str()),
            ("candidate-sha", self.candidate_sha.as_str()),
            ("payload-sha256", self.payload_sha256.as_str()),
            ("snapshot-sha256", self.snapshot_sha256.as_str()),
            ("engine-image", self.engine_image_digest.as_str()),
            ("act-image", self.act_image_digest.as_str()),
            ("runner-image", self.runner_image_digest.as_str()),
            ("act-version", self.act_version.as_str()),
        ] {
            labels.insert(format!("com.zackees.bosn.act.{key}"), value.into());
        }
        Ok(labels)
    }
}
impl ActEngineRecord {
    fn validate(&self) -> Result<(), Error> {
        self.intent.validate()?;
        if (self.state == ActEngineState::Terminal) != self.removal.is_some()
            || self.removal.as_ref().is_some_and(|p| {
                p.name != self.intent.engine_name() || p.engine_id != self.engine_id
            })
        {
            return Err(Error::BadRow("act removal snapshot"));
        }
        if !matches!(self.schema_version, 1 | 2)
            || (self.schema_version == 1 && self.execution_claim.is_some())
            || self.execution_claim.as_ref().is_some_and(|token| {
                canonical_run(token).is_err()
                    || self.engine_id.is_none()
                    || self.state == ActEngineState::Pending
            })
            || (self.schema_version == 2
                && self.execution.is_some()
                && self.execution_claim.is_none())
            || !is_uuid(&self.registry_id)
            || !self.updated_at.is_finite()
            || self.updated_at < self.intent.created_at
            || self.engine_id.as_ref().is_some_and(|v| !hex(v, 64))
            || (self.engine_id.is_none() && self.execution.is_some())
            || (self.state == ActEngineState::Pending
                && (self.engine_id.is_some() || self.outcome.is_some()))
            || (self.state == ActEngineState::Registered
                && (self.engine_id.is_none() || self.outcome.is_some()))
            || (matches!(
                self.state,
                ActEngineState::CleanupRequired | ActEngineState::Terminal
            ) && self.outcome.is_none())
            || (self.outcome == Some(ActRunOutcome::Passed)
                && (self.engine_id.is_none() || self.execution != Some(ActRunOutcome::Passed)))
        {
            return Err(Error::BadRow("act state snapshot"));
        }
        Ok(())
    }
    fn check_time(&self, at: f64) -> Result<(), Error> {
        if !at.is_finite() || at < self.updated_at {
            return Err(Error::BadRow("act transition time"));
        }
        Ok(())
    }
}
fn decode(detail: &str, run: &str) -> Result<ActEngineRecord, Error> {
    let record: ActEngineRecord =
        serde_json::from_str(detail).map_err(|_| Error::BadRow("act state JSON"))?;
    record.validate()?;
    if record.intent.run_id != run {
        return Err(Error::BadRow("act event identity"));
    }
    Ok(record)
}
fn kind(run: &str) -> Result<String, Error> {
    canonical_run(run)?;
    Ok(format!("{PREFIX}{run}"))
}
impl Immediate<'_> {
    fn act_record(&mut self, run: &str) -> Result<Option<ActEngineRecord>, Error> {
        let rows = self.transaction.query(
            "SELECT detail FROM events WHERE kind=? ORDER BY id DESC LIMIT 1",
            &[Value::Text(kind(run)?)],
            QueryLimits {
                max_rows: 1,
                max_bytes: 32768,
            },
        )?;
        let record = rows
            .first()
            .map(|r| decode(&text(r, 0)?, run))
            .transpose()?;
        if let Some(record) = &record {
            let owner = self.transaction.query(
                "SELECT value FROM meta WHERE key='registry_id'",
                &[],
                QueryLimits {
                    max_rows: 1,
                    max_bytes: 128,
                },
            )?;
            if owner.first().map(|r| text(r, 0)).transpose()?.as_deref()
                != Some(record.registry_id.as_str())
            {
                return Err(Error::ResourceIdentityConflict);
            }
        }
        Ok(record)
    }
    fn store_act_record(&mut self, record: &ActEngineRecord) -> Result<(), Error> {
        record.validate()?;
        let detail = serde_json::to_string(record).map_err(|_| Error::BadRow("act state JSON"))?;
        self.append_event(record.updated_at, &kind(&record.intent.run_id)?, &detail)
    }
    pub fn begin_act_engine(&mut self, intent: &ActEngineIntent) -> Result<(), Error> {
        intent.validate()?;
        if self.act_record(&intent.run_id)?.is_some() {
            return Err(Error::ResourceIdentityConflict);
        }
        let rows = self.transaction.query(
            "SELECT value FROM meta WHERE key='registry_id'",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 128,
            },
        )?;
        let registry_id = text(rows.first().ok_or(Error::BadRow("registry_id"))?, 0)?;
        self.store_act_record(&ActEngineRecord {
            schema_version: 2,
            registry_id,
            intent: intent.clone(),
            state: ActEngineState::Pending,
            engine_id: None,
            execution: None,
            execution_claim: None,
            outcome: None,
            removal: None,
            updated_at: intent.created_at,
        })
    }
    /// Exact ownership predicate for both registration and pending-intent
    /// crash recovery. A deterministic name alone never authorizes adoption.
    pub fn verify_act_engine(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
    ) -> Result<ActEngineRecord, Error> {
        let record = self
            .act_record(run)?
            .ok_or(Error::BadRow("act intent missing"))?;
        if observed.name != record.intent.engine_name()
            || !hex(&observed.engine_id, 64)
            || observed.image_digest != record.intent.engine_image_digest
            || record
                .engine_id
                .as_ref()
                .is_some_and(|id| id != &observed.engine_id)
            || record
                .intent
                .required_labels(&record.registry_id)?
                .iter()
                .any(|(k, v)| observed.labels.get(k) != Some(v))
        {
            return Err(Error::ResourceIdentityConflict);
        }
        Ok(record)
    }
    pub fn register_act_engine(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
        at: f64,
    ) -> Result<(), Error> {
        self.register_act_identity(run, observed, at, false)
    }
    /// Capture the immutable identity of an engine discovered after a crash
    /// between Docker creation and registration. This preserves cleanup-only
    /// state and never grants permission to execute workflow jobs.
    pub fn recover_act_engine(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
        at: f64,
    ) -> Result<(), Error> {
        self.register_act_identity(run, observed, at, true)
    }
    fn register_act_identity(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
        at: f64,
        recovery: bool,
    ) -> Result<(), Error> {
        let mut record = self.verify_act_engine(run, observed)?;
        record.check_time(at)?;
        let expected = if recovery {
            ActEngineState::CleanupRequired
        } else {
            ActEngineState::Pending
        };
        if record.state != expected || record.engine_id.is_some() {
            return Err(Error::BadRow("act registration transition"));
        }
        let id = format!("act-engine:{run}");
        let occupied = self.transaction.query(
            "SELECT 1 FROM resources WHERE id=? OR (kind='container' AND name=?) LIMIT 1",
            &[
                Value::Text(id.clone()),
                Value::Text(record.intent.engine_name()),
            ],
            QueryLimits {
                max_rows: 1,
                max_bytes: 64,
            },
        )?;
        if !occupied.is_empty() {
            return Err(Error::ResourceIdentityConflict);
        }
        self.put_resource(&Resource {
            id: id.clone(),
            kind: ResourceKind::Container,
            name: record.intent.engine_name(),
            stack: "act".into(),
            generation: run.into(),
            scope: Scope::Spec,
            workspace: record.intent.workspace.clone(),
            created_at: record.intent.created_at,
            last_used: at,
            state: ResourceState::Active,
            retention: Retention::Warm,
        })?;
        self.put_resource_use(&ResourceUse {
            resource_id: id,
            workspace: record.intent.workspace.clone(),
            stack: "act".into(),
            generation: run.into(),
            last_used: at,
            state: ResourceState::Active,
        })?;
        if !recovery {
            record.state = ActEngineState::Registered;
        }
        record.engine_id = Some(observed.engine_id.clone());
        record.updated_at = at;
        self.store_act_record(&record)
    }
    /// Commit one exclusive daemon execution owner before any runtime mutation.
    /// Older snapshots remain cleanup-only; a persisted claim cannot be reused.
    pub fn claim_act_execution(
        &mut self,
        intent: &ActEngineIntent,
        observed: &ActEngineObservation,
        token: &str,
        at: f64,
    ) -> Result<ActEngineRecord, Error> {
        canonical_run(token)?;
        let mut record = self.verify_act_engine(&intent.run_id, observed)?;
        record.check_time(at)?;
        if record.schema_version != 2
            || record.intent != *intent
            || record.state != ActEngineState::Registered
            || record.execution.is_some()
            || record.execution_claim.is_some()
        {
            return Err(Error::BadRow("act exclusive execution claim"));
        }
        record.execution_claim = Some(token.into());
        record.updated_at = at;
        self.store_act_record(&record)?;
        Ok(record)
    }
    pub fn verify_act_execution(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
        token: &str,
    ) -> Result<ActEngineRecord, Error> {
        let record = self.verify_act_execution_owner(run, observed, token)?;
        if record.state != ActEngineState::Registered || record.execution.is_some() {
            return Err(Error::BadRow("act active execution claim"));
        }
        Ok(record)
    }
    /// Trusted cleanup ownership probe also admits an execution already recorded
    /// or cleanup already requested, but never a terminal retired engine.
    pub fn verify_act_execution_owner(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
        token: &str,
    ) -> Result<ActEngineRecord, Error> {
        let record = self.verify_act_engine(run, observed)?;
        Self::check_execution_owner(&record, token)?;
        if !matches!(
            record.state,
            ActEngineState::Registered | ActEngineState::CleanupRequired
        ) {
            return Err(Error::BadRow("act execution ownership state"));
        }
        Ok(record)
    }
    fn check_execution_owner(record: &ActEngineRecord, token: &str) -> Result<(), Error> {
        canonical_run(token)?;
        if record.schema_version != 2 || record.execution_claim.as_deref() != Some(token) {
            return Err(Error::BadRow("act execution owner"));
        }
        Ok(())
    }
    pub fn record_act_execution(
        &mut self,
        run: &str,
        token: &str,
        outcome: ActRunOutcome,
        at: f64,
    ) -> Result<(), Error> {
        let mut record = self
            .act_record(run)?
            .ok_or(Error::BadRow("act intent missing"))?;
        record.check_time(at)?;
        Self::check_execution_owner(&record, token)?;
        if record.state != ActEngineState::Registered || record.execution.is_some() {
            return Err(Error::BadRow("act execution transition"));
        }
        record.execution = Some(outcome);
        record.updated_at = at;
        self.store_act_record(&record)
    }
    /// Owner-only cleanup. Startup recovery must first establish that the
    /// recorded owner cannot still execute; a live claim is not removal authority.
    pub fn request_act_execution_cleanup(
        &mut self,
        run: &str,
        token: &str,
        outcome: ActRunOutcome,
        at: f64,
    ) -> Result<(), Error> {
        let record = self
            .act_record(run)?
            .ok_or(Error::BadRow("act intent missing"))?;
        Self::check_execution_owner(&record, token)?;
        self.request_act_cleanup_inner(run, Some(token), outcome, at)
    }
    pub fn request_act_cleanup(
        &mut self,
        run: &str,
        outcome: ActRunOutcome,
        at: f64,
    ) -> Result<(), Error> {
        self.request_act_cleanup_inner(run, None, outcome, at)
    }
    fn request_act_cleanup_inner(
        &mut self,
        run: &str,
        token: Option<&str>,
        outcome: ActRunOutcome,
        at: f64,
    ) -> Result<(), Error> {
        let mut record = self
            .act_record(run)?
            .ok_or(Error::BadRow("act intent missing"))?;
        record.check_time(at)?;
        if record.execution_claim.as_deref() != token
            || !matches!(
                record.state,
                ActEngineState::Pending | ActEngineState::Registered
            )
            || (outcome == ActRunOutcome::Passed && record.execution != Some(ActRunOutcome::Passed))
        {
            return Err(Error::BadRow("act cleanup transition"));
        }
        record.state = ActEngineState::CleanupRequired;
        record.outcome = Some(outcome);
        record.updated_at = at;
        self.store_act_record(&record)
    }
    /// Recheck registry liveness before the trusted runtime removes an
    /// observed engine. Finalization repeats this predicate after absence.
    pub fn authorize_act_cleanup(
        &mut self,
        run: &str,
        observed: &ActEngineObservation,
    ) -> Result<ActEngineRecord, Error> {
        let record = self.verify_act_engine(run, observed)?;
        if record.state != ActEngineState::CleanupRequired || record.engine_id.is_none() {
            return Err(Error::BadRow("act cleanup authorization"));
        }
        self.check_act_retirement(&record)?;
        Ok(record)
    }
    fn check_act_retirement(&mut self, record: &ActEngineRecord) -> Result<(), Error> {
        let id = format!("act-engine:{}", record.intent.run_id);
        // Recheck ownership/use and liveness before retiring registry facts.
        let rows = self.transaction.query("SELECT 1 FROM resources r WHERE r.id=? AND r.name=? AND r.kind='container' AND r.stack='act' AND r.generation=? AND r.workspace=? AND r.scope='spec' AND r.retention='warm' AND r.state='active' AND EXISTS(SELECT 1 FROM resource_uses u WHERE u.resource_id=r.id AND u.workspace=r.workspace AND u.stack=r.stack AND u.generation=r.generation AND u.state='active') AND NOT EXISTS(SELECT 1 FROM resource_uses u WHERE u.resource_id=r.id AND (u.workspace<>r.workspace OR u.stack<>r.stack OR u.generation<>r.generation OR u.state<>'active')) AND NOT EXISTS(SELECT 1 FROM leases l WHERE l.resource_id=r.id) AND NOT EXISTS(SELECT 1 FROM execution_sessions s WHERE s.container_id=r.id OR s.container_id=r.name)",
                &[Value::Text(id.clone()),Value::Text(record.intent.engine_name()),Value::Text(record.intent.run_id.clone()),Value::Text(record.intent.workspace.clone())], QueryLimits { max_rows: 1, max_bytes: 64 })?;
        if rows.is_empty() {
            return Err(Error::ResourceIdentityConflict);
        }
        Ok(())
    }
    pub fn finalize_act_cleanup(
        &mut self,
        run: &str,
        proof: &ActEngineRemovalProof,
        at: f64,
    ) -> Result<(), Error> {
        let mut record = self
            .act_record(run)?
            .ok_or(Error::BadRow("act intent missing"))?;
        record.check_time(at)?;
        if record.state != ActEngineState::CleanupRequired
            || proof.name != record.intent.engine_name()
            || proof.engine_id != record.engine_id
        {
            return Err(Error::BadRow("act removal proof"));
        }
        if record.engine_id.is_some() {
            let id = format!("act-engine:{run}");
            self.check_act_retirement(&record)?;
            self.transaction.execute(
                "UPDATE resource_uses SET state='retired',last_used=? WHERE resource_id=?",
                &[Value::Real(at), Value::Text(id.clone())],
            )?;
            self.transaction.execute(
                "UPDATE resources SET state='retired',last_used=? WHERE id=?",
                &[Value::Real(at), Value::Text(id)],
            )?;
        }
        record.removal = Some(proof.clone());
        record.state = ActEngineState::Terminal;
        record.updated_at = at;
        self.store_act_record(&record)
    }
}
impl Registry {
    pub fn act_engine(&self, run: &str) -> Result<Option<ActEngineRecord>, Error> {
        let rows = self.connection.query(
            "SELECT detail FROM events WHERE kind=? ORDER BY id DESC LIMIT 1",
            &[Value::Text(kind(run)?)],
            QueryLimits {
                max_rows: 1,
                max_bytes: 32768,
            },
        )?;
        let record = rows
            .first()
            .map(|r| decode(&text(r, 0)?, run))
            .transpose()?;
        let owner = self.registry_id()?;
        if record.as_ref().is_some_and(|r| owner != r.registry_id) {
            return Err(Error::ResourceIdentityConflict);
        }
        Ok(record)
    }
    /// Bounded active recovery page using immutable run-ID keyset ordering.
    /// Follow next_run_id until absent; retiring earlier pages cannot skip work.
    /// Concurrent inserts at or before the cursor are intentionally covered by
    /// the next reconciliation pass. A runtime must repeat passes, rather than
    /// claim that a single traversal proves no concurrent work remains.
    pub fn pending_act_engines(
        &self,
        after_run_id: Option<&str>,
        limit: usize,
    ) -> Result<ActEngineRecoveryPage, Error> {
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(Error::BadRow("act recovery page limit"));
        }
        let after_kind = after_run_id.map(kind).transpose()?.unwrap_or_default();
        let rows = self.connection.query("SELECT e.kind,e.detail FROM events e JOIN (SELECT MAX(id) AS id FROM events WHERE kind GLOB 'act.engine.v1:*' GROUP BY kind) latest ON latest.id=e.id WHERE e.kind > ? AND (json_extract(e.detail,'$.state') IS NULL OR json_extract(e.detail,'$.state')<>'terminal') ORDER BY e.kind LIMIT ?",
            &[Value::Text(after_kind),Value::Integer((limit+1) as i64)], QueryLimits { max_rows: limit+1, max_bytes: (limit+1)*32768 })?;
        let more = rows.len() > limit;
        let items = rows
            .iter()
            .take(limit)
            .map(|r| {
                let k = text(r, 0)?;
                decode(
                    &text(r, 1)?,
                    k.strip_prefix(PREFIX)
                        .ok_or(Error::BadRow("act event kind"))?,
                )
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let owner = self.registry_id()?;
        if items.iter().any(|r| r.registry_id != owner) {
            return Err(Error::ResourceIdentityConflict);
        }
        let next_run_id = more.then(|| {
            items
                .last()
                .expect("nonempty bounded page")
                .intent
                .run_id
                .clone()
        });
        Ok(ActEngineRecoveryPage { items, next_run_id })
    }
}
