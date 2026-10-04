//! Validated act timestamps on job and section status transitions.

use super::{ActResult, ItemConclusion, ItemStatus, Job, Section};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

fn valid_timestamp(value: Option<&str>) -> Option<String> {
    let value = value?;
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|_| value.to_owned())
}

fn elapsed_ms(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let start = OffsetDateTime::parse(start?, &Rfc3339).ok()?;
    let end = OffsetDateTime::parse(end?, &Rfc3339).ok()?;
    u64::try_from((end - start).whole_milliseconds()).ok()
}

impl Job {
    pub(super) fn observe_start(&mut self, time: Option<&str>) {
        if self.started_at.is_none() {
            self.started_at = valid_timestamp(time);
        }
    }

    pub(super) fn observe_end(&mut self, time: Option<&str>) {
        self.completed_at = valid_timestamp(time);
        self.duration_ms = elapsed_ms(self.started_at.as_deref(), self.completed_at.as_deref());
    }

    pub(super) fn observe_result(&mut self, result: Option<ActResult>, time: Option<&str>) {
        if let Some(result) = result
            && self.conclusion != Some(ItemConclusion::Unsupported)
        {
            self.status = ItemStatus::Completed;
            self.conclusion = Some(result.into());
            self.observe_end(time);
        }
    }
}

impl Section {
    pub(super) fn observe_start(&mut self, time: Option<&str>) {
        if self.started_at.is_none() {
            self.started_at = valid_timestamp(time);
        }
    }

    pub(super) fn observe_end(&mut self, time: Option<&str>) {
        self.completed_at = valid_timestamp(time);
    }
}
