//! On-disk CI state under `<state>/ci`: run records, seq-ordered logs,
//! frozen sources, staging and runner settings. Only files; no scheduling.
//!
//! ```text
//! ci/staging/<uuid>/source/   client-written snapshot awaiting submission
//! ci/runs/<run>/run.json      durable run record (rewritten atomically)
//! ci/runs/<run>/log.jsonl     seq-ordered log records
//! ci/runs/<run>/status.jsonl  replayable status snapshots
//! ci/runs/<run>/event.json    provider event payload
//! ci/runs/<run>/source/       frozen source snapshot (kept for retry)
//! ci/settings.json            runner settings (limit, drain)
//! ```

use std::{
    collections::VecDeque,
    io::{self, BufRead, Seek, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{
    model::{LogRecord, truncate_text},
    wire::RunRecord,
};

/// One `log.jsonl` line is indexed every this many records.
pub const INDEX_STRIDE: u64 = 128;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub limit: usize,
    pub drained: bool,
}

/// Which log records a page returns.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogFilter<'a> {
    pub job: Option<&'a str>,
    pub section: Option<&'a str>,
}
impl LogFilter<'_> {
    fn matches(&self, record: &LogRecord) -> bool {
        self.job.is_none_or(|j| record.job.as_deref() == Some(j))
            && self
                .section
                .is_none_or(|s| record.section.as_deref() == Some(s))
    }
}

/// A bounded read after a cursor.
#[derive(Clone, Copy, Debug)]
pub struct LogQuery<'a> {
    pub since: u64,
    /// Highest seq published to readers (later lines may be half-written).
    pub visible: u64,
    pub filter: LogFilter<'a>,
    pub limit: usize,
    pub max_bytes: usize,
}

