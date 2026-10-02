//! Documentary coverage only: producer-labelled data is not an authorization capability.
use crate::act::{ActProofScope, ActRequiredCell};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ActMatrixTuple(BTreeMap<String, Value>);
impl ActMatrixTuple {
    pub fn new(values: BTreeMap<String, Value>) -> Result<Self, &'static str> {
        let tuple = Self(values);
        tuple.validate()?;
        Ok(tuple)
    }
    pub fn empty() -> Self {
        Self(BTreeMap::new())
    }
    pub fn canonical_json(&self) -> String {
        fn canonical(v: &Value) -> Value {
            match v {
                Value::Object(o) => {
                    let sorted: BTreeMap<_, _> =
                        o.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
                    Value::Object(sorted.into_iter().collect())
                }
                Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
                _ => v.clone(),
            }
        }
        // Serialize sorted maps recursively, independent of serde_json preserve_order.
        fn encode(v: &Value) -> String {
            match v {
                Value::Object(o) => {
                    let sorted: BTreeMap<_, _> = o.iter().collect();
                    format!(
                        "{{{}}}",
                        sorted
                            .iter()
                            .map(|(k, v)| format!(
                                "{}:{}",
                                serde_json::to_string(k).unwrap(),
                                encode(v)
                            ))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                }
                Value::Array(a) => {
                    format!("[{}]", a.iter().map(encode).collect::<Vec<_>>().join(","))
                }
                _ => serde_json::to_string(v).unwrap(),
            }
        }
        encode(&canonical(&serde_json::to_value(&self.0).unwrap()))
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        fn add(bytes: &mut usize, amount: usize) -> Result<(), &'static str> {
            *bytes = bytes
                .checked_add(amount)
                .ok_or("matrix byte bound exceeded")?;
            if *bytes > 32768 {
                return Err("matrix byte bound exceeded");
            }
            Ok(())
        }
        fn string_size(s: &str) -> Result<usize, &'static str> {
            if s.len() > 32768 {
                return Err("matrix string bound exceeded");
            }
            Ok(2 + s
                .chars()
                .map(|c| match c {
                    '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
                    c if c <= '\u{1f}' => 6,
                    c => c.len_utf8(),
                })
                .sum::<usize>())
        }
        fn key(k: &str) -> Result<(), &'static str> {
            if k.is_empty() || k.len() > 64 || k.chars().any(char::is_control) {
                Err("unsafe matrix key")
            } else {
                Ok(())
            }
        }
        fn visit(
            v: &Value,
            depth: usize,
            nodes: &mut usize,
            bytes: &mut usize,
        ) -> Result<(), &'static str> {
            *nodes += 1;
            if depth > 8 || *nodes > 1024 {
                return Err("matrix depth or node bound exceeded");
            }
            match v {
                Value::Number(n) => {
                    if !n.is_i64() && !n.is_u64() {
                        return Err("matrix floats unsupported");
                    }
                    add(bytes, n.to_string().len())?;
                }
                Value::String(s) => add(bytes, string_size(s)?)?,
                Value::Null => add(bytes, 4)?,
                Value::Bool(b) => add(bytes, if *b { 4 } else { 5 })?,
                Value::Array(a) => {
                    add(bytes, 2 + a.len().saturating_sub(1))?;
                    for v in a {
                        visit(v, depth + 1, nodes, bytes)?;
                    }
                }
                Value::Object(o) => {
                    add(bytes, 2 + o.len().saturating_sub(1))?;
                    for (k, v) in o {
                        key(k)?;
                        add(bytes, string_size(k)? + 1)?;
                        visit(v, depth + 1, nodes, bytes)?;
                    }
                }
            }
            Ok(())
        }
        if self.0.len() > 32 {
            return Err("matrix axis bound exceeded");
        }
        let mut nodes = 1;
        let mut bytes = 2 + self.0.len().saturating_sub(1);
        for (k, v) in &self.0 {
            key(k)?;
            add(&mut bytes, string_size(k)? + 1)?;
            visit(v, 1, &mut nodes, &mut bytes)?;
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ActMatrixTuple {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(BTreeMap::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActObservationProvenance {
    ProducerGraph,
    ProducerExecution,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActGraphMatrix {
    NoMatrix,
    Exact(ActMatrixTuple),
}
#[derive(Clone, Debug)]
pub struct ActGraphObservation {
    pub workflow: String,
    pub job: String,
    pub runner: String,
    pub matrix: ActGraphMatrix,
    pub provenance: ActObservationProvenance,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActObservedOutcome {
    Success,
    Failed,
    Skipped,
    Unsupported,
}
#[derive(Clone, Debug)]
pub struct ActExecutionObservation {
    pub workflow: Option<String>,
    pub job: String,
    pub runner: Option<String>,
    pub matrix: Option<ActMatrixTuple>,
    pub outcome: ActObservedOutcome,
    pub provenance: ActObservationProvenance,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActCoverageReason {
    Success,
    GithubOnly,
    UnresolvedMatrix,
    Missing,
    Ambiguous,
    IncompleteIdentity,
    Failed,
    Skipped,
    Unsupported,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActCellCoverage {
    pub id: String,
    pub reason: ActCoverageReason,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActCoverageReport {
    pub cells: Vec<ActCellCoverage>,
    /// Tuple coverage only, never authenticated source or native execution proof.
    pub local_complete: bool,
    /// Tuple coverage only; required GithubOnly cells prevent completion.
    pub full_complete: bool,
    pub documentary_only: bool,
}
/// All provenance fields are documentary assertions. This result cannot authorize mutations.
pub fn reconcile_act_coverage(
    required: &[ActRequiredCell],
    graph: &[ActGraphObservation],
    observed: &[ActExecutionObservation],
) -> Result<ActCoverageReport, &'static str> {
    if required.is_empty() || required.len() > 4096 || graph.len() > 4096 || observed.len() > 4096 {
        return Err("coverage inventory bound exceeded");
    }
    fn identifier(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    }
    fn identity(workflow: &str, job: &str, runner: &str) -> bool {
        !workflow.is_empty()
            && workflow.len() <= 1024
            && !workflow.starts_with('/')
            && !workflow.contains('\\')
            && !workflow.chars().any(char::is_control)
            && workflow
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != "..")
            && identifier(job)
            && identifier(runner)
    }
    let mut ids = std::collections::BTreeSet::new();
    for c in required {
        if !identifier(&c.id) || !ids.insert(&c.id) || !identity(&c.workflow, &c.job, &c.runner) {
            return Err("invalid or duplicate required identity");
        }
    }
    for g in graph {
        if !identity(&g.workflow, &g.job, &g.runner) {
            return Err("invalid graph identity");
        }
    }
    for o in observed {
        if !identifier(&o.job)
            || o.workflow
                .as_ref()
                .is_some_and(|w| !identity(w, &o.job, o.runner.as_deref().unwrap_or("unknown")))
            || o.runner.as_ref().is_some_and(|r| !identifier(r))
        {
            return Err("invalid execution identity");
        }
    }
    for c in required {
        if let Some(m) = &c.matrix {
            m.validate()?;
        }
    }
    for g in graph {
        if let ActGraphMatrix::Exact(m) = &g.matrix {
            m.validate()?;
        }
    }
    for o in observed {
        if let Some(m) = &o.matrix {
            m.validate()?;
        }
    }
    let mut cells = Vec::with_capacity(required.len());
    for c in required {
        let reason = if c.proof_scope == ActProofScope::GithubOnly {
            ActCoverageReason::GithubOnly
        } else {
            let proofs: Vec<_> = graph
                .iter()
                .filter(|g| g.workflow == c.workflow && g.job == c.job && g.runner == c.runner)
                .collect();
            let resolved = c.matrix.clone().or_else(|| {
                (proofs.len() == 1
                    && proofs[0].provenance == ActObservationProvenance::ProducerGraph
                    && proofs[0].matrix == ActGraphMatrix::NoMatrix)
                    .then(ActMatrixTuple::empty)
            });
            if let Some(matrix) = resolved {
                let incomplete = observed
                    .iter()
                    .any(|o| o.job == c.job && (o.workflow.is_none() || o.runner.is_none()));
                let hits: Vec<_> = observed
                    .iter()
                    .filter(|o| {
                        o.workflow.as_deref() == Some(&c.workflow)
                            && o.job == c.job
                            && o.runner.as_deref() == Some(&c.runner)
                            && o.matrix.as_ref() == Some(&matrix)
                    })
                    .collect();
                let repeated = required
                    .iter()
                    .filter(|other| {
                        other.workflow == c.workflow
                            && other.job == c.job
                            && other.runner == c.runner
                            && (other.matrix.as_ref() == Some(&matrix)
                                || (other.matrix.is_none() && matrix == ActMatrixTuple::empty()))
                    })
                    .count()
                    > 1;
                let exact_proofs: Vec<_> = proofs
                    .iter()
                    .filter(|g| match &g.matrix {
                        ActGraphMatrix::NoMatrix => matrix == ActMatrixTuple::empty(),
                        ActGraphMatrix::Exact(m) => m == &matrix,
                    })
                    .collect();
                let graph_matches = exact_proofs.len() == 1
                    && exact_proofs[0].provenance == ActObservationProvenance::ProducerGraph;
                if incomplete {
                    ActCoverageReason::IncompleteIdentity
                } else if repeated || hits.len() > 1 || exact_proofs.len() > 1 {
                    ActCoverageReason::Ambiguous
                } else if !graph_matches || hits.is_empty() {
                    ActCoverageReason::Missing
                } else if hits[0].provenance != ActObservationProvenance::ProducerExecution {
                    ActCoverageReason::IncompleteIdentity
                } else {
                    match hits[0].outcome {
                        ActObservedOutcome::Success => ActCoverageReason::Success,
                        ActObservedOutcome::Failed => ActCoverageReason::Failed,
                        ActObservedOutcome::Skipped => ActCoverageReason::Skipped,
                        ActObservedOutcome::Unsupported => ActCoverageReason::Unsupported,
                    }
                }
            } else {
                ActCoverageReason::UnresolvedMatrix
            }
        };
        cells.push(ActCellCoverage {
            id: c.id.clone(),
            reason,
        });
    }
    let local_complete = cells.iter().all(|c| {
        matches!(
            c.reason,
            ActCoverageReason::Success | ActCoverageReason::GithubOnly
        )
    });
    let full_complete = cells.iter().all(|c| c.reason == ActCoverageReason::Success);
    Ok(ActCoverageReport {
        cells,
        local_complete,
        full_complete,
        documentary_only: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tuple(raw: &str) -> ActMatrixTuple {
        serde_json::from_str(raw).unwrap()
    }
    fn cell(matrix: Option<ActMatrixTuple>) -> ActRequiredCell {
        ActRequiredCell {
            id: "cell".into(),
            workflow: "a.yml".into(),
            job: "test".into(),
            runner: "ubuntu-latest".into(),
            proof_scope: ActProofScope::LocalLinux,
            matrix,
        }
    }
    fn graph(matrix: ActGraphMatrix) -> ActGraphObservation {
        ActGraphObservation {
            workflow: "a.yml".into(),
            job: "test".into(),
            runner: "ubuntu-latest".into(),
            matrix,
            provenance: ActObservationProvenance::ProducerGraph,
        }
    }
    fn observation(matrix: ActMatrixTuple) -> ActExecutionObservation {
        ActExecutionObservation {
            workflow: Some("a.yml".into()),
            job: "test".into(),
            runner: Some("ubuntu-latest".into()),
            matrix: Some(matrix),
            outcome: ActObservedOutcome::Success,
            provenance: ActObservationProvenance::ProducerExecution,
        }
    }
    #[test]
    fn missing_is_never_wildcard_and_empty_requires_graph() {
        let o = observation(ActMatrixTuple::empty());
        assert_eq!(
            reconcile_act_coverage(&[cell(None)], &[], std::slice::from_ref(&o))
                .unwrap()
                .cells[0]
                .reason,
            ActCoverageReason::UnresolvedMatrix
        );
        assert!(
            !reconcile_act_coverage(
                &[cell(Some(ActMatrixTuple::empty()))],
                &[],
                std::slice::from_ref(&o)
            )
            .unwrap()
            .full_complete
        );
        assert!(
            reconcile_act_coverage(
                &[cell(None)],
                &[graph(ActGraphMatrix::NoMatrix)],
                std::slice::from_ref(&o)
            )
            .unwrap()
            .full_complete
        );
        assert!(
            !reconcile_act_coverage(
                &[cell(None)],
                &[graph(ActGraphMatrix::Exact(ActMatrixTuple::empty()))],
                &[o]
            )
            .unwrap()
            .full_complete
        );
    }
    #[test]
    fn shard_target_and_floor_are_exact() {
        for (a, b) in [
            (r#"{"shard":1}"#, r#"{"shard":2}"#),
            (
                r#"{"target":"x86_64-apple-darwin","floor":"10.12"}"#,
                r#"{"target":"aarch64-apple-darwin","floor":"11.0"}"#,
            ),
        ] {
            let a = tuple(a);
            let b = tuple(b);
            assert!(
                !reconcile_act_coverage(
                    &[cell(Some(a.clone()))],
                    &[graph(ActGraphMatrix::Exact(a))],
                    &[observation(b)]
                )
                .unwrap()
                .full_complete
            );
        }
    }
    #[test]
    fn skips_duplicates_foreign_and_incomplete_refuse() {
        let m = ActMatrixTuple::empty();
        let c = cell(Some(m.clone()));
        let g = graph(ActGraphMatrix::NoMatrix);
        let o = observation(m);
        for outcome in [
            ActObservedOutcome::Skipped,
            ActObservedOutcome::Failed,
            ActObservedOutcome::Unsupported,
        ] {
            let mut bad = o.clone();
            bad.outcome = outcome;
            assert!(
                !reconcile_act_coverage(std::slice::from_ref(&c), std::slice::from_ref(&g), &[bad])
                    .unwrap()
                    .full_complete
            );
        }
        assert!(
            !reconcile_act_coverage(
                std::slice::from_ref(&c),
                std::slice::from_ref(&g),
                &[o.clone(), o.clone()]
            )
            .unwrap()
            .full_complete
        );
        assert!(
            !reconcile_act_coverage(
                std::slice::from_ref(&c),
                &[
                    g.clone(),
                    graph(ActGraphMatrix::Exact(ActMatrixTuple::empty()))
                ],
                std::slice::from_ref(&o)
            )
            .unwrap()
            .full_complete
        );
        let mut foreign = c.clone();
        foreign.proof_scope = ActProofScope::GithubOnly;
        assert!(
            !reconcile_act_coverage(
                &[foreign],
                std::slice::from_ref(&g),
                std::slice::from_ref(&o)
            )
            .unwrap()
            .full_complete
        );
        let mut other = c.clone();
        other.id = "other".into();
        other.workflow = "b.yml".into();
        let mut incomplete = o.clone();
        incomplete.workflow = None;
        assert!(
            !reconcile_act_coverage(&[c.clone(), other], std::slice::from_ref(&g), &[incomplete])
                .unwrap()
                .local_complete
        );
        assert!(reconcile_act_coverage(&[c.clone(), c], &[g], &[o]).is_err());
    }
    #[test]
    fn multiple_shards_succeed_only_with_exact_graph_and_execution() {
        let a = tuple(r#"{"shard":1}"#);
        let b = tuple(r#"{"shard":2}"#);
        let first = cell(Some(a.clone()));
        let mut second = cell(Some(b.clone()));
        second.id = "second".into();
        let gs = [
            graph(ActGraphMatrix::Exact(a.clone())),
            graph(ActGraphMatrix::Exact(b.clone())),
        ];
        let os = [observation(a), observation(b)];
        assert!(
            reconcile_act_coverage(&[first, second], &gs, &os)
                .unwrap()
                .full_complete
        );
    }
    #[test]
    fn backward_compatible_missing_matrix_and_inventory_bound() {
        let raw = r#"{"id":"x","workflow":"a.yml","job":"test","runner":"ubuntu-latest","proof_scope":"local_linux"}"#;
        let c: ActRequiredCell = serde_json::from_str(raw).unwrap();
        assert!(c.matrix.is_none());
        assert!(!serde_json::to_string(&c).unwrap().contains("matrix"));
        assert!(reconcile_act_coverage(&vec![c; 4097], &[], &[]).is_err());
    }
    #[test]
    fn duplicate_ids_across_successful_shards_and_oversized_observations_refuse() {
        let a = tuple(r#"{"shard":1}"#);
        let b = tuple(r#"{"shard":2}"#);
        let first = cell(Some(a.clone()));
        let second = cell(Some(b.clone()));
        let gs = [
            graph(ActGraphMatrix::Exact(a.clone())),
            graph(ActGraphMatrix::Exact(b.clone())),
        ];
        let os = [observation(a), observation(b)];
        assert!(reconcile_act_coverage(&[first, second], &gs, &os).is_err());
        let c = cell(Some(ActMatrixTuple::empty()));
        let mut g = graph(ActGraphMatrix::NoMatrix);
        g.workflow = "x".repeat(1025);
        assert!(reconcile_act_coverage(std::slice::from_ref(&c), &[g], &[]).is_err());
        let mut o = observation(ActMatrixTuple::empty());
        o.runner = Some("x".repeat(129));
        assert!(reconcile_act_coverage(&[c], &[], &[o]).is_err());
    }
    #[test]
    fn canonical_and_bounds() {
        assert_eq!(
            tuple(r#"{"z":{"z":1,"a":2},"a":null}"#).canonical_json(),
            r#"{"a":null,"z":{"a":2,"z":1}}"#
        );
        for raw in [r#"{"a":1.1}"#, r#"{"":1}"#, r#"{"a\u0000":1}"#] {
            assert!(serde_json::from_str::<ActMatrixTuple>(raw).is_err());
        }
        assert!(
            ActMatrixTuple::new((0..33).map(|i| (i.to_string(), Value::Null)).collect()).is_err()
        );
        assert!(
            ActMatrixTuple::new(BTreeMap::from([(
                "a".into(),
                Value::String("x".repeat(32768))
            )]))
            .is_err()
        );
        assert!(
            ActMatrixTuple::new(BTreeMap::from([(
                "a".into(),
                Value::Array(vec![Value::Null; 1024])
            )]))
            .is_err()
        );
        let mut v = Value::Null;
        for _ in 0..9 {
            v = Value::Array(vec![v]);
        }
        assert!(ActMatrixTuple::new(BTreeMap::from([("a".into(), v)])).is_err());
    }
}
