//! Bounded typed act2 retention evidence. Logical archive bytes are not disk blocks.
use super::{
    cache_cohort::{Namespace, root},
    cache_policy::CachePolicy,
};
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CohortReport {
    pub schema_version: u32,
    pub root: String,
    pub partial: bool,
    pub errors: Option<Vec<String>>,
    pub budget_bytes: i64,
    pub remaining_completed_bytes: Option<u64>,
    pub protected_bytes: Option<u64>,
    pub budget_met: Option<bool>,
    pub namespaces: Option<Vec<NamespaceAudit>>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StoreStatus {
    Missing,
    Ready,
    Busy,
    Partial,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceAudit {
    pub schema_version: u32,
    pub namespace: String,
    pub status: StoreStatus,
    pub partial: bool,
    pub errors: Option<Vec<String>>,
    pub entry_count: Option<u64>,
    pub archive_bytes: Option<u64>,
    pub temporary_bytes: u64,
    pub untracked_bytes: u64,
    pub fingerprint: String,
    pub entries: Option<Vec<StoreEntry>>,
    pub next_cursor: Option<u64>,
    pub retention: Option<RetentionReport>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreEntry {
    pub id: u64,
    pub key: String,
    pub version: String,
    #[serde(rename = "cacheSize")]
    pub cache_size: i64,
    pub complete: bool,
    #[serde(rename = "usedAt")]
    pub used_at: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    pub bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionReport {
    pub deleted_count: u64,
    pub reclaimed_archive_bytes: u64,
    pub receipts: Option<Vec<EvictionReceipt>>,
    pub receipts_omitted: u64,
    pub budget_bytes: i64,
    pub remaining_completed_bytes: Option<u64>,
    pub protected_bytes: Option<u64>,
    pub budget_met: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionReason {
    Incomplete,
    UnusedAge,
    AbsoluteAge,
    Superseded,
    ByteBudget,
    AggregateBudget,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvictionReceipt {
    pub id: u64,
    pub reason: EvictionReason,
    pub archive_bytes: u64,
}

fn counts(
    remaining: Option<u64>,
    protected: Option<u64>,
    met: Option<bool>,
    budget: i64,
) -> Result<(), String> {
    if budget <= 0
        || remaining.is_some_and(|v| v > i64::MAX as u64)
        || protected.is_some_and(|v| v > i64::MAX as u64)
        || remaining.zip(protected).is_some_and(|(r, p)| p > r)
        || met.is_some_and(|value| remaining.is_none_or(|r| value != (r <= budget as u64)))
    {
        return Err("retention byte evidence is contradictory".into());
    }
    Ok(())
}

impl RetentionReport {
    /// `namespace_budget` is the per-namespace ceiling the pass was asked to apply.
    pub(crate) fn validate(&self, namespace_budget: i64) -> Result<(), String> {
        if self.budget_bytes != namespace_budget {
            return Err("namespace retention ceiling mismatch".into());
        }
        counts(
            self.remaining_completed_bytes,
            self.protected_bytes,
            self.budget_met,
            self.budget_bytes,
        )?;
        let receipts = self.receipts.as_deref().unwrap_or_default();
        if receipts.len() > 32
            || (receipts.len() as u64).checked_add(self.receipts_omitted)
                != Some(self.deleted_count)
        {
            return Err("eviction receipt count mismatch".into());
        }
        let mut ids = BTreeSet::new();
        let mut bytes = 0_u64;
        for receipt in receipts {
            if receipt.id == 0 || !ids.insert(receipt.id) {
                return Err("eviction receipt identity mismatch".into());
            }
            bytes = bytes
                .checked_add(receipt.archive_bytes)
                .ok_or("eviction receipt byte overflow")?;
        }
        if bytes > self.reclaimed_archive_bytes
            || (self.receipts_omitted == 0 && bytes != self.reclaimed_archive_bytes)
        {
            return Err("eviction receipt byte mismatch".into());
        }
        Ok(())
    }
}

impl CohortReport {
    pub fn parse(bytes: &[u8], policy: CachePolicy) -> Result<Self, String> {
        if bytes.len() > 64 * 1024 {
            return Err("cohort report exceeds bounded output".into());
        }
        let report: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if report.schema_version != 1
            || report.root != root()
            || report.budget_bytes != policy.aggregate_max_bytes
        {
            return Err("cohort report identity, schema or ceiling mismatch".into());
        }
        counts(
            report.remaining_completed_bytes,
            report.protected_bytes,
            report.budget_met,
            report.budget_bytes,
        )?;
        let stores = report.namespaces.as_deref().unwrap_or_default();
        if stores.len() > 64 || report.errors.as_ref().is_some_and(|v| v.len() > 8) {
            return Err("cohort report exceeds detail limits".into());
        }
        let mut seen = BTreeSet::new();
        let mut remaining = 0_u64;
        for store in stores {
            // act2 audits identify a direct child by basename. The enclosing
            // report binds that child to the exact shared root.
            let name = store.namespace.as_str();
            Namespace::parse(name)?;
            if store.schema_version != 1
                || !seen.insert(name)
                || store.entries.as_ref().is_some_and(|v| v.len() > 12)
            {
                return Err("cohort namespace identity or page mismatch".into());
            }
            if let Some(retention) = &store.retention {
                retention.validate(policy.repository_max_bytes)?;
            }
            if !report.partial {
                let retention = store
                    .retention
                    .as_ref()
                    .ok_or("namespace retention missing")?;
                remaining = remaining
                    .checked_add(
                        retention
                            .remaining_completed_bytes
                            .ok_or("namespace bytes unknown")?,
                    )
                    .ok_or("cohort byte overflow")?;
                if retention.protected_bytes.is_none() || retention.budget_met.is_none() {
                    return Err("namespace protection or budget outcome unknown".into());
                }
            }
        }
        if !report.partial
            && (report.errors.as_ref().is_some_and(|v| !v.is_empty())
                || report.remaining_completed_bytes.is_none()
                || report.protected_bytes.is_none()
                || report.budget_met.is_none()
                || stores.iter().any(|store| {
                    store.partial
                        || store.status != StoreStatus::Ready
                        || store.errors.as_ref().is_some_and(|v| !v.is_empty())
                        || store.archive_bytes.is_none()
                        || store.entry_count.is_none()
                        || store.retention.is_none()
                }))
        {
            return Err("complete cohort report has unknown namespace evidence".into());
        }
        if !report.partial && report.remaining_completed_bytes != Some(remaining) {
            return Err("cohort remaining bytes differ from namespace retention evidence".into());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