/// One bounded page of log records after a cursor.
#[derive(Clone, Debug, Default)]
pub struct LogPage {
    pub records: Vec<LogRecord>,
    /// Cursor for the next call: the last seq scanned (matching or not).
    pub next_seq: u64,
    /// True when the page stopped at a size or count cap.
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join("ci"),
        }
    }

    /// Create the layout and drop staging copies abandoned by clients that
    /// died mid-submit (no daemon request can still name them).
    pub fn open(state_dir: &Path) -> io::Result<Self> {
        let store = Self::new(state_dir);
        std::fs::create_dir_all(store.root.join("runs"))?;
        let staging = store.staging_root();
        std::fs::create_dir_all(&staging)?;
        for entry in std::fs::read_dir(&staging)?.flatten() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
        Ok(store)
    }

    pub fn staging_root(&self) -> PathBuf {
        self.root.join("staging")
    }
    pub fn staging(&self, id: &str) -> PathBuf {
        self.staging_root().join(id)
    }
    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.root.join("runs").join(id)
    }
    pub fn source(&self, id: &str) -> PathBuf {
        self.run_dir(id).join("source")
    }
    /// bosn's rewrites of the run's workflow files (#424), mirroring
    /// [`Self::source`]; act reads them through `--workflow-overlay`, so the
    /// snapshot every job checks out stays the tree under test.
    pub fn overlay(&self, id: &str) -> PathBuf {
        overlay_beside(&self.source(id))
    }
    pub fn event(&self, id: &str) -> PathBuf {
        self.run_dir(id).join("event.json")
    }
    fn log_path(&self, id: &str) -> PathBuf {
        self.run_dir(id).join("log.jsonl")
    }

    pub fn load_settings(&self) -> Option<Settings> {
        std::fs::read(self.root.join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }
    pub fn save_settings(&self, settings: &Settings) {
        logged(
            "runner settings",
            write_atomic(
                &self.root.join("settings.json"),
                &serde_json::to_vec(settings).unwrap_or_default(),
            ),
        );
    }

    /// Every readable run record, oldest first.
    pub fn load_runs(&self) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = std::fs::read_dir(self.root.join("runs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read(e.path().join("run.json")).ok())
            .filter_map(|b| serde_json::from_slice(&b).ok())
            .collect();
        runs.sort_by(|a, b| a.created_at.total_cmp(&b.created_at));
        runs
    }

    pub fn save_run(&self, record: &RunRecord) {
        let dir = self.run_dir(&record.id);
        let saved = std::fs::create_dir_all(&dir).and_then(|()| {
            write_atomic(
                &dir.join("run.json"),
                &serde_json::to_vec_pretty(record).unwrap_or_default(),
            )
        });
        logged("run record", saved);
    }

    /// Move a staged snapshot into a new run directory with its payload.
    pub fn place(&self, run: &str, staging: &Path, payload: &[u8]) -> io::Result<()> {
        std::fs::create_dir_all(self.run_dir(run))?;
        std::fs::rename(staging.join("source"), self.source(run))?;
        let _ = std::fs::remove_dir_all(staging);
        std::fs::write(self.event(run), payload)
    }

    /// Copy a finished run's frozen source into a fresh staging directory.
    pub fn stage_copy(&self, run: &str, staging_id: &str) -> io::Result<PathBuf> {
        let staging = self.staging(staging_id);
        if let Err(error) = copy_tree(&self.source(run), &staging.join("source")) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        Ok(staging)
    }

    pub fn remove_run(&self, run: &str) {
        let _ = std::fs::remove_dir_all(self.run_dir(run));
    }
    pub fn remove_source(&self, run: &str) {
        let _ = std::fs::remove_dir_all(self.source(run));
    }
    pub fn run_bytes(&self, run: &str) -> u64 {
        dir_size(&self.run_dir(run))
    }

    pub fn log_writer(&self, run: &str) -> Option<LogWriter> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(run))
            .ok()
            .map(|file| LogWriter {
                out: io::BufWriter::new(file),
                offset: 0,
                lost: 0,
            })
    }

    /// Records after `query.since`, scanning from byte `offset` (an index
    /// entry at or before the cursor).
    pub fn read_log(&self, run: &str, offset: u64, query: &LogQuery<'_>) -> LogPage {
        let mut page = LogPage {
            next_seq: query.since,
            ..LogPage::default()
        };
        let mut bytes = 0;
        for (record, size) in self.scan(run, offset) {
            if record.seq <= query.since {
                continue;
            }
            if record.seq > query.visible {
                break;
            }
            if query.filter.matches(&record) {
                if page.records.len() >= query.limit || bytes + size > query.max_bytes {
                    if !page.records.is_empty() {
                        page.truncated = true;
                        break;
                    }
                    // A record larger than the whole page is cut down rather
                    // than skipped, so the cursor always advances.
                    let mut record = record.clone();
                    truncate_text(&mut record.text, query.max_bytes / 2);
                    page.records.push(record);
                    page.next_seq = page.records[0].seq;
                    page.truncated = true;
                    break;
                }
                bytes += size;
                page.records.push(record.clone());
            }
            page.next_seq = record.seq;
        }
        page
    }

    /// The last `n` texts of the records matching `filter`.
    pub fn tail(&self, run: &str, filter: LogFilter<'_>, n: usize) -> Vec<String> {
        let mut ring = VecDeque::with_capacity(n);
        for (record, _) in self.scan(run, 0).filter(|(r, _)| filter.matches(r)) {
            if ring.len() == n {
                ring.pop_front();
            }
            let mut text = record.text;
            truncate_text(&mut text, 2048);
            ring.push_back(text);
        }
        ring.into()
    }

    fn scan(&self, run: &str, offset: u64) -> impl Iterator<Item = (LogRecord, usize)> {
        let reader = std::fs::File::open(self.log_path(run)).ok().map(|file| {
            let mut reader = io::BufReader::new(file);
            let _ = reader.seek(io::SeekFrom::Start(offset));
            reader
        });
        reader.into_iter().flat_map(|reader| {
            reader
                .split(b'\n')
                .map_while(Result::ok)
                .filter_map(|line| {
                    let size = line.len() + 1;
                    serde_json::from_slice(&line).ok().map(|r| (r, size))
                })
        })
    }
}

/// Appends log records, tracking byte offsets for the sparse seq index.
pub struct LogWriter {
    out: io::BufWriter<std::fs::File>,
    offset: u64,
    /// Records that could not be written (their seqs are missing).
    lost: u64,
}
impl LogWriter {
    /// Append one record; returns its byte offset, or `None` if it was lost.
    pub fn append(&mut self, record: &LogRecord) -> Option<u64> {
        let at = self.offset;
        let mut line = serde_json::to_string(record).ok()?;
        line.push('\n');
        if self.out.write_all(line.as_bytes()).is_err() {
            self.lost += 1;
            return None;
        }
        self.offset += line.len() as u64;
        Some(at)
    }
    pub fn flush(&mut self) {
        if self.out.flush().is_err() {
            self.lost += 1;
        }
    }
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

/// State writes are best effort for the run itself, but never silent.
fn logged(what: &str, result: io::Result<()>) {
    if let Err(error) = result {
        eprintln!("bosn ci: could not write the {what}: {error}");
    }
}

/// Write, fsync, then rename, so a crash leaves the old or the new file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)
}

fn dir_size(path: &Path) -> u64 {
    kernal_api::platform::fs::DirectoryWalk::new(path.to_path_buf())
        .walk()
        .filter_map(Result::ok)
        .filter(|e| e.is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            kernal_api::platform::fs::copy_file(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The overlay directory that belongs to a run's `source` directory.
pub fn overlay_beside(source: &Path) -> PathBuf {
    source.with_file_name("overlay")
}
