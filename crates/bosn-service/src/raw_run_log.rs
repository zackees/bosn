//! Durable, byte-preserving task output. The bounded job log is a view of this
//! data; it must never be the source used for replay.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use bosn_engine::EngineEvent;
use kernal_api::async_engine;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChunkIndex {
    pub seq: u64,
    pub ts_unix_ms: u64,
    pub stream: String,
    pub offset: u64,
    pub len: u64,
    pub job_id: Option<String>,
    pub step_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RawChunk {
    pub index: ChunkIndex,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
pub struct IndexCursor {
    offset: u64,
    previous: u64,
}

pub struct RawRunLog {
    root: PathBuf,
    stdout: File,
    stderr: File,
    index: File,
    next_seq: u64,
    stdout_len: u64,
    stderr_len: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunMetadata {
    pub run_id: String,
    pub job_id: u64,
    #[serde(default)]
    pub task: Option<String>,
    pub created_unix_ms: u64,
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub ended_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunEnd {
    pub state: String,
    pub ended_unix_ms: u64,
    pub seq: u64,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Clone)]
pub struct JobLogSink {
    sender: async_engine::Sender<String>,
    raw: Option<Arc<Mutex<RawRunLog>>>,
}

impl JobLogSink {
    pub fn transient(sender: async_engine::Sender<String>) -> Self {
        Self { sender, raw: None }
    }

    pub fn durable(sender: async_engine::Sender<String>, raw: RawRunLog) -> Self {
        Self {
            sender,
            raw: Some(Arc::new(Mutex::new(raw))),
        }
    }

    pub fn append(&self, event: &EngineEvent) -> Result<(), String> {
        if let Some(raw) = &self.raw {
            raw.lock()
                .map_err(|_| "raw run log lock poisoned".to_owned())?
                .append(event)
                .map_err(|error| format!("raw run log write failed: {error}"))?;
        }
        Ok(())
    }

    pub fn finish(&self, state: &str) -> io::Result<()> {
        if let Some(raw) = &self.raw {
            raw.lock()
                .map_err(|_| io::Error::other("raw run log lock poisoned"))?
                .finish(state)?;
        }
        Ok(())
    }
}

impl std::ops::Deref for JobLogSink {
    type Target = async_engine::Sender<String>;

    fn deref(&self) -> &Self::Target {
        &self.sender
    }
}

impl RawRunLog {
    pub fn create(state_dir: &Path, run_id: &str) -> io::Result<Self> {
        if !crate::ci::wire::valid_uuid(run_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid run ID",
            ));
        }
        let root = state_dir.join("runs").join(run_id);
        fs::create_dir_all(&root)?;
        // A task can contain private output even when no named secret was
        // declared. Tighten an existing directory too, before opening files.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let stdout = private_create(&root.join("stdout.log"))?;
        let stderr = private_create(&root.join("stderr.log"))?;
        let index = private_create(&root.join("index.jsonl"))?;
        Ok(Self {
            root,
            stdout,
            stderr,
            index,
            next_seq: 1,
            stdout_len: 0,
            stderr_len: 0,
        })
    }

    /// Associate a durable output directory with its daemon job. This is
    /// written once, before any task output, so discovery survives restart.
    pub fn write_metadata(&self, run_id: &str, job_id: u64, task: Option<&str>) -> io::Result<()> {
        let metadata = RunMetadata {
            run_id: run_id.to_owned(),
            job_id,
            task: task.map(str::to_owned),
            created_unix_ms: now_unix_ms(),
            schema_version: 1,
            state: Some("running".into()),
            ended_unix_ms: None,
        };
        let next = self.root.join(".run.next");
        let mut file = private_create(&next)?;
        serde_json::to_writer(&mut file, &metadata)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        kernal_api::platform::fs::replacement::atomic_replace(&next, &self.root.join("run.json"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn finish(&self, state: &str) -> io::Result<()> {
        write_end_at_seq(&self.root, state, self.next_seq)
    }

    pub fn append(&mut self, event: &EngineEvent) -> io::Result<Option<ChunkIndex>> {
        let (stream, bytes, file, offset) = match event {
            EngineEvent::Stdout(bytes) => ("stdout", bytes, &mut self.stdout, &mut self.stdout_len),
            EngineEvent::Stderr(bytes) => ("stderr", bytes, &mut self.stderr, &mut self.stderr_len),
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        let record = ChunkIndex {
            seq: self.next_seq,
            ts_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            stream: stream.into(),
            offset: *offset,
            len: bytes.len().try_into().unwrap_or(u64::MAX),
            job_id: None,
            step_id: None,
        };
        // Write data first. A torn index entry can be discarded at recovery;
        // an index entry pointing beyond a byte file would be unreplayable.
        file.write_all(bytes)?;
        file.flush()?;
        serde_json::to_writer(&mut self.index, &record)?;
        self.index.write_all(b"\n")?;
        self.index.flush()?;
        *offset = offset.saturating_add(record.len);
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(Some(record))
    }
}

pub fn list_runs(state_dir: &Path) -> io::Result<Vec<RunMetadata>> {
    let root = state_dir.join("runs");
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !crate::ci::wire::valid_uuid(name) || !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("run.json");
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata: RunMetadata = match serde_json::from_reader(file) {
            Ok(metadata) => metadata,
            Err(error) if !error.is_io() => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.run_id == name {
            let mut metadata = metadata;
            match File::open(entry.path().join("end.json")) {
                Ok(file) => {
                    let end: RunEnd = match serde_json::from_reader(file) {
                        Ok(end) => end,
                        Err(error) if !error.is_io() => continue,
                        Err(error) => return Err(error.into()),
                    };
                    metadata.state = Some(end.state);
                    metadata.ended_unix_ms = Some(end.ended_unix_ms);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            runs.push(metadata);
        }
    }
    runs.sort_by_key(|run| (run.created_unix_ms, run.run_id.clone()));
    Ok(runs)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn write_end_at_seq(root: &Path, state: &str, seq: u64) -> io::Result<()> {
    let end = RunEnd {
        state: state.into(),
        ended_unix_ms: now_unix_ms(),
        seq,
        exit_code: (state == "success").then_some(0),
    };
    let next = root.join(format!(
        ".end.{}.{}.next",
        std::process::id(),
        now_unix_ms()
    ));
    let mut file = private_create(&next)?;
    serde_json::to_writer(&mut file, &end)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    kernal_api::platform::fs::replacement::atomic_replace(&next, &root.join("end.json"))
}

/// Previous daemon instance's active runs cannot continue after restart.
pub fn mark_interrupted_runs(state_dir: &Path) -> io::Result<()> {
    for run in list_runs(state_dir)? {
        if run.schema_version == 1 && run.ended_unix_ms.is_none() {
            let root = state_dir.join("runs").join(&run.run_id);
            write_end_at_seq(&root, "interrupted", last_seq(&root)?.saturating_add(1))?;
        }
    }
    Ok(())
}

fn last_seq(root: &Path) -> io::Result<u64> {
    let mut last = 0;
    let mut reader = BufReader::new(File::open(root.join("index.jsonl"))?);
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let index: ChunkIndex = serde_json::from_slice(&line)?;
        last = index.seq;
    }
    Ok(last)
}

pub fn read_end(state_dir: &Path, run_id: &str) -> io::Result<Option<RunEnd>> {
    if !crate::ci::wire::valid_uuid(run_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid run ID",
        ));
    }
    match File::open(state_dir.join("runs").join(run_id).join("end.json")) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Read at most `limit` committed chunks after `from_seq`, including after
/// daemon restart. Call again with the last returned sequence to page onward.
/// The index is appended only after its channel bytes, so it is the commit
/// record. A partial final index line from an interrupted write is ignored.
pub fn read_since(
    state_dir: &Path,
    run_id: &str,
    from_seq: u64,
    limit: usize,
) -> io::Result<Vec<RawChunk>> {
    read_since_indexed(state_dir, run_id, from_seq, limit, IndexCursor::default())
        .map(|(chunks, _)| chunks)
}

/// Resume a tail from its last consumed index byte. The first call starts at
/// zero; later calls only read newly appended index lines.
pub fn read_since_indexed(
    state_dir: &Path,
    run_id: &str,
    from_seq: u64,
    limit: usize,
    mut cursor: IndexCursor,
) -> io::Result<(Vec<RawChunk>, IndexCursor)> {
    if !(1..=256).contains(&limit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid page limit",
        ));
    }
    if !crate::ci::wire::valid_uuid(run_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid run ID",
        ));
    }
    let root = state_dir.join("runs").join(run_id);
    let mut stdout = File::open(root.join("stdout.log"))?;
    let mut stderr = File::open(root.join("stderr.log"))?;
    let mut chunks = Vec::new();
    let mut page_bytes = 0_usize;
    let mut index_reader = BufReader::new(File::open(root.join("index.jsonl"))?);
    index_reader.seek(SeekFrom::Start(cursor.offset))?;
    loop {
        let mut line = Vec::new();
        let read = index_reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let index: ChunkIndex = serde_json::from_slice(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if Some(index.seq) != cursor.previous.checked_add(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-contiguous run sequence",
            ));
        }
        let seq = index.seq;
        if seq > from_seq {
            let file = match index.stream.as_str() {
                "stdout" => &mut stdout,
                "stderr" => &mut stderr,
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid stream")),
            };
            let len = usize::try_from(index.len)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk too large"))?;
            if len > 64 * 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunk too large",
                ));
            }
            if !chunks.is_empty() && page_bytes.saturating_add(len) > 4 * 1024 * 1024 {
                break;
            }
            file.seek(SeekFrom::Start(index.offset))?;
            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes)?;
            page_bytes = page_bytes.saturating_add(len);
            chunks.push(RawChunk { index, bytes });
        }
        cursor.previous = seq;
        cursor.offset = index_reader.stream_position()?;
        if chunks.len() == limit {
            break;
        }
    }
    Ok((chunks, cursor))
}

