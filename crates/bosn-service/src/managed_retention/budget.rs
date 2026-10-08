//! One elapsed-time budget shared by a synchronous pass and all peer sweeps.

use super::*;
use std::{cell::Cell, time::Instant};

pub(super) const PASS_LIMIT: Duration = Duration::from_secs(10 * 60);
thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub(super) struct Guard(Option<Instant>);

impl Guard {
    pub(super) fn start(limit: Duration) -> Self {
        let deadline = Instant::now() + limit;
        Self(DEADLINE.replace(Some(
            DEADLINE.get().map_or(deadline, |prior| prior.min(deadline)),
        )))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        DEADLINE.set(self.0);
    }
}

pub(super) fn check() -> Result<(), String> {
    if DEADLINE
        .get()
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        Err("retention pass time budget exhausted; remaining objects deferred".into())
    } else {
        Ok(())
    }
}

pub(super) fn options(mut options: RunOptions) -> RunOptions {
    if let Some(deadline) = DEADLINE.get() {
        options.deadline = options
            .deadline
            .min(deadline.saturating_duration_since(Instant::now()));
    }
    options
}

pub(super) fn observe(unreadable: &mut details::ReadFailures) -> bool {
    match check() {
        Ok(()) => true,
        Err(reason) => {
            unreadable.push(reason);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_work_cannot_extend_budget_and_scope_restores_it() {
        assert!(check().is_ok());
        let outer = Guard::start(Duration::ZERO);
        assert!(check().is_err());
        {
            let _peer = Guard::start(Duration::from_secs(60));
            assert!(check().is_err());
            assert_eq!(
                options(RunOptions::bounded(Duration::from_secs(30), 123)).deadline,
                Duration::ZERO
            );
        }
        assert!(check().is_err());
        drop(outer);
        assert!(check().is_ok());
    }

    #[test]
    fn expired_apply_refuses_before_any_engine_access() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let marker = root.path().join("engine-called");
        let engine = DockerEngine::synthetic_for_test(
            "/bin/sh",
            [
                std::ffi::OsString::from("-c"),
                "printf probe > \"$1\"; exit 42".into(),
                "budget-test".into(),
                marker.as_os_str().to_owned(),
            ],
        );
        let _budget = Guard::start(Duration::ZERO);
        let outcome =
            managed_retention_pass(&engine, root.path(), RetentionPolicy::default(), true);
        assert!(
            outcome
                .summary
                .refused
                .as_deref()
                .unwrap()
                .contains("time budget exhausted")
        );
        assert_eq!(outcome.summary.removed, 0);
        assert!(!marker.exists());
    }
}
