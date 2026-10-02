//! The live run feed: a lossy broadcast of run state changes.
//!
//! Publishing never waits for readers. A reader that falls behind loses the
//! oldest events and is told to resync (`RunEvent::Resync`), then refetches
//! state over the typed API; a slow browser can never hold the daemon back.

use kernal_api::async_engine::{BroadcastReceiver, BroadcastSender, broadcast_channel};
use serde::{Deserialize, Serialize};

use super::{
    reply::{JobCounts, RunView},
    wire::{Conclusion, RunRecord, RunState},
};

/// Events retained per reader before the oldest are dropped.
pub const FEED_CAPACITY: usize = 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// A run's state, conclusion or progress changed.
    Run {
        run: String,
        state: RunState,
        conclusion: Option<Conclusion>,
        log_records: u64,
        jobs: JobCounts,
        actor: String,
        workspace: String,
    },
    /// This reader missed `skipped` events: refetch everything.
    Resync { skipped: u64 },
}

impl RunEvent {
    pub fn of(record: &RunRecord) -> Self {
        let view = RunView::of(record.clone(), false);
        Self::Run {
            run: record.id.clone(),
            state: record.state,
            conclusion: record.conclusion,
            log_records: record.log_records,
            jobs: view.jobs,
            actor: record.actor.clone(),
            workspace: record.workspace.clone(),
        }
    }
}

/// The daemon side of the feed. Cheap to clone.
#[derive(Clone)]
pub struct Feed {
    sender: BroadcastSender<RunEvent>,
}

impl Feed {
    pub fn new() -> Self {
        let (sender, _) =
            broadcast_channel(FEED_CAPACITY).expect("feed capacity is a power of two");
        Self { sender }
    }

    /// Never blocks; with no readers the event is simply dropped.
    pub fn publish(&self, record: &RunRecord) {
        let _ = self.sender.send(RunEvent::of(record));
    }

    pub fn subscribe(&self) -> BroadcastReceiver<RunEvent> {
        self.sender.subscribe()
    }
}

impl Default for Feed {
    fn default() -> Self {
        Self::new()
    }
}
