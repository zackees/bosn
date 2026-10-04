//! Local migration journal. Publication evidence never authorizes routing or deletion.
use super::*;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "ci.cache-migration.v1:";
const RECORD_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMigrationIntent {
    pub namespace: String,
    pub nonce: String,
    pub max_bytes: i64,
    pub created_at: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePublicationEvidence {
    pub source_fingerprint: String,
    pub imported_count: u64,
    pub imported_bytes: u64,
    pub retained_source_archive_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMigrationRecord {
    pub schema_version: u32,
    pub intent: CacheMigrationIntent,
    pub publication: Option<CachePublicationEvidence>,
    pub updated_at: f64,
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

fn key(namespace: &str) -> Result<String, Error> {
    if !lower_hex(namespace, 16) {
        return Err(Error::BadRow("cache migration namespace"));
    }
    Ok(format!("{PREFIX}{namespace}"))
}

fn decode(detail: &str, namespace: &str) -> Result<CacheMigrationRecord, Error> {
    let record: CacheMigrationRecord =
        serde_json::from_str(detail).map_err(|_| Error::BadRow("cache migration JSON"))?;
    if record.schema_version != 1
        || record.intent.namespace != namespace
        || !is_uuid(&record.intent.nonce)
        || record.intent.max_bytes <= 0
        || !record.intent.created_at.is_finite()
        || record.intent.created_at < 0.0
        || !record.updated_at.is_finite()
        || record.updated_at < record.intent.created_at
    {
        return Err(Error::BadRow("cache migration intent"));
    }
    key(namespace)?;
    if let Some(proof) = &record.publication
        && (!lower_hex(&proof.source_fingerprint, 64)
            || proof.imported_bytes > record.intent.max_bytes as u64
            || proof.retained_source_archive_bytes > i64::MAX as u64
            || proof.retained_source_archive_bytes < proof.imported_bytes
            || (proof.retained_source_archive_bytes > 0 && proof.imported_count == 0)
            || (proof.imported_count == 0 && proof.imported_bytes != 0))
    {
        return Err(Error::BadRow("cache migration publication"));
    }
    Ok(record)
}

impl Immediate<'_> {
    fn migration_record(&mut self, namespace: &str) -> Result<Option<CacheMigrationRecord>, Error> {
        let rows = self.transaction.query(
            "SELECT value FROM meta WHERE key=?",
            &[Value::Text(key(namespace)?)],
            QueryLimits {
                max_rows: 1,
                max_bytes: RECORD_BYTES,
            },
        )?;
        rows.first()
            .map(|row| decode(&text(row, 0)?, namespace))
            .transpose()
    }

    fn store_migration(&mut self, record: &CacheMigrationRecord) -> Result<(), Error> {
        let detail =
            serde_json::to_string(record).map_err(|_| Error::BadRow("cache migration JSON"))?;
        decode(&detail, &record.intent.namespace)?;
        if detail.len() > RECORD_BYTES {
            return Err(Error::BadRow("cache migration size"));
        }
        self.transaction.execute("INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[Value::Text(key(&record.intent.namespace)?), Value::Text(detail)])?;
        Ok(())
    }

    /// Commit before issuing import. An existing intent requires reconciliation;
    /// it is not permission to repeat import, even with the same nonce.
    pub fn begin_cache_migration(&mut self, intent: &CacheMigrationIntent) -> Result<(), Error> {
        if self.migration_record(&intent.namespace)?.is_some() {
            return Err(Error::BadRow("cache migration already started"));
        }
        self.store_migration(&CacheMigrationRecord {
            schema_version: 1,
            intent: intent.clone(),
            publication: None,
            updated_at: intent.created_at,
        })
    }

    /// Record a validated historical receipt. This is not current inventory,
    /// machine-wide enrollment, old-writer exclusion or source deletion authority.
    pub fn record_cache_publication(
        &mut self,
        namespace: &str,
        nonce: &str,
        proof: &CachePublicationEvidence,
        at: f64,
    ) -> Result<(), Error> {
        let mut record = self
            .migration_record(namespace)?
            .ok_or(Error::BadRow("cache migration missing"))?;
        if record.intent.nonce != nonce
            || !at.is_finite()
            || at < record.updated_at
            || record
                .publication
                .as_ref()
                .is_some_and(|previous| previous != proof)
        {
            return Err(Error::BadRow("cache migration publication conflict"));
        }
        record.publication = Some(proof.clone());
        record.updated_at = at;
        self.store_migration(&record)
    }
}

fn read(connection: &Connection, namespace: &str) -> Result<Option<CacheMigrationRecord>, Error> {
    let rows = connection.query(
        "SELECT value FROM meta WHERE key=?",
        &[Value::Text(key(namespace)?)],
        QueryLimits {
            max_rows: 1,
            max_bytes: RECORD_BYTES,
        },
    )?;
    rows.first()
        .map(|row| decode(&text(row, 0)?, namespace))
        .transpose()
}
impl Registry {
    pub fn cache_migration(&self, namespace: &str) -> Result<Option<CacheMigrationRecord>, Error> {
        read(&self.connection, namespace)
    }
}
impl ReadOnlyRegistry {
    pub fn cache_migration(&self, namespace: &str) -> Result<Option<CacheMigrationRecord>, Error> {
        read(&self.connection, namespace)
    }
}

#[cfg(test)]
mod tests;
