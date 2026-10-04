//! Durable intents for temporary cache accounting/maintenance containers.
//! Docker observation and absence proof belong to the trusted daemon, not a
//! client. Unacknowledged creates remain pending even when currently absent.

use super::*;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "ci.cache-helper.v1:";
const RECORD_BYTES: usize = 4096;
pub const MAX_HELPER_PAGE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHelperRole {
    MaintenanceV1,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheHelperIntent {
    pub registry_id: String,
    pub nonce: String,
    pub volume: String,
    pub image: String,
    pub created_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<CacheHelperRole>,
}

impl CacheHelperIntent {
    pub fn name(&self) -> String {
        let role = if self.role.is_some() {
            "maintain"
        } else {
            "measure"
        };
        format!("bosn-cache-{role}-{}", self.nonce)
    }

    pub fn validate(&self) -> Result<(), Error> {
        let digest = self.image.strip_prefix("docker.io/library/docker@sha256:");
        if !is_uuid(&self.registry_id)
            || kind(&self.nonce).is_err()
            || !self.created_at.is_finite()
            || self.created_at < 0.0
            || digest.is_none_or(|digest| !hex_id(digest))
        {
            return Err(Error::BadRow("cache helper intent"));
        }
        act::ActEngineCacheVolume {
            name: self.volume.clone(),
            target: "/cache".into(),
        }
        .validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHelperState {
    Pending,
    Created,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheHelperRecord {
    pub schema_version: u32,
    pub intent: CacheHelperIntent,
    pub state: CacheHelperState,
    pub container_id: Option<String>,
    pub updated_at: f64,
}

#[derive(Debug)]
pub struct CacheHelperPage {
    pub items: Vec<CacheHelperRecord>,
    pub next_nonce: Option<String>,
}

fn kind(nonce: &str) -> Result<String, Error> {
    if !is_uuid(nonce) || nonce != nonce.to_ascii_lowercase() {
        return Err(Error::BadRow("cache helper nonce"));
    }
    Ok(format!("{PREFIX}{nonce}"))
}

fn hex_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn decode(detail: &str, nonce: &str, owner: &str) -> Result<CacheHelperRecord, Error> {
    let record: CacheHelperRecord =
        serde_json::from_str(detail).map_err(|_| Error::BadRow("cache helper snapshot JSON"))?;
    record.intent.validate()?;
    if record.schema_version != 1
        || record.intent.nonce != nonce
        || !record.updated_at.is_finite()
        || record.updated_at < record.intent.created_at
        || (record.state == CacheHelperState::Pending) != record.container_id.is_none()
        || record.container_id.as_ref().is_some_and(|id| !hex_id(id))
    {
        return Err(Error::BadRow("cache helper snapshot"));
    }
    if record.intent.registry_id != owner {
        return Err(Error::ResourceIdentityConflict);
    }
    Ok(record)
}

impl Immediate<'_> {
    fn helper_owner(&mut self) -> Result<String, Error> {
        let rows = self.transaction.query(
            "SELECT value FROM meta WHERE key='registry_id'",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 128,
            },
        )?;
        rows.first()
            .ok_or(Error::BadRow("registry_id"))
            .and_then(|row| text(row, 0))
    }

    fn helper_record(&mut self, nonce: &str) -> Result<Option<CacheHelperRecord>, Error> {
        let rows = self.transaction.query(
            "SELECT detail FROM events WHERE kind=? ORDER BY id DESC LIMIT 1",
            &[Value::Text(kind(nonce)?)],
            QueryLimits {
                max_rows: 1,
                max_bytes: RECORD_BYTES,
            },
        )?;
        let owner = self.helper_owner()?;
        rows.first()
            .map(|row| decode(&text(row, 0)?, nonce, &owner))
            .transpose()
    }

    fn store_helper(&mut self, record: &CacheHelperRecord) -> Result<(), Error> {
        let detail =
            serde_json::to_string(record).map_err(|_| Error::BadRow("cache helper JSON"))?;
        decode(&detail, &record.intent.nonce, &self.helper_owner()?)?;
        if detail.len() > RECORD_BYTES {
            return Err(Error::BadRow("cache helper snapshot size"));
        }
        self.append_event(record.updated_at, &kind(&record.intent.nonce)?, &detail)
    }

    /// Commit before issuing Docker create. A nonce can never be reused, even
    /// after terminal removal, so an old receipt cannot authorize a new helper.
    pub fn begin_cache_helper(&mut self, intent: &CacheHelperIntent) -> Result<(), Error> {
        intent.validate()?;
        if self.helper_record(&intent.nonce)?.is_some() {
            return Err(Error::BadRow("cache helper nonce already used"));
        }
        self.store_helper(&CacheHelperRecord {
            schema_version: 1,
            intent: intent.clone(),
            state: CacheHelperState::Pending,
            container_id: None,
            updated_at: intent.created_at,
        })
    }

    /// Record the acknowledged or independently verified immutable ID before
    /// starting/removing it. Conflicting IDs never replace the original claim.
    pub fn register_cache_helper(&mut self, nonce: &str, id: &str, at: f64) -> Result<(), Error> {
        let mut record = self
            .helper_record(nonce)?
            .ok_or(Error::BadRow("cache helper intent missing"))?;
        if !hex_id(id)
            || !at.is_finite()
            || at < record.updated_at
            || record.state == CacheHelperState::Removed
            || record
                .container_id
                .as_ref()
                .is_some_and(|existing| existing != id)
        {
            return Err(Error::BadRow("cache helper registration"));
        }
        record.container_id = Some(id.into());
        record.state = CacheHelperState::Created;
        record.updated_at = at;
        self.store_helper(&record)
    }

    /// The trusted daemon supplies explicit absence of the registered ID.
    /// Mere name absence for an uncertain create is deliberately insufficient.
    pub fn finish_cache_helper(&mut self, nonce: &str, id: &str, at: f64) -> Result<(), Error> {
        let mut record = self
            .helper_record(nonce)?
            .ok_or(Error::BadRow("cache helper intent missing"))?;
        if record.state != CacheHelperState::Created
            || record.container_id.as_deref() != Some(id)
            || !at.is_finite()
            || at < record.updated_at
        {
            return Err(Error::BadRow("cache helper removal proof"));
        }
        record.state = CacheHelperState::Removed;
        record.updated_at = at;
        self.store_helper(&record)
    }
}

impl Registry {
    pub fn cache_helper(&self, nonce: &str) -> Result<Option<CacheHelperRecord>, Error> {
        read_helper(&self.connection, &self.registry_id()?, nonce)
    }

    pub fn pending_cache_helpers(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<CacheHelperPage, Error> {
        if limit == 0 || limit > MAX_HELPER_PAGE {
            return Err(Error::BadRow("cache helper page limit"));
        }
        let after_kind = after.map(kind).transpose()?.unwrap_or_default();
        let rows = self.connection.query("SELECT e.kind,e.detail FROM events e JOIN (SELECT MAX(id) AS id FROM events WHERE kind GLOB 'ci.cache-helper.v1:*' GROUP BY kind) latest ON latest.id=e.id WHERE e.kind>? AND (json_extract(e.detail,'$.state') IS NULL OR json_extract(e.detail,'$.state')<>'removed') ORDER BY e.kind LIMIT ?",
            &[Value::Text(after_kind), Value::Integer((limit + 1) as i64)],
            QueryLimits { max_rows: limit + 1, max_bytes: (limit + 1) * RECORD_BYTES })?;
        let owner = self.registry_id()?;
        let items = rows
            .iter()
            .take(limit)
            .map(|row| {
                let event = text(row, 0)?;
                let nonce = event
                    .strip_prefix(PREFIX)
                    .ok_or(Error::BadRow("cache helper event kind"))?;
                decode(&text(row, 1)?, nonce, &owner)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let next_nonce =
            (rows.len() > limit).then(|| items.last().expect("nonempty page").intent.nonce.clone());
        Ok(CacheHelperPage { items, next_nonce })
    }
}

fn read_helper(
    connection: &Connection,
    owner: &str,
    nonce: &str,
) -> Result<Option<CacheHelperRecord>, Error> {
    let rows = connection.query(
        "SELECT detail FROM events WHERE kind=? ORDER BY id DESC LIMIT 1",
        &[Value::Text(kind(nonce)?)],
        QueryLimits {
            max_rows: 1,
            max_bytes: RECORD_BYTES,
        },
    )?;
    rows.first()
        .map(|row| decode(&text(row, 0)?, nonce, owner))
        .transpose()
}

impl ReadOnlyRegistry {
    pub fn cache_helper(&self, nonce: &str) -> Result<Option<CacheHelperRecord>, Error> {
        read_helper(&self.connection, &self.registry_id()?, nonce)
    }
}

#[cfg(test)]
mod tests;
