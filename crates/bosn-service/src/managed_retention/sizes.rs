//! Volume sizes for managed retention (#549).
//!
//! `docker volume inspect` never reports a volume's size: Docker computes `UsageData` only for
//! `/system/df`. So a pass that observed owned volumes reads `docker system df -v` once. The read
//! walks every volume, so it is taken only when there is a volume to size, and a failed read
//! leaves sizes unmeasured rather than refusing the pass: sizes bound a budget and account for
//! reclaimed bytes, they never decide ownership.

use std::collections::BTreeMap;

use bosn_core::SystemDfReport;
use bosn_engine::{CensusRead, DockerEngine, RunOptions};

/// Each volume's size in bytes, by name. Empty when the read failed.
pub(super) fn volume_sizes(engine: &DockerEngine, options: RunOptions) -> BTreeMap<String, i128> {
    let Ok(CensusRead::Document(text)) = engine.system_df_verbose(options) else {
        return BTreeMap::new();
    };
    let Ok(report) = serde_json::from_str::<SystemDfReport>(&text) else {
        return BTreeMap::new();
    };
    report
        .volumes
        .into_iter()
        .filter_map(|volume| Some((volume.name, bosn_core::parse_docker_size(&volume.size)?)))
        .collect()
}
