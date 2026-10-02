//! Run-record writes, off the state lock and the executor threads.
//!
//! Each change to a run takes a version under the state lock ([`Save`]);
//! the write itself runs on the blocking lane. An older version never
//! overwrites a newer one, so writes may finish in any order, and a run
//! whose directory was pruned is never recreated by a late write.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use super::{CiError, RunRecord, Store, blocking};

/// One version of a run's record, taken under the state lock.
pub(crate) struct Save {
    pub(crate) record: RunRecord,
    pub(crate) version: u64,
}

#[derive(Clone)]
pub(crate) struct RecordWriter {
    store: Store,
    /// The newest version written per run. Held across a write, so two
    /// writes of the same run never interleave.
    written: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl RecordWriter {
    pub(crate) fn new(store: Store) -> Self {
        Self {
            store,
            written: Arc::default(),
        }
    }

    /// Write `save` unless a newer version is already on disk (blocking).
    pub(crate) fn write(&self, save: &Save) {
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        let newest = written.get(&save.record.id).copied().unwrap_or(0);
        if save.version <= newest || !self.store.run_dir(&save.record.id).is_dir() {
            return;
        }
        self.store.save_run(&save.record);
        written.insert(save.record.id.clone(), save.version);
    }

    /// Write `save` and wait until it is on disk.
    pub(crate) async fn save(&self, save: Save) -> Result<(), CiError> {
        let writer = self.clone();
        blocking(move || writer.write(&save)).await
    }

    /// Write `save` in the background (progress, which a later version
    /// supersedes anyway).
    pub(crate) fn save_detached(&self, save: Save) {
        let writer = self.clone();
        kernal_api::async_engine::launch(async move {
            let _ = writer.save(save).await;
        })
        .detach();
    }

    /// Delete a run's directory (blocking). Under the same lock as writes,
    /// so a late write can never recreate it.
    pub(crate) fn remove_run(&self, run: &str) {
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        self.store.remove_run(run);
        written.remove(run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::tests::sample_record;

    fn saved(store: &Store, id: &str) -> Option<RunRecord> {
        let bytes = std::fs::read(store.run_dir(id).join("run.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    #[test]
    fn an_older_version_never_overwrites_a_newer_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let writer = RecordWriter::new(store.clone());
        let mut record = sample_record("run-a");
        std::fs::create_dir_all(store.run_dir(&record.id)).unwrap();
        record.log_records = 2;
        writer.write(&Save {
            record: record.clone(),
            version: 2,
        });
        record.log_records = 1;
        writer.write(&Save {
            record: record.clone(),
            version: 1,
        });
        assert_eq!(saved(&store, "run-a").unwrap().log_records, 2);
    }

    #[test]
    fn a_pruned_run_is_never_recreated_by_a_late_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let writer = RecordWriter::new(store.clone());
        let record = sample_record("run-b");
        writer.write(&Save { record, version: 1 });
        assert!(!store.run_dir("run-b").exists());
    }
}
