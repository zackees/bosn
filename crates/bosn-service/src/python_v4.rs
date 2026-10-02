//! Reconciling an imported Python v4 registry against observed Docker state.

use super::*;

/// Fixed inspection seam for the one offline Python-v4 cutover reconciler.
/// The durable registry chooses every `(kind, name)`; callers cannot use this
/// to list Docker objects, provide an engine ID, or request lifecycle work.
pub trait PythonV4ReconcileExecutor {
    fn inspect(
        &self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<Option<PythonV4ObservedResource>, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PythonV4ObservedResource {
    pub engine_id: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

/// Preview and apply share this report.  `refusals` is deliberately compact:
/// exact IDs are included for operator repair, but no arbitrary Docker output
/// becomes a durable authority or command surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PythonV4ReconciliationReport {
    pub verified: Vec<String>,
    pub refusals: Vec<String>,
}
impl PythonV4ReconciliationReport {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.refusals.is_empty()
    }
}

/// Read-only verification for a reconciliation-gated Python-v4 import.  The
/// gate itself is the exclusivity evidence: normal daemon startup/writers
/// refuse it, while this function owns the database-inode lock for its short
/// inspection interval.  No resource is started, stopped, created, removed,
/// adopted, or selected from Docker by name.
pub fn preview_python_v4_reconciliation(
    registry_path: &Path,
    engine: &dyn PythonV4ReconcileExecutor,
) -> Result<PythonV4ReconciliationReport, Error> {
    let registry = Registry::open_reconciliation_preview(registry_path)?;
    Ok(collect_python_v4_reconciliation(&registry, engine)?.report)
}

/// Repeat the complete verification while holding the gated exclusive writer,
/// then atomically append every proof and remove the gate.  A preview is never
/// authorization: engine state and every registry blocker are re-read here.
pub fn apply_python_v4_reconciliation(
    registry_path: &Path,
    engine: &dyn PythonV4ReconcileExecutor,
    at: f64,
) -> Result<PythonV4ReconciliationReport, Error> {
    let mut registry = Registry::open_reconciliation_writer(registry_path)?;
    let collected = collect_python_v4_reconciliation(&registry, engine)?;
    if !collected.report.ready() {
        return Ok(collected.report);
    }
    let mut transaction = registry.begin_immediate()?;
    transaction.complete_python_v4_reconciliation(&collected.proofs, at)?;
    transaction.commit()?;
    Ok(collected.report)
}

pub(crate) struct CollectedPythonV4Reconciliation {
    pub(crate) report: PythonV4ReconciliationReport,
    pub(crate) proofs: Vec<ReconciliationProof>,
}

pub(crate) fn collect_python_v4_reconciliation(
    registry: &impl ReconciliationReader,
    engine: &dyn PythonV4ReconcileExecutor,
) -> Result<CollectedPythonV4Reconciliation, Error> {
    let registry_id = registry.registry_id()?;
    let resources = reconciliation_resources(registry)?;
    let uses = reconciliation_uses(registry)?;
    let leases = reconciliation_leases(registry)?;
    let sessions = reconciliation_sessions(registry)?;
    let intents = reconciliation_intents(registry)?;
    let mut report = PythonV4ReconciliationReport::default();
    let mut proofs = Vec::new();
    if !leases.is_empty() {
        report.refusals.push("legacy_leases_present".into());
    }
    if !sessions.is_empty() {
        report.refusals.push("legacy_sessions_present".into());
    }
    for intent in intents {
        report
            .refusals
            .push(format!("creation_intent:{}", intent.name));
    }
    for resource in resources
        .into_iter()
        .filter(|resource| resource.state == ResourceState::Active)
    {
        let resource_uses: Vec<_> = uses
            .iter()
            .filter(|use_| use_.resource_id == resource.id)
            .collect();
        if resource_uses.is_empty()
            || resource_uses
                .iter()
                .any(|use_| use_.state != ResourceState::Active)
            || resource_uses
                .iter()
                .any(|use_| use_.stack != resource.stack || use_.generation != resource.generation)
            || (resource.scope != Scope::Machine
                && resource_uses
                    .iter()
                    .any(|use_| use_.workspace != resource.workspace))
        {
            report
                .refusals
                .push(format!("ambiguous_use:{}", resource.id));
            continue;
        }
        let observed = match engine.inspect(resource.kind, &resource.name) {
            Ok(Some(observed)) => observed,
            Ok(None) => {
                report.refusals.push(format!("missing:{}", resource.id));
                continue;
            }
            Err(_) => {
                report
                    .refusals
                    .push(format!("inspect_error:{}", resource.id));
                continue;
            }
        };
        if observed.engine_id.is_empty()
            || observed.engine_id.len() > 1024
            || !observed_name_matches(resource.kind, &resource.name, &observed.name)
        {
            report
                .refusals
                .push(format!("identity_mismatch:{}", resource.id));
            continue;
        }
        let Ok(labels) = ResourceLabels::parse(&observed.labels) else {
            report
                .refusals
                .push(format!("foreign_or_ambiguous_labels:{}", resource.id));
            continue;
        };
        if !labels.is_owned_by(&registry_id)
            || labels.kind != resource.kind
            || labels.stack != resource.stack
            || labels.generation != resource.generation
            || labels.scope != resource.scope
            || labels.workspace != resource.workspace
            || labels.retention != resource.retention
        {
            report
                .refusals
                .push(format!("foreign_or_ambiguous_labels:{}", resource.id));
            continue;
        }
        // Retain the observed immutable engine identity until the final atomic
        // event write. The public preview projects only durable resource IDs.
        report.verified.push(resource.id.clone());
        proofs.push(ReconciliationProof {
            resource_id: resource.id,
            engine_id: observed.engine_id,
        });
    }
    report.verified.sort();
    report.refusals.sort();
    proofs.sort_by(|left, right| left.resource_id.cmp(&right.resource_id));
    Ok(CollectedPythonV4Reconciliation { report, proofs })
}

// Imported state is normally small, but never silently truncate it: that
// would make a successful-looking preview omit an active legacy resource.
pub(crate) const MAX_RECONCILIATION_ROWS: usize = 10_000;
pub(crate) trait ReconciliationReader {
    fn registry_id(&self) -> Result<String, bosn_registry::Error>;
    fn resources(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<bosn_registry::Page<Resource>, bosn_registry::Error>;
    fn resource_uses(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<bosn_registry::Page<ResourceUse>, bosn_registry::Error>;
    fn leases(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<bosn_registry::Page<Lease>, bosn_registry::Error>;
    fn execution_sessions(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<bosn_registry::Page<ExecutionSession>, bosn_registry::Error>;
    fn volume_creation_intents(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<bosn_registry::Page<VolumeCreationIntent>, bosn_registry::Error>;
}
macro_rules! impl_reconciliation_reader {
    ($type:ty) => {
        impl ReconciliationReader for $type {
            fn registry_id(&self) -> Result<String, bosn_registry::Error> {
                <$type>::registry_id(self)
            }
            fn resources(
                &self,
                offset: usize,
                limit: usize,
            ) -> Result<bosn_registry::Page<Resource>, bosn_registry::Error> {
                <$type>::resources(self, offset, limit)
            }
            fn resource_uses(
                &self,
                offset: usize,
                limit: usize,
            ) -> Result<bosn_registry::Page<ResourceUse>, bosn_registry::Error> {
                <$type>::resource_uses(self, offset, limit)
            }
            fn leases(
                &self,
                offset: usize,
                limit: usize,
            ) -> Result<bosn_registry::Page<Lease>, bosn_registry::Error> {
                <$type>::leases(self, offset, limit)
            }
            fn execution_sessions(
                &self,
                offset: usize,
                limit: usize,
            ) -> Result<bosn_registry::Page<ExecutionSession>, bosn_registry::Error> {
                <$type>::execution_sessions(self, offset, limit)
            }
            fn volume_creation_intents(
                &self,
                offset: usize,
                limit: usize,
            ) -> Result<bosn_registry::Page<VolumeCreationIntent>, bosn_registry::Error> {
                <$type>::volume_creation_intents(self, offset, limit)
            }
        }
    };
}
impl_reconciliation_reader!(Registry);
impl_reconciliation_reader!(ReadOnlyRegistry);
pub(crate) fn reconciliation_resources(
    registry: &impl ReconciliationReader,
) -> Result<Vec<Resource>, Error> {
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = registry.resources(offset, 1_000)?;
        values.extend(page.items);
        match page.next_offset {
            Some(next) if values.len() < MAX_RECONCILIATION_ROWS => offset = next,
            Some(_) => {
                return Err(Error::Protocol(
                    "too many imported reconciliation resources",
                ));
            }
            None => return Ok(values),
        }
    }
}
pub(crate) fn reconciliation_uses(
    registry: &impl ReconciliationReader,
) -> Result<Vec<ResourceUse>, Error> {
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = registry.resource_uses(offset, 1_000)?;
        values.extend(page.items);
        match page.next_offset {
            Some(next) if values.len() < MAX_RECONCILIATION_ROWS => offset = next,
            Some(_) => return Err(Error::Protocol("too many imported reconciliation uses")),
            None => return Ok(values),
        }
    }
}
pub(crate) fn reconciliation_leases(
    registry: &impl ReconciliationReader,
) -> Result<Vec<Lease>, Error> {
    // Only presence is an authorization veto; avoid retaining a second public
    // liveness representation in this service layer.
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = registry.leases(offset, 1_000)?;
        values.extend(page.items);
        match page.next_offset {
            Some(next) if values.len() < MAX_RECONCILIATION_ROWS => offset = next,
            Some(_) => return Err(Error::Protocol("too many imported reconciliation leases")),
            None => return Ok(values),
        }
    }
}
pub(crate) fn reconciliation_sessions(
    registry: &impl ReconciliationReader,
) -> Result<Vec<ExecutionSession>, Error> {
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = registry.execution_sessions(offset, 1_000)?;
        values.extend(page.items);
        match page.next_offset {
            Some(next) if values.len() < MAX_RECONCILIATION_ROWS => offset = next,
            Some(_) => return Err(Error::Protocol("too many imported reconciliation sessions")),
            None => return Ok(values),
        }
    }
}
pub(crate) fn reconciliation_intents(
    registry: &impl ReconciliationReader,
) -> Result<Vec<VolumeCreationIntent>, Error> {
    let mut values = Vec::new();
    let mut offset = 0;
    loop {
        let page = registry.volume_creation_intents(offset, 1_000)?;
        values.extend(page.items);
        match page.next_offset {
            Some(next) if values.len() < MAX_RECONCILIATION_ROWS => offset = next,
            Some(_) => return Err(Error::Protocol("too many imported reconciliation intents")),
            None => return Ok(values),
        }
    }
}

pub(crate) fn observed_name_matches(kind: ResourceKind, expected: &str, observed: &str) -> bool {
    match kind {
        ResourceKind::Container => observed == format!("/{expected}"),
        ResourceKind::Volume | ResourceKind::Image | ResourceKind::Network => observed == expected,
        // Docker buildx does not expose the resource-label/immutable-ID shape
        // needed here. Refuse rather than turning this cutover into adoption.
        ResourceKind::Builder => false,
    }
}
