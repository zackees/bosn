//! Qualified reusable executions: caller IDs and every caller's matrix.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ActLine, ItemConclusion, ItemStatus, Job, RunTree};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobIdentity {
    #[serde(rename = "jobID")]
    pub job_id: String,
    pub matrix: Option<Value>,
}

fn matrix(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null() && !v.as_object().is_some_and(|m| m.is_empty()))
}

impl ActLine {
    pub(super) fn qualified_identity(&self) -> Result<Option<Vec<JobIdentity>>, String> {
        let Some(identity) = &self.job_identity else {
            return Ok(None);
        };
        if identity.is_empty()
            || identity.len() > 32
            || identity.iter().any(|i| {
                !super::super::workflow::valid_job_id(&i.job_id)
                    || i.matrix
                        .as_ref()
                        .is_some_and(|m| !m.is_null() && !m.is_object())
            })
            || identity.last().map(|i| i.job_id.as_str()) != self.job_id.as_deref()
            || matrix(identity.last().and_then(|i| i.matrix.as_ref()))
                != matrix(self.matrix.as_ref())
            || serde_json::to_vec(identity).map_or(true, |bytes| bytes.len() > 65536)
        {
            return Err("invalid qualified execution identity".into());
        }
        let mut canonical = identity.clone();
        for i in &mut canonical {
            if matrix(i.matrix.as_ref()).is_none() {
                i.matrix = None;
            }
        }
        Ok(Some(canonical))
    }
}

pub(super) fn execution_key(identity: &[JobIdentity]) -> String {
    format!(
        "@job-{}/{}",
        identity[0].job_id,
        serde_json::to_string(identity).unwrap_or_default()
    )
}

impl Job {
    pub(crate) fn declaration_key(&self) -> String {
        self.identity.as_ref().map_or_else(
            || self.job_id.clone(),
            |i| {
                i.iter()
                    .map(|part| part.job_id.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            },
        )
    }
}

impl RunTree {
    pub(crate) fn qualified_coverage_missing(
        &self,
        declared: &super::super::workflow::Declared,
    ) -> bool {
        self.jobs()
            .filter(|job| {
                job.status != ItemStatus::Queued && job.conclusion != Some(ItemConclusion::Skipped)
            })
            .any(|job| {
                (declared.needs_qualified_identity && job.identity.is_none())
                    || (job.identity.is_some()
                        && !declared.steps.contains_key(&job.declaration_key()))
            })
            || (declared.needs_qualified_identity && self.malformed_lines > 0)
    }
}
