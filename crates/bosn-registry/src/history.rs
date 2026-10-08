//! Bounded diagnostic history; ownership/protection live in separate tables.
use super::*;

pub const RETAINED_EVENTS: usize = 4096;
pub const MAX_EVENT_DETAIL_BYTES: usize = 64 * 1024;
pub const MAX_EVENT_KIND_BYTES: usize = 256;

impl Immediate<'_> {
    pub(crate) fn trim_event_history(&mut self) -> Result<(), Error> {
        self.transaction.execute(
            "DELETE FROM events WHERE id <= (SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET ?)",
            &[Value::Integer(RETAINED_EVENTS as i64)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_events_keep_recent_history_and_reuse_sqlite_storage() {
        let root = fs::TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(
            root.path().join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        let mut sizes = Vec::new();
        for cycle in 0..3 {
            let mut transaction = registry.begin_immediate().unwrap();
            for offset in 0..RETAINED_EVENTS {
                let index = cycle * RETAINED_EVENTS + offset;
                transaction
                    .append_event(index as f64, "setup.finished", &format!("run={index}"))
                    .unwrap();
            }
            transaction.commit().unwrap();
            registry.connection.checkpoint().unwrap();
            let rows = registry
                .connection
                .query(
                    "SELECT COUNT(*) FROM events",
                    &[],
                    QueryLimits {
                        max_rows: 1,
                        max_bytes: 1024,
                    },
                )
                .unwrap();
            assert_eq!(integer(&rows[0], 0).unwrap(), RETAINED_EVENTS as i64);
            sizes.push(
                std::fs::metadata(root.path().join("registry.sqlite3"))
                    .unwrap()
                    .len(),
            );
        }
        assert!(
            sizes[2] <= sizes[1] + 16 * 1024,
            "history grew across bounded cycles: {sizes:?}"
        );
        let newest = registry
            .connection
            .query(
                "SELECT detail FROM events ORDER BY id DESC LIMIT 1",
                &[],
                QueryLimits {
                    max_rows: 1,
                    max_bytes: 1024,
                },
            )
            .unwrap();
        assert_eq!(
            text(&newest[0], 0).unwrap(),
            format!("run={}", 3 * RETAINED_EVENTS - 1)
        );
        let mut transaction = registry.begin_immediate().unwrap();
        assert!(
            transaction
                .append_event(1.0, "oversize", &"x".repeat(MAX_EVENT_DETAIL_BYTES + 1))
                .is_err()
        );
    }
}
