//! Exact fresh ownership facts retained only after successful physical removal.

#[derive(Clone, Debug)]
pub(crate) struct DeletionReceipt {
    pub(crate) state_dir: std::path::PathBuf,
    pub(crate) physical_id: String,
    pub(crate) physical_name: String,
    pub(crate) labels: bosn_core::ResourceLabels,
    pub(crate) observed_at: f64,
}

pub(super) struct Revalidated {
    pub(super) bytes: Option<i128>,
    pub(super) receipt: Option<DeletionReceipt>,
}

impl DeletionReceipt {
    pub(super) fn describe(&self) -> String {
        format!(
            "removed {} {} ({}) from registry {} generation {} at {:.3}, state {}",
            self.labels.kind.as_str(),
            self.physical_id,
            self.physical_name,
            self.labels.registry,
            self.labels.generation,
            self.observed_at,
            self.state_dir.display()
        )
    }
}