fn private_create(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::async_engine::RuntimeBuilder;

    #[test]
    fn raw_bytes_and_cross_stream_sequence_survive_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log =
            RawRunLog::create(tmp.path(), "00000000-0000-0000-0000-000000000042").unwrap();
        assert!(log.append(&EngineEvent::Stdout(vec![])).unwrap().is_none());
        let a = log
            .append(&EngineEvent::Stdout(vec![0xff, 0, b'a']))
            .unwrap()
            .unwrap();
        let b = log
            .append(&EngineEvent::Stderr(vec![0xfe, b'b']))
            .unwrap()
            .unwrap();
        let c = log
            .append(&EngineEvent::Stdout(vec![b'c'; 16 * 1024]))
            .unwrap()
            .unwrap();
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert_eq!((a.offset, b.offset, c.offset), (0, 0, 3));
        let root = log.root().to_path_buf();
        drop(log);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for name in ["stdout.log", "stderr.log", "index.jsonl"] {
                assert_eq!(
                    fs::metadata(root.join(name)).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        let stdout = fs::read(root.join("stdout.log")).unwrap();
        assert_eq!(&stdout[..3], &[0xff, 0, b'a']);
        assert_eq!(&stdout[3..], &[b'c'; 16 * 1024]);
        assert_eq!(fs::read(root.join("stderr.log")).unwrap(), [0xfe, b'b']);
        let lines = fs::read_to_string(root.join("index.jsonl")).unwrap();
        let replay: Vec<serde_json::Value> = lines
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[1]["stream"], "stderr");
        assert_eq!(replay[2]["len"], 16 * 1024);
        let resumed = read_since(tmp.path(), "00000000-0000-0000-0000-000000000042", 1, 1).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].bytes, [0xfe, b'b']);
        let resumed = read_since(tmp.path(), "00000000-0000-0000-0000-000000000042", 2, 1).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].bytes, [b'c'; 16 * 1024]);
    }

    #[test]
    fn run_metadata_survives_restart_and_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = "00000000-0000-0000-0000-000000000042";
        let log = RawRunLog::create(tmp.path(), run_id).unwrap();
        log.write_metadata(run_id, 123, None).unwrap();
        drop(log);
        OpenOptions::new()
            .append(true)
            .open(tmp.path().join("runs").join(run_id).join("index.jsonl"))
            .unwrap()
            .write_all(b"{\"seq\":")
            .unwrap();
        let runs = list_runs(tmp.path()).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, run_id);
        assert_eq!(runs[0].job_id, 123);
        assert!(runs[0].created_unix_ms > 0);
        assert_eq!(runs[0].state.as_deref(), Some("running"));
        mark_interrupted_runs(tmp.path()).unwrap();
        let interrupted = list_runs(tmp.path()).unwrap();
        assert_eq!(interrupted[0].state.as_deref(), Some("interrupted"));
        assert!(interrupted[0].ended_unix_ms.is_some());
        assert_eq!(read_end(tmp.path(), run_id).unwrap().unwrap().seq, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(tmp.path().join("runs").join(run_id).join("run.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn finished_run_stays_finished_after_daemon_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = "00000000-0000-0000-0000-000000000043";
        let mut log = RawRunLog::create(tmp.path(), run_id).unwrap();
        log.write_metadata(run_id, 124, None).unwrap();
        log.append(&EngineEvent::Stdout(b"done".to_vec())).unwrap();
        log.finish("success").unwrap();
        mark_interrupted_runs(tmp.path()).unwrap();
        let runs = list_runs(tmp.path()).unwrap();
        assert_eq!(runs[0].state.as_deref(), Some("success"));
        assert!(runs[0].ended_unix_ms.is_some());
        assert_eq!(read_end(tmp.path(), run_id).unwrap().unwrap().seq, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(tmp.path().join("runs").join(run_id).join("end.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn damaged_historical_metadata_does_not_block_run_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let good = "00000000-0000-0000-0000-000000000044";
        let bad_run = "00000000-0000-0000-0000-000000000045";
        let bad_end = "00000000-0000-0000-0000-000000000046";
        for (id, job) in [(good, 1), (bad_run, 2), (bad_end, 3)] {
            let log = RawRunLog::create(tmp.path(), id).unwrap();
            log.write_metadata(id, job, None).unwrap();
        }
        fs::write(tmp.path().join("runs").join(bad_run).join("run.json"), b"{").unwrap();
        fs::write(tmp.path().join("runs").join(bad_end).join("end.json"), b"{").unwrap();
        mark_interrupted_runs(tmp.path()).unwrap();
        let runs = list_runs(tmp.path()).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, good);
        assert_eq!(runs[0].state.as_deref(), Some("interrupted"));
        assert!(read_end(tmp.path(), bad_run).unwrap().is_none());
        assert_eq!(
            fs::read(tmp.path().join("runs").join(bad_end).join("end.json")).unwrap(),
            b"{"
        );
    }

    #[test]
    fn indexed_tail_resumes_at_last_consumed_line() {
        let tmp = tempfile::tempdir().unwrap();
        let run = "00000000-0000-0000-0000-000000000047";
        let mut log = RawRunLog::create(tmp.path(), run).unwrap();
        for _ in 0..300 {
            log.append(&EngineEvent::Stdout(b"x".to_vec())).unwrap();
        }
        let (first, cursor) =
            read_since_indexed(tmp.path(), run, 0, 256, IndexCursor::default()).unwrap();
        assert_eq!(first.len(), 256);
        assert_eq!(cursor.previous, 256);
        let (second, cursor) = read_since_indexed(tmp.path(), run, 256, 256, cursor).unwrap();
        assert_eq!(second.len(), 44);
        assert_eq!(cursor.previous, 300);
        let (empty, idle_cursor) = read_since_indexed(tmp.path(), run, 300, 256, cursor).unwrap();
        assert!(empty.is_empty());
        assert_eq!(idle_cursor.offset, cursor.offset);
        log.append(&EngineEvent::Stderr(b"new".to_vec())).unwrap();
        let (new, cursor) = read_since_indexed(tmp.path(), run, 300, 256, idle_cursor).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].bytes, b"new");
        assert_eq!(cursor.previous, 301);
    }

    #[test]
    fn engine_forwarder_keeps_exact_bytes_before_the_text_view() {
        let tmp = tempfile::tempdir().unwrap();
        let run = "00000000-0000-0000-0000-000000000043";
        let raw = RawRunLog::create(tmp.path(), run).unwrap();
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, mut receiver) = async_engine::channel(8);
            let sink = JobLogSink::durable(sender, raw);
            crate::task_executors::forward_engine_event(
                &sink,
                EngineEvent::Stdout(vec![0xff, b'a']),
            )
            .await
            .unwrap();
            crate::task_executors::forward_engine_event(
                &sink,
                EngineEvent::Stderr(vec![0xfe, b'b']),
            )
            .await
            .unwrap();
            assert!(receiver.recv().await.unwrap().starts_with("[stdout] "));
            assert!(receiver.recv().await.unwrap().starts_with("[stderr] "));
        });
        let chunks = read_since(tmp.path(), run, 0, 256).unwrap();
        assert_eq!(chunks[0].bytes, [0xff, b'a']);
        assert_eq!(chunks[1].bytes, [0xfe, b'b']);
    }
}
