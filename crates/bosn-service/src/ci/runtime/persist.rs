//! Run-record writes, off the state lock and the executor threads.
//!
//! Each change to a run takes a version under the state lock ([`Save`]);
//! the write itself runs on the blocking lane. An older version never
//! overwrites a newer one, so writes may finish in any order, and a run
//! whose directory was pruned is never recreated by a late write.

use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};

use super::{CiError, RunRecord, Store, blocking};
use crate::ci::status;

#[derive(Default)]
struct Written {
    version: u64,
    last_status: Option<status::StatusSnapshot>,
    repair_needed: bool,
}

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
    written: Arc<Mutex<BTreeMap<String, Written>>>,
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
        if !self.store.run_dir(&save.record.id).is_dir() {
            return;
        }
        let entry = written
            .entry(save.record.id.clone())
            .or_insert_with(|| Written {
                last_status: status::repair_and_last(&self.store, &save.record.id).unwrap_or_else(
                    |error| {
                        eprintln!("bosn ci: could not inspect status journal: {error}");
                        None
                    },
                ),
                ..Written::default()
            });
        if save.version <= entry.version {
            return;
        }
        self.store.save_run(&save.record);
        entry.version = save.version;
        if let Err(error) = Self::append_status(&self.store, entry, &save.record) {
            eprintln!("bosn ci: could not append status journal: {error}");
        }
    }

    fn append_status(store: &Store, entry: &mut Written, record: &RunRecord) -> io::Result<bool> {
        if entry.repair_needed {
            entry.last_status = status::repair_and_last(store, &record.id)?;
            entry.repair_needed = false;
        }
        let next = entry
            .last_status
            .as_ref()
            .map_or(1, |last| last.seq.saturating_add(1));
        let snapshot = status::StatusSnapshot::of(record, next);
        if entry
            .last_status
            .as_ref()
            .is_some_and(|last| last.same_status(&snapshot))
        {
            return Ok(false);
        }
        if let Err(error) = status::append(store, record, next) {
            entry.repair_needed = true;
            return Err(error);
        }
        entry.last_status = Some(snapshot);
        Ok(true)
    }

    /// Reconcile a terminal record if its journal append failed after save.
    pub(crate) fn ensure_status(&self, run: &str) -> io::Result<(bool, crate::ci::RunState)> {
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = std::fs::read(self.store.run_dir(run).join("run.json"))?;
        let record: RunRecord = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if !written.contains_key(run) {
            let last_status = status::repair_and_last(&self.store, run)?;
            written.insert(
                run.into(),
                Written {
                    last_status,
                    ..Written::default()
                },
            );
        }
        let entry = written.get_mut(run).expect("entry was inserted");
        let appended = Self::append_status(&self.store, entry, &record)?;
        Ok((appended, record.state))
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

    #[test]
    fn status_journal_replays_transitions_but_skips_log_only_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let writer = RecordWriter::new(store.clone());
        let mut record = sample_record("run-c");
        std::fs::create_dir_all(store.run_dir(&record.id)).unwrap();
        for version in 1..=4 {
            match version {
                2 => record.state = crate::ci::RunState::Running,
                3 => record.log_records = 25,
                4 => record.state = crate::ci::RunState::Done,
                _ => {}
            }
            writer.write(&Save {
                record: record.clone(),
                version,
            });
        }
        let all = status::read_since(&store, &record.id, 0, 10).unwrap();
        assert_eq!(
            all.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(all.last().unwrap().log_records, 25);
        assert_eq!(
            status::read_since(&store, &record.id, 2, 10).unwrap()[0].state,
            crate::ci::RunState::Done
        );
    }

    #[test]
    fn terminal_record_repairs_a_missing_final_event() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let mut record = sample_record("run-d");
        std::fs::create_dir_all(store.run_dir(&record.id)).unwrap();
        let writer = RecordWriter::new(store.clone());
        writer.write(&Save {
            record: record.clone(),
            version: 1,
        });
        record.state = crate::ci::RunState::Done;
        store.save_run(&record); // crash after record replacement, before journal append
        let restarted = RecordWriter::new(store.clone());
        assert!(restarted.ensure_status(&record.id).unwrap().0);
        assert!(!restarted.ensure_status(&record.id).unwrap().0);
        let events = status::read_since(&store, &record.id, 0, 10).unwrap();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(events[1].state, crate::ci::RunState::Done);
    }
}
