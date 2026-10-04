//! Coordinated deadlines for exact engine and private-storage retirement.
use std::time::Duration;

pub(super) const CONTROL: Duration = Duration::from_secs(30);
pub(super) const STORAGE_CONTROL: Duration = Duration::from_secs(10);
pub(super) const DELETE: Duration = Duration::from_secs(90);
pub(crate) const CLEANUP_BUDGET: Duration = Duration::from_secs(360);
pub(crate) const CLEANUP_PASS_BUDGET: Duration = Duration::from_secs(380);

/// Deletion, independent absence observations, and durable persistence.
pub(crate) fn removal_reserve(named_storage: bool) -> Duration {
    let container = DELETE + CONTROL * 2 + Duration::from_secs(5);
    if named_storage {
        container + DELETE + STORAGE_CONTROL * 3
    } else {
        container
    }
}
