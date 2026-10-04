//! Typed import receipts. Publication is not equivalent to complete enrollment.
use super::{cache_cohort::Namespace, cache_policy::CachePolicy};
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportReceipt {
    pub source_id: u64,
    pub destination_id: u64,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportReport {
    pub schema_version: u32,
    pub source: String,
    pub destination: String,
    pub published: bool,
    pub partial: bool,
    pub error: Option<String>,
    pub pending_stage: Option<String>,
    pub retained_source_archive_bytes: Option<u64>,
    pub available_destination_bytes: Option<u64>,
    pub required_additional_bytes: Option<u64>,
    pub imported_count: u64,
    pub imported_bytes: u64,
    pub skipped_incomplete: u64,
    pub skipped_budget: u64,
    pub receipts: Option<Vec<ImportReceipt>>,
    pub receipts_omitted: u64,
}

impl ImportReport {
    /// Check identity and bounded evidence before any receipt is trusted.
    pub fn parse(bytes: &[u8], namespace: &Namespace, policy: CachePolicy) -> Result<Self, String> {
        if bytes.len() > 64 * 1024 {
            return Err("import report exceeds bounded output".into());
        }
        let report: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if report.schema_version != 1
            || report.source != namespace.legacy_path()
            || report.destination != namespace.path()
            || !i64::try_from(report.imported_bytes)
                .is_ok_and(|bytes| bytes <= policy.repository_max_bytes)
        {
            return Err("import report identity, schema or byte ceiling mismatch".into());
        }
        let receipts = report.receipts.as_deref().unwrap_or_default();
        let count = u64::try_from(receipts.len()).map_err(|error| error.to_string())?;
        if receipts.len() > 12
            || count.checked_add(report.receipts_omitted) != Some(report.imported_count)
        {
            return Err("import receipt count mismatch".into());
        }
        let mut sources = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        let mut receipt_bytes = 0_u64;
        for receipt in receipts {
            if receipt.source_id == 0
                || receipt.destination_id == 0
                || !sources.insert(receipt.source_id)
                || !destinations.insert(receipt.destination_id)
                || receipt.sha256.len() != 64
                || !receipt
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("invalid import archive receipt".into());
            }
            receipt_bytes = receipt_bytes
                .checked_add(receipt.bytes)
                .ok_or("import receipt byte overflow")?;
        }
        if receipt_bytes > report.imported_bytes
            || (report.receipts_omitted == 0 && receipt_bytes != report.imported_bytes)
        {
            return Err("import receipt byte total mismatch".into());
        }
        Ok(report)
    }

    /// A late publication followed by fsync failure stays published/incomplete.
    /// It must be reconciled, never treated as an absent destination or retried blindly.
    pub fn require_warm_publication(&self) -> Result<(), String> {
        let retained = self.retained_source_archive_bytes;
        let headroom = self
            .available_destination_bytes
            .zip(self.required_additional_bytes);
        if !self.published
            || self.partial
            || self.error.as_ref().is_some_and(|error| !error.is_empty())
            || self
                .pending_stage
                .as_ref()
                .is_some_and(|stage| !stage.is_empty())
            || retained.is_none_or(|bytes| bytes < self.imported_bytes)
            || headroom.is_none_or(|(available, required)| available < required)
        {
            return Err(
                "import is incomplete; publication must be reconciled before enrollment".into(),
            );
        }
        if retained.is_some_and(|bytes| bytes > 0) && self.imported_count == 0 {
            return Err("populated legacy cache cannot enroll an empty warm destination".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> CachePolicy {
        toml::from_str("repository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap()
    }
    fn namespace() -> Namespace {
        Namespace::parse("0123456789abcdef").unwrap()
    }
    fn fixture() -> serde_json::Value {
        serde_json::json!({
            "schema_version":1,"source":namespace().legacy_path(),"destination":namespace().path(),
            "published":true,"partial":false,"retained_source_archive_bytes":160,
            "available_destination_bytes":1000000000,"required_additional_bytes":67108864,
            "imported_count":1,"imported_bytes":80,"skipped_incomplete":0,"skipped_budget":1,
            "receipts":[{"source_id":1,"destination_id":1,"bytes":80,"sha256":"a".repeat(64)}],"receipts_omitted":0
        })
    }
    fn parse(value: &serde_json::Value) -> Result<ImportReport, String> {
        ImportReport::parse(&serde_json::to_vec(value).unwrap(), &namespace(), policy())
    }
    #[test]
    fn import_receipts_bind_identity_counts_bytes_and_archive_digests() {
        parse(&fixture())
            .unwrap()
            .require_warm_publication()
            .unwrap();
        for (field, invalid) in [
            ("schema_version", serde_json::json!(2)),
            ("source", serde_json::json!("/foreign")),
            ("destination", serde_json::json!("/foreign")),
            ("imported_bytes", serde_json::json!(101)),
            ("imported_count", serde_json::json!(2)),
            ("receipts_omitted", serde_json::json!(u64::MAX)),
        ] {
            let mut value = fixture();
            value[field] = invalid;
            assert!(parse(&value).is_err(), "{field}");
        }
        let mut value = fixture();
        value["receipts"][0]["sha256"] = serde_json::json!("A".repeat(64));
        assert!(parse(&value).is_err());
        assert!(ImportReport::parse(&vec![b' '; 65537], &namespace(), policy()).is_err());
    }
    #[test]
    fn partial_publication_stays_visible_without_authorizing_enrollment() {
        let mut value = fixture();
        value["partial"] = serde_json::json!(true);
        value["error"] = serde_json::json!("parent sync failed");
        let report = parse(&value).unwrap();
        assert!(report.published);
        assert!(report.require_warm_publication().is_err());
        let mut value = fixture();
        value["available_destination_bytes"] = serde_json::Value::Null;
        assert!(parse(&value).unwrap().require_warm_publication().is_err());
    }
    #[test]
    fn empty_destination_cannot_replace_populated_legacy_cache() {
        let mut value = fixture();
        value["imported_count"] = serde_json::json!(0);
        value["imported_bytes"] = serde_json::json!(0);
        value["receipts"] = serde_json::Value::Null;
        assert!(parse(&value).unwrap().require_warm_publication().is_err());
        value["retained_source_archive_bytes"] = serde_json::json!(0);
        parse(&value).unwrap().require_warm_publication().unwrap();
    }
}
