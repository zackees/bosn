//! Bound engine diagnostics independently of the total failure count.

const MAX_DETAILS: usize = 64;
const MAX_DETAIL_BYTES: usize = 2048;

pub(super) fn push(details: &mut Vec<String>, mut detail: String) {
    if details.len() >= MAX_DETAILS {
        return;
    }
    if detail.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES - 3;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        detail.push_str("...");
    }
    details.push(detail);
}

/// Retain exact failure accounting while bounding the response payload.
#[derive(Default)]
pub(super) struct ReadFailures {
    count: usize,
    details: Vec<String>,
}

impl ReadFailures {
    pub(super) fn push(&mut self, detail: String) {
        self.count = self.count.saturating_add(1);
        push(&mut self.details, detail);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(super) fn len(&self) -> usize {
        self.count
    }

    pub(super) fn describe(&self) -> String {
        let mut text = self.details.join("; ");
        let omitted = self.count.saturating_sub(self.details.len());
        if omitted > 0 {
            text.push_str(&format!(
                "; {omitted} additional read failure detail(s) omitted"
            ));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_failures_keep_total_count_with_bounded_details() {
        let mut failures = ReadFailures::default();
        for _ in 0..1024 {
            failures.push("失敗".repeat(1024));
        }
        assert_eq!(failures.len(), 1024);
        assert!(!failures.is_empty());
        let report = failures.describe();
        assert!(report.contains("960 additional read failure detail(s) omitted"));
        assert!(report.len() < MAX_DETAILS * (MAX_DETAIL_BYTES + 2) + 100);
    }

    #[test]
    fn engine_failures_have_bounded_count_and_utf8_safe_size() {
        let mut details = Vec::new();
        for _ in 0..1024 {
            push(&mut details, "失敗".repeat(1024));
        }
        assert_eq!(details.len(), MAX_DETAILS);
        assert!(
            details
                .iter()
                .all(|detail| detail.len() <= MAX_DETAIL_BYTES)
        );
        assert!(details.iter().all(|detail| detail.ends_with("...")));
    }
}
