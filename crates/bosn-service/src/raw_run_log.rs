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

pub struct RawRunLog {
    root: PathBuf,
    stdout: File,
    stderr: File,
    index: File,
    next_seq: u64,
    stdout_len: u64,
    stderr_len: u64,
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

    pub fn root(&self) -> &Path {
        &self.root
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
    let mut previous: u64 = 0;
    let mut index_reader = BufReader::new(File::open(root.join("index.jsonl"))?);
    loop {
        let mut line = Vec::new();
        let read = index_reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let index: ChunkIndex = serde_json::from_slice(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if Some(index.seq) != previous.checked_add(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-contiguous run sequence",
            ));
        }
        previous = index.seq;
        if index.seq <= from_seq {
            continue;
        }
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
        if chunks.len() == limit {
            break;
        }
    }
    Ok(chunks)
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
