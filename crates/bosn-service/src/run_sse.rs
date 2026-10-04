//! Pull-driven SSE tail of the durable run index. No output queue is spawned.

use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::stream;
use kernal_api::{async_engine, http_server};
use serde_json::json;

use crate::raw_run_log::{self, RawChunk};

struct Tail {
    state_dir: PathBuf,
    run_id: String,
    cursor: u64,
    index_cursor: raw_run_log::IndexCursor,
    streams: String,
    pending: VecDeque<RawChunk>,
    done: bool,
    gap_retries: u8,
}

pub(crate) fn response(
    state_dir: &Path,
    run_id: &str,
    from_seq: u64,
    streams: &str,
) -> http_server::Response {
    let state = Tail {
        state_dir: state_dir.to_path_buf(),
        run_id: run_id.into(),
        cursor: from_seq,
        index_cursor: raw_run_log::IndexCursor::default(),
        streams: streams.into(),
        pending: VecDeque::new(),
        done: false,
        gap_retries: 0,
    };
    let events = stream::unfold(state, |mut tail| async move {
        loop {
            if tail.done {
                return None;
            }
            if let Some(chunk) = tail.pending.pop_front() {
                tail.cursor = chunk.index.seq;
                if tail.streams != "stdout,stderr" && tail.streams != chunk.index.stream {
                    continue;
                }
                let event = http_server::SseEvent {
                    id: Some(tail.cursor.to_string()),
                    event: Some(chunk.index.stream.clone()),
                    data: crate::run_http::chunk_json(&chunk).to_string(),
                };
                return Some((Ok(event), tail));
            }
            let state_dir = tail.state_dir.clone();
            let run_id = tail.run_id.clone();
            let cursor = tail.cursor;
            let index_cursor = tail.index_cursor;
            let read = async_engine::launch_blocking(move || {
                Ok::<_, io::Error>((
                    raw_run_log::read_since_indexed(
                        &state_dir,
                        &run_id,
                        cursor,
                        256,
                        index_cursor,
                    )?,
                    raw_run_log::read_end(&state_dir, &run_id)?,
                ))
            })
            .await;
            let ((chunks, index_cursor), end) = match read {
                Ok(Ok(page)) => page,
                Ok(Err(error)) => {
                    tail.done = true;
                    return Some((Err(error), tail));
                }
                Err(error) => {
                    tail.done = true;
                    return Some((Err(io::Error::other(error)), tail));
                }
            };
            tail.index_cursor = index_cursor;
            if !chunks.is_empty() {
                tail.gap_retries = 0;
                tail.pending = chunks.into();
                continue;
            }
            if let Some(end) = end {
                if end.seq <= tail.cursor {
                    return None;
                }
                if end.seq != tail.cursor.saturating_add(1) {
                    tail.gap_retries = tail.gap_retries.saturating_add(1);
                    if tail.gap_retries < 4 {
                        async_engine::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    tail.done = true;
                    return Some((
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "run end sequence gap",
                        )),
                        tail,
                    ));
                }
                tail.done = true;
                let event = http_server::SseEvent {
                    id: Some(end.seq.to_string()),
                    event: Some("end".into()),
                    data: json!({
                        "state": end.state,
                        "ended_unix_ms": end.ended_unix_ms,
                        "exit_code": end.exit_code,
                    })
                    .to_string(),
                };
                return Some((Ok(event), tail));
            }
            async_engine::sleep(Duration::from_millis(250)).await;
        }
    });
    http_server::Response::sse_stream(events, Duration::from_secs(15)).unwrap_or_default()
}
