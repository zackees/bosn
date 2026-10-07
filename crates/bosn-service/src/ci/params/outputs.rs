//! Explicit non-secret job outputs requested as execution evidence.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(
    Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct OutputSelector {
    pub path: Vec<String>,
    pub output: String,
}

impl OutputSelector {
    pub(super) fn parse(spec: &str) -> Result<Self, String> {
        let (path, output) = spec
            .split_once(':')
            .ok_or("--ci-output takes job-path:output")?;
        let selector = Self {
            path: path.split('/').map(str::to_string).collect(),
            output: output.to_string(),
        };
        selector.validate()?;
        Ok(selector)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.path.is_empty()
            || self.path.len() > 32
            || self.spec().len() > 1024
            || self
                .path
                .iter()
                .chain([&self.output])
                .any(|name| !valid_name(name))
        {
            return Err("--ci-output takes a bounded qualified job-path:output".into());
        }
        Ok(())
    }

    pub(super) fn spec(&self) -> String {
        format!("{}:{}", self.path.join("/"), self.output)
    }
}

pub(super) fn validate(selectors: &BTreeSet<OutputSelector>) -> Result<(), String> {
    if selectors.len() > 256 {
        return Err("at most 256 --ci-output selectors".into());
    }
    for selector in selectors {
        selector.validate()?;
    }
    Ok(())
}

/// Act2's selected-output contract uses workflow identifiers at every segment.
pub(crate) fn valid_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic() || *first == b'_')
        && super::super::workflow::valid_job_id(name)
}
