//! Bounded latest maintenance evidence; not physical footprint or enrollment.
use super::*;
use serde::{Deserialize, Serialize};

const KEY: &str = "ci.cache-maintenance.latest.v1";
const MAX_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceSnapshot {
    pub schema_version: u32,
    pub observed_at: f64,
    pub helper: Option<MaintenanceHelper>,
    pub outcome: MaintenanceOutcome,
    pub recovery_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceHelper {
    pub nonce: String,
    pub container_id: String,
    pub cleanup_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum MaintenanceOutcome {
    Unknown {
        diagnostic: String,
    },
    Observed {
        exit_code: i32,
        partial: bool,
        budget_bytes: i64,
        remaining_completed_bytes: Option<u64>,
        protected_bytes: Option<u64>,
        budget_met: Option<bool>,
        reclaimed_archive_bytes: Option<u64>,
    },
}
impl MaintenanceSnapshot {
    pub fn validate(&self) -> Result<(), Error> {
        let bounded = |value: &str| value.len() <= 512;
        if self.schema_version != 1
            || !self.observed_at.is_finite()
            || self.observed_at < 0.0
            || self.recovery_error.as_deref().is_some_and(|s| !bounded(s))
        {
            return Err(Error::BadRow("maintenance snapshot"));
        }
        if let Some(helper) = &self.helper
            && (!is_uuid(&helper.nonce)
                || helper.nonce != helper.nonce.to_ascii_lowercase()
                || helper.container_id.len() != 64
                || !helper
                    .container_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
                || helper.cleanup_error.as_deref().is_some_and(|s| !bounded(s)))
        {
            return Err(Error::BadRow("maintenance helper receipt"));
        }
        match &self.outcome {
            MaintenanceOutcome::Unknown { diagnostic } if !bounded(diagnostic) => {
                return Err(Error::BadRow("maintenance diagnostic"));
            }
            MaintenanceOutcome::Observed {
                partial,
                budget_bytes,
                remaining_completed_bytes,
                protected_bytes,
                budget_met,
                reclaimed_archive_bytes,
                ..
            } if self.helper.is_none()
                || *budget_bytes <= 0
                || (*partial && reclaimed_archive_bytes.is_some())
                || (!partial
                    && (remaining_completed_bytes.is_none()
                        || protected_bytes.is_none()
                        || budget_met.is_none()))
                || matches!((protected_bytes, remaining_completed_bytes), (Some(p), Some(r)) if p > r) =>
            {
                return Err(Error::BadRow("maintenance observed totals"));
            }
            _ => {}
        }
        Ok(())
    }
}
impl Immediate<'_> {
    pub fn record_cache_maintenance(
        &mut self,
        snapshot: &MaintenanceSnapshot,
    ) -> Result<(), Error> {
        snapshot.validate()?;
        if let Some(helper) = &snapshot.helper {
            let record = self
                .helper_record(&helper.nonce)?
                .ok_or(Error::BadRow("maintenance helper missing"))?;
            if record.intent.role != Some(cache_helper::CacheHelperRole::MaintenanceV1)
                || record.container_id.as_deref() != Some(helper.container_id.as_str())
                || snapshot.observed_at < record.intent.created_at
                || (helper.cleanup_error.is_none()
                    && record.state != cache_helper::CacheHelperState::Removed)
            {
                return Err(Error::BadRow("maintenance helper evidence mismatch"));
            }
        }
        let value =
            serde_json::to_string(snapshot).map_err(|_| Error::BadRow("maintenance JSON"))?;
        if value.len() > MAX_BYTES {
            return Err(Error::BadRow("maintenance snapshot size"));
        }
        self.transaction.execute("INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[Value::Text(KEY.into()), Value::Text(value)])?;
        Ok(())
    }
}

fn read(connection: &Connection) -> Result<Option<MaintenanceSnapshot>, Error> {
    let rows = connection.query(
        "SELECT value FROM meta WHERE key=?",
        &[Value::Text(KEY.into())],
        QueryLimits {
            max_rows: 1,
            max_bytes: MAX_BYTES,
        },
    )?;
    rows.first()
        .map(|row| {
            let snapshot: MaintenanceSnapshot = serde_json::from_str(&text(row, 0)?)
                .map_err(|_| Error::BadRow("maintenance snapshot JSON"))?;
            snapshot.validate()?;
            Ok(snapshot)
        })
        .transpose()
}
impl Registry {
    pub fn latest_cache_maintenance(&self) -> Result<Option<MaintenanceSnapshot>, Error> {
        read(&self.connection)
    }
}
impl ReadOnlyRegistry {
    pub fn latest_cache_maintenance(&self) -> Result<Option<MaintenanceSnapshot>, Error> {
        read(&self.connection)
    }
}

#[cfg(test)]
mod tests;
