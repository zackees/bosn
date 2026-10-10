//! Coordinated deadlines for exact engine and private-storage retirement.
use std::time::Duration;

/// Production budgets. Unit tests run the same code on a shorter scale
/// (#503), so a fixture that must outlast `CONTROL` takes seconds, not 30 s.
const PRODUCTION_CONTROL: Duration = Duration::from_secs(30);
const PRODUCTION_STORAGE_CONTROL: Duration = Duration::from_secs(10);
const PRODUCTION_DELETE: Duration = Duration::from_secs(90);
const TEST_CONTROL: Duration = Duration::from_secs(5);
const TEST_STORAGE_CONTROL: Duration = Duration::from_secs(4);
const TEST_DELETE: Duration = Duration::from_secs(10);

pub(super) const CONTROL: Duration = if cfg!(test) {
    TEST_CONTROL
} else {
    PRODUCTION_CONTROL
};
pub(super) const STORAGE_CONTROL: Duration = if cfg!(test) {
    TEST_STORAGE_CONTROL
} else {
    PRODUCTION_STORAGE_CONTROL
};
pub(super) const DELETE: Duration = if cfg!(test) {
    TEST_DELETE
} else {
    PRODUCTION_DELETE
};
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_budgets_are_unchanged() {
        assert_eq!(PRODUCTION_CONTROL, Duration::from_secs(30));
        assert_eq!(PRODUCTION_STORAGE_CONTROL, Duration::from_secs(10));
        assert_eq!(PRODUCTION_DELETE, Duration::from_secs(90));
    }

    #[test]
    fn test_scale_keeps_the_production_ordering() {
        assert_eq!(
            (CONTROL, STORAGE_CONTROL, DELETE),
            (TEST_CONTROL, TEST_STORAGE_CONTROL, TEST_DELETE)
        );
        // A removal slower than one control call must still fit the delete
        // budget, as in production.
        assert!(TEST_CONTROL < TEST_DELETE && PRODUCTION_CONTROL < PRODUCTION_DELETE);
        assert!(TEST_STORAGE_CONTROL < TEST_DELETE);
        assert!(PRODUCTION_STORAGE_CONTROL < PRODUCTION_DELETE);
        assert!(removal_reserve(true) < CLEANUP_BUDGET);
    }
}
