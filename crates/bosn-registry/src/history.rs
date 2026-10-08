//! Bounded diagnostics while preserving event-backed operational authority.
use super::*;

pub const RETAINED_EVENTS: usize = 4096;
pub const MAX_EVENT_DETAIL_BYTES: usize = 64 * 1024;
pub const MAX_EVENT_KIND_BYTES: usize = 256;

impl Immediate<'_> {
    pub(crate) fn trim_event_history(&mut self) -> Result<(), Error> {
        // Engine/helper recovery reads the newest event for each exact producer
        // key. Keep that state, including terminal tombstones, so an older
        // nonterminal state can never reappear after trimming.
        self.transaction.execute(
            "DELETE FROM events WHERE (kind GLOB 'act.engine.v1:*' OR kind GLOB 'ci.cache-helper.v1:*') \
             AND id NOT IN (SELECT MAX(id) FROM events WHERE kind GLOB 'act.engine.v1:*' \
                 OR kind GLOB 'ci.cache-helper.v1:*' GROUP BY kind)",
            &[],
        )?;
        // Exact duplicate contracts and vetoes convey the same authorization.
        // Distinct records must survive unrelated diagnostic traffic.
        self.transaction.execute(
            "DELETE FROM events WHERE kind IN ('manifest.recovery.contract','manifest.autostart.disabled') \
             AND id NOT IN (SELECT MAX(id) FROM events WHERE kind IN \
                 ('manifest.recovery.contract','manifest.autostart.disabled') GROUP BY kind,detail)",
            &[],
        )?;
        self.transaction.execute(
            "DELETE FROM events WHERE kind NOT IN ('manifest.recovery.contract','manifest.autostart.disabled') \
             AND kind NOT GLOB 'act.engine.v1:*' AND kind NOT GLOB 'ci.cache-helper.v1:*' \
             AND id <= (SELECT id FROM events WHERE kind NOT IN \
                 ('manifest.recovery.contract','manifest.autostart.disabled') \
                 AND kind NOT GLOB 'act.engine.v1:*' AND kind NOT GLOB 'ci.cache-helper.v1:*' \
                 ORDER BY id DESC LIMIT 1 OFFSET ?)",
            &[Value::Integer(RETAINED_EVENTS as i64)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_flood_preserves_operational_authority_after_reopen() {
        let directory = fs::TemporaryDirectory::new().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let mut registry =
            Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
        let operational = [
            ("manifest.recovery.contract", "contract"),
            ("manifest.autostart.disabled", "veto"),
            ("act.engine.v1:fixture", "terminal"),
            ("ci.cache-helper.v1:fixture", "removed"),
        ];
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .append_event(0.0, "act.engine.v1:fixture", "nonterminal")
            .unwrap();
        transaction
            .append_event(0.0, "ci.cache-helper.v1:fixture", "live")
            .unwrap();
        transaction
            .append_event(0.0, "manifest.recovery.contract", "distinct-contract")
            .unwrap();
        transaction
            .append_event(0.0, "manifest.autostart.disabled", "distinct-veto")
            .unwrap();
        for (kind, detail) in operational {
            transaction.append_event(1.0, kind, detail).unwrap();
            transaction.append_event(1.0, kind, detail).unwrap();
        }
        for index in 0..=RETAINED_EVENTS {
            transaction
                .append_event(index as f64 + 2.0, "diagnostic", "noise")
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(registry);
        let registry = Registry::open_writer(&path).unwrap();
        assert_eq!(
            registry.manifest_recovery_contract_details(2).unwrap(),
            ["contract", "distinct-contract"]
        );
        assert!(registry.manifest_autostart_intent_disabled("veto").unwrap());
        assert!(
            registry
                .manifest_autostart_intent_disabled("distinct-veto")
                .unwrap()
        );
        for (kind, detail) in operational {
            let rows = registry
                .connection
                .query(
                    "SELECT detail FROM events WHERE kind=? AND detail=? ORDER BY id DESC",
                    &[Value::Text(kind.into()), Value::Text(detail.into())],
                    QueryLimits {
                        max_rows: 1,
                        max_bytes: 1024,
                    },
                )
                .unwrap();
            assert_eq!(rows.len(), 1, "lost operational record {kind}");
            assert_eq!(text(&rows[0], 0).unwrap(), detail);
        }
        let old = registry
            .connection
            .query(
                "SELECT id FROM events WHERE detail IN ('nonterminal','live')",
                &[],
                QueryLimits {
                    max_rows: 2,
                    max_bytes: 1024,
                },
            )
            .unwrap();
        assert!(old.is_empty(), "old journal state could resurface");
    }
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
