//! Current read-only inventory, independent of historical import receipts.
use super::{
    cache_cohort::Namespace,
    cache_maintenance::{NamespaceAudit, StoreStatus},
};

impl NamespaceAudit {
    /// Validate the initial bounded page. Summary counts cover the whole store;
    /// entries remain a sample when a continuation cursor is present.
    pub fn parse_inventory(bytes: &[u8], namespace: &Namespace) -> Result<Self, String> {
        if bytes.len() > 64 * 1024 {
            return Err("cache inventory exceeds bounded output".into());
        }
        let report: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if report.schema_version != 1
            || report.namespace != namespace.as_str()
            || report.retention.is_some()
            || report.errors.as_ref().is_some_and(|errors| {
                errors.len() > 8 || errors.iter().any(|e| e.chars().count() > 128)
            })
        {
            return Err("cache inventory identity, schema or detail mismatch".into());
        }
        let entries = report.entries.as_deref().unwrap_or_default();
        if entries.len() > 12
            || report
                .entry_count
                .is_some_and(|count| count > 100_000 || count < entries.len() as u64)
            || [
                report.archive_bytes,
                Some(report.temporary_bytes),
                Some(report.untracked_bytes),
            ]
            .into_iter()
            .flatten()
            .any(|v| v > i64::MAX as u64)
        {
            return Err("cache inventory exceeds catalog bounds".into());
        }
        let mut previous = 0;
        let mut sampled_bytes = 0_u64;
        for entry in entries {
            if entry.id <= previous
                || entry.key.len() > 512
                || entry.version.len() > 128
                || entry.cache_size < -1
                || entry.bytes.is_some_and(|v| v > i64::MAX as u64)
            {
                return Err("cache inventory entry is invalid".into());
            }
            previous = entry.id;
            sampled_bytes = sampled_bytes
                .checked_add(entry.bytes.unwrap_or_default())
                .ok_or("cache inventory byte overflow")?;
        }
        if report
            .next_cursor
            .is_some_and(|cursor| entries.len() != 12 || cursor != previous)
            || report.archive_bytes.is_some_and(|bytes| {
                sampled_bytes > bytes
                    || report
                        .temporary_bytes
                        .checked_add(report.untracked_bytes)
                        .is_none_or(|known| known > bytes)
            })
        {
            return Err("cache inventory page or byte evidence is contradictory".into());
        }
        if !report.partial && report.status == StoreStatus::Ready {
            report.require_current_inventory()?;
            let count = report.entry_count.unwrap();
            if report.next_cursor.is_none() && count != entries.len() as u64
                || report.next_cursor.is_some() && count <= entries.len() as u64
                || entries
                    .iter()
                    .any(|entry| entry.complete && entry.bytes.is_none())
            {
                return Err("complete inventory page omits evidence".into());
            }
        }
        Ok(report)
    }

    /// A missing, busy or partial store cannot authorize a warm route.
    pub fn require_current_inventory(&self) -> Result<(), String> {
        if self.partial
            || self.status != StoreStatus::Ready
            || self
                .errors
                .as_ref()
                .is_some_and(|errors| !errors.is_empty())
            || self.archive_bytes.is_none()
            || self.entry_count.is_none()
            || self.fingerprint.len() != 64
            || !self
                .fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("current cache inventory is unavailable or incomplete".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> serde_json::Value {
        serde_json::json!({"schema_version":1,"namespace":"0123456789abcdef","status":"ready","partial":false,"errors":[],"entry_count":1,"archive_bytes":80,"temporary_bytes":0,"untracked_bytes":0,"fingerprint":"a".repeat(64),"entries":[{"id":1,"key":"warm","version":"v1","cacheSize":80,"complete":true,"usedAt":1,"createdAt":1,"bytes":80}],"next_cursor":null})
    }
    #[test]
    fn current_inventory_rejects_contradictory_or_historical_evidence() {
        let namespace = Namespace::parse("0123456789abcdef").unwrap();
        let parse = |value: &serde_json::Value| {
            NamespaceAudit::parse_inventory(&serde_json::to_vec(value).unwrap(), &namespace)
        };
        let value = ready();
        parse(&value).unwrap().require_current_inventory().unwrap();
        for (field, invalid) in [
            ("schema_version", serde_json::json!(2)),
            ("entry_count", serde_json::json!(0)),
            ("archive_bytes", serde_json::json!(79)),
            ("fingerprint", serde_json::json!("historical")),
            ("next_cursor", serde_json::json!(1)),
            ("namespace", serde_json::json!("0123456789abcdee")),
        ] {
            let mut invalid_value = ready();
            invalid_value[field] = invalid;
            assert!(parse(&invalid_value).is_err(), "{field}");
        }
        let mut partial = ready();
        partial["partial"] = serde_json::json!(true);
        partial["status"] = serde_json::json!("busy");
        partial["archive_bytes"] = serde_json::Value::Null;
        partial["fingerprint"] = serde_json::json!("");
        assert!(
            parse(&partial)
                .unwrap()
                .require_current_inventory()
                .is_err()
        );
    }
}
