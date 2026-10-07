//! Bounded selected-output evidence, attached to one qualified execution.

use std::{collections::BTreeMap, fmt};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor},
};

use super::{ActLine, Job};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OutputEvidenceError {
    MissingOutput,
    MaskedOutput,
    OutputLimit,
    InvalidEvent,
    DuplicateEvent,
    UnqualifiedIdentity,
    #[serde(other)]
    Unknown,
}

/// This is execution data, never a pass or a ci-lint attestation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobOutputEvidence {
    pub schema_version: u32,
    pub seq: u64,
    pub values: BTreeMap<String, String>,
    pub error: Option<OutputEvidenceError>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(super) struct OutputEvent {
    #[serde(rename = "ciOutputSchema")]
    schema_version: Option<u32>,
    #[serde(rename = "jobOutputs")]
    values: Option<OutputValues>,
    #[serde(rename = "jobOutputsError")]
    error: Option<OutputEvidenceError>,
}

#[derive(Debug, schemars::JsonSchema)]
struct OutputValues(BTreeMap<String, String>);

impl<'de> Deserialize<'de> for OutputValues {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueValues;
        impl<'de> Visitor<'de> for UniqueValues {
            type Value = OutputValues;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded object of distinct output names to strings")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = BTreeMap::new();
                while let Some((name, value)) = map.next_entry::<String, String>()? {
                    if !super::super::params::valid_output_name(&name)
                        || values.len() >= 256
                        || name.len() > 1024
                        || value.len() > 65536
                        || values.insert(name, value).is_some()
                    {
                        return Err(de::Error::custom("invalid, duplicated or oversized output"));
                    }
                }
                if serde_json::to_vec(&values).map_or(true, |bytes| bytes.len() > 65536) {
                    return Err(de::Error::custom("output payload exceeds 64 KiB"));
                }
                Ok(OutputValues(values))
            }
        }
        deserializer.deserialize_map(UniqueValues)
    }
}

impl OutputEvent {
    fn present(&self) -> bool {
        self.schema_version.is_some() || self.values.is_some() || self.error.is_some()
    }

    fn valid(&self) -> bool {
        self.schema_version == Some(1)
            && ((self
                .values
                .as_ref()
                .is_some_and(|values| !values.0.is_empty())
                && self.error.is_none())
                || (self.values.is_none() && self.error.is_some()))
    }
}

impl Job {
    pub(super) fn observe_outputs(&mut self, act: &ActLine, seq: u64, qualified: bool) {
        if act.msg != "CI output evidence" && !act.output.present() {
            return;
        }
        if let Some(previous) = &mut self.output_evidence {
            previous.error = Some(OutputEvidenceError::DuplicateEvent);
            previous.values.clear();
            return;
        }
        let error = if !qualified {
            Some(OutputEvidenceError::UnqualifiedIdentity)
        } else if act.raw_output
            || act.msg != "CI output evidence"
            || !act.output.valid()
            || act.step_ids.is_some()
            || act.synthetic_step_ids.is_some()
            || act.stage.is_some()
            || act.step.is_some()
            || act.step_result.is_some()
            || act.job_result.is_some()
        {
            Some(OutputEvidenceError::InvalidEvent)
        } else {
            act.output.error
        };
        self.output_evidence = Some(JobOutputEvidence {
            schema_version: act.output.schema_version.unwrap_or(0),
            seq,
            values: if error.is_none() {
                act.output
                    .values
                    .as_ref()
                    .map_or_else(BTreeMap::new, |values| values.0.clone())
            } else {
                BTreeMap::new()
            },
            error,
        });
    }
}
