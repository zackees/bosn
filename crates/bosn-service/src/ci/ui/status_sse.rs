//! Pull-driven replay of persisted per-run status changes.

use std::{collections::VecDeque, io, time::Duration};

use futures_util::stream;
use kernal_api::{async_engine, http_server};

use crate::ci::{
    CiRuntime,
    status::{self, StatusSnapshot},
    store::Store,
    wire::RunState,
};

struct Tail {
    store: Store,
    run: String,
    cursor: u64,
    offset: u64,
    pending: VecDeque<StatusSnapshot>,
    done: bool,
}

pub(super) fn response(ci: &CiRuntime, run: &str, from_seq: u64) -> http_server::Response {
    if ci.record(run).is_err() {
        return super::text(404, "run not found");
    }
    let state = Tail {
        store: ci.status_store(),
        run: run.into(),
        cursor: from_seq,
        offset: 0,
        pending: VecDeque::new(),
        done: false,
    };
    let runtime = ci.clone();
    let events = stream::unfold(state, move |mut tail| {
        let runtime = runtime.clone();
        async move {
            loop {
                if tail.done {
                    return None;
                }
                if let Some(snapshot) = tail.pending.pop_front() {
                    tail.cursor = snapshot.seq;
                    tail.done = snapshot.state == RunState::Done;
                    let event = http_server::SseEvent {
                        id: Some(snapshot.seq.to_string()),
                        event: Some("status".into()),
                        data: serde_json::to_string(&snapshot).unwrap_or_default(),
                    };
                    return Some((Ok(event), tail));
                }
                let (store, run, cursor, offset) = (
                    tail.store.clone(),
                    tail.run.clone(),
                    tail.cursor,
                    tail.offset,
                );
                let page = async_engine::launch_blocking(move || {
                    status::read_since_offset(&store, &run, cursor, 64, offset)
                })
                .await;
                match page {
                    Ok(Ok(page)) => {
                        tail.offset = page.next_offset;
                        if !page.records.is_empty() {
                            tail.pending = page.records.into();
                            continue;
                        }
                        if let Ok(record) = runtime.record(&tail.run)
                            && record.state == RunState::Done
                        {
                            match runtime.reconcile_status(record.id).await {
                                Ok((true, _)) => continue,
                                Ok((false, RunState::Done)) => return None,
                                Ok((false, _)) => {
                                    async_engine::sleep(Duration::from_millis(250)).await;
                                }
                                Err(error) => return Some((Err(error), tail)),
                            }
                        }
                        async_engine::sleep(Duration::from_millis(250)).await;
                    }
                    Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                        async_engine::sleep(Duration::from_millis(250)).await;
                    }
                    Ok(Err(error)) => return Some((Err(error), tail)),
                    Err(error) => return Some((Err(io::Error::other(error)), tail)),
                }
            }
        }
    });
    http_server::Response::sse_stream(events, super::SSE_KEEPALIVE)
        .unwrap_or_else(|_| super::text(500, "status stream unavailable"))
}
