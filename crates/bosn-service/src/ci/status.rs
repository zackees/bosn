//! Durable, per-run status snapshots for replayable dashboard updates.
//!
//! The record writer serializes appends for each run. A cursor is assigned
//! only after the record has been saved, so a reconnect can replay saved
//! status changes without depending on the lossy global feed.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
};

use serde::{Deserialize, Serialize};

use super::{
    model::RunTree,
    store::Store,
    wire::{Conclusion, RunRecord, RunState},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusSnapshot {
    pub seq: u64,
    pub run: String,
    pub state: RunState,
    pub conclusion: Option<Conclusion>,
    pub log_records: u64,
    pub tree: RunTree,
}

impl StatusSnapshot {
    pub fn of(record: &RunRecord, seq: u64) -> Self {
        Self {
            seq,
            run: record.id.clone(),
            state: record.state,
            conclusion: record.conclusion,
            log_records: record.log_records,
            tree: record.tree.clone(),
        }
    }

    /// Log growth alone is not a status transition.
    pub fn same_status(&self, other: &Self) -> bool {
        if self.state != other.state || self.conclusion != other.conclusion {
            return false;
        }
        let mut left = self.tree.clone();
        let mut right = other.tree.clone();
        for tree in [&mut left, &mut right] {
            tree.malformed_lines = 0;
            for group in &mut tree.groups {
                for job in &mut group.jobs {
                    for section in &mut job.sections {
                        section.first_seq = None;
                        section.last_seq = None;
                    }
                }
            }
        }
        left == right
    }
}

pub fn path(store: &Store, run: &str) -> std::path::PathBuf {
    store.run_dir(run).join("status.jsonl")
}

/// Remove an incomplete crash-tail and return the last durable cursor.
pub fn repair_and_last(store: &Store, run: &str) -> io::Result<Option<StatusSnapshot>> {
    let path = path(store, run);
    if !path.exists() {
        return Ok(None);
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    if complete != bytes.len() {
        file.set_len(complete as u64)?;
        file.sync_data()?;
    }
    let last = bytes[..complete]
        .rsplit(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_slice::<StatusSnapshot>(line).ok());
    Ok(last)
}

pub fn append(store: &Store, record: &RunRecord, seq: u64) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(&StatusSnapshot::of(record, seq)).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(store, &record.id))?;
    file.write_all(&bytes)?;
    file.sync_data()
}

/// Bounded replay after a cursor. An unfinished tail is ignored until fixed.
pub struct StatusPage {
    pub records: Vec<StatusSnapshot>,
    pub next_offset: u64,
}

pub fn read_since(
    store: &Store,
    run: &str,
    since: u64,
    limit: usize,
) -> io::Result<Vec<StatusSnapshot>> {
    Ok(read_since_offset(store, run, since, limit, 0)?.records)
}

/// Continue from a byte offset so idle polling never rescans old snapshots.
pub fn read_since_offset(
    store: &Store,
    run: &str,
    since: u64,
    limit: usize,
    offset: u64,
) -> io::Result<StatusPage> {
    let mut file = File::open(path(store, run))?;
    let mut next_offset = if offset > file.metadata()?.len() {
        0
    } else {
        offset
    };
    file.seek(SeekFrom::Start(next_offset))?;
    let mut page = Vec::new();
    let mut reader = io::BufReader::new(file);
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 || !line.ends_with(b"\n") {
            break;
        }
        next_offset += line.len() as u64;
        line.pop();
        if let Ok(snapshot) = serde_json::from_slice::<StatusSnapshot>(&line)
            && snapshot.seq > since
        {
            page.push(snapshot);
            if page.len() >= limit {
                break;
            }
        }
    }
    Ok(StatusPage {
        records: page,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::tests::sample_record;

    #[test]
    fn replay_survives_restart_and_repairs_a_crash_tail() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        let record = sample_record("run-a");
        std::fs::create_dir_all(store.run_dir(&record.id)).unwrap();
        append(&store, &record, 1).unwrap();
        append(&store, &record, 2).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(path(&store, &record.id))
            .unwrap()
            .write_all(b"{\"seq\":3")
            .unwrap();
        assert_eq!(repair_and_last(&store, &record.id).unwrap().unwrap().seq, 2);
        let replay = read_since(&store, &record.id, 1, 10).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, 2);
        let first = read_since_offset(&store, &record.id, 0, 1, 0).unwrap();
        assert_eq!(first.records[0].seq, 1);
        let second = read_since_offset(&store, &record.id, 1, 1, first.next_offset).unwrap();
        assert_eq!(second.records[0].seq, 2);
        append(&store, &record, 3).unwrap();
        let third = read_since_offset(&store, &record.id, 2, 1, second.next_offset).unwrap();
        assert_eq!(third.records[0].seq, 3);
    }
}
