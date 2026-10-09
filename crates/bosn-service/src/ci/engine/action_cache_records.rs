//! Typed custody for coarse eviction of the owned action-cache class.
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retirement {
    pub schema_version: u32,
    pub nonce: String,
    pub cache: Identity,
    pub source: Identity,
}

impl Retirement {
    pub(super) fn validate(&self, cache: Identity) -> Result<(), String> {
        if self.schema_version != 1
            || !crate::ci::wire::valid_uuid(&self.nonce)
            || self.cache != cache
            || self.source.device != cache.device
            || self.source.inode == 0
            || self.source.inode == cache.inode
        {
            return Err(
                "cache retirement authority differs from the original cache or class".into(),
            );
        }
        Ok(())
    }

    pub(super) fn stage(&self, class: &str) -> String {
        format!("{}/.{class}-retired-{}", super::ENGINE_CACHE, self.nonce)
    }

    pub(super) fn argument(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.nonce, self.cache.device, self.cache.inode, self.source.device, self.source.inode
        )
    }
}

pub(super) struct Observation {
    pub cache: Identity,
    pub source: Option<(Identity, u64)>,
}

impl Observation {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        let rows: Vec<_> = text.lines().filter(|row| !row.is_empty()).collect();
        if rows.len() != 2 {
            return Err("cache observation lacks exact root/class rows".into());
        }
        let cache: Vec<_> = rows[0].split_whitespace().collect();
        let source: Vec<_> = rows[1].split_whitespace().collect();
        if cache.len() != 3 || cache[0] != "cache" {
            return Err("cache root identity is invalid".into());
        }
        let cache = identity(&cache[1..])?;
        let source = if source == ["source", "absent"] {
            None
        } else {
            if source.len() != 4 || source[0] != "source" {
                return Err("cache class observation is invalid".into());
            }
            let id = identity(&source[1..3])?;
            let blocks = number(source[3])?;
            Some((
                id,
                blocks
                    .checked_mul(1024)
                    .ok_or("cache allocation overflows")?,
            ))
        };
        if source.is_some_and(|(id, _)| id.device != cache.device || id.inode == cache.inode) {
            return Err("cache class crosses the original cache filesystem".into());
        }
        Ok(Self { cache, source })
    }
}

fn identity(fields: &[&str]) -> Result<Identity, String> {
    let value = Identity {
        device: number(fields[0])?,
        inode: number(fields[1])?,
    };
    if value.inode == 0 {
        return Err("cache inode identity is invalid".into());
    }
    Ok(value)
}

fn number(value: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("cache observation contains an invalid integer".into());
    }
    value
        .parse()
        .map_err(|_| "cache observation integer overflows".into())
}

/// Same-device bind mounts are boundaries too; a device-only census is insufficient.
pub(super) fn require_no_submounts(
    table: &[u8],
    class: &str,
    stage: Option<&str>,
) -> Result<(), String> {
    let table = std::str::from_utf8(table).map_err(|e| e.to_string())?;
    let rows: Vec<_> = table.lines().collect();
    if rows.is_empty() || rows.len() > 4096 {
        return Err("cache mount census is empty or exceeds bounds".into());
    }
    for row in rows {
        let fields: Vec<_> = row.split_whitespace().collect();
        let separator = fields
            .iter()
            .position(|f| *f == "-")
            .ok_or("cache mount census lacks separator")?;
        if separator < 6 || fields.len() < separator + 4 {
            return Err("cache mount census is incomplete".into());
        }
        number(fields[0])?;
        number(fields[1])?;
        let mount = decode_mount(fields[4])?;
        let path = Path::new(&mount);
        if !path.is_absolute() {
            return Err("cache mount path is not absolute".into());
        }
        if path.starts_with(class) || stage.is_some_and(|stage| path.starts_with(stage)) {
            return Err("cache retirement held by a nested mount boundary".into());
        }
    }
    Ok(())
}

fn decode_mount(value: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut remaining = value;
    while let Some(at) = remaining.find('\\') {
        result.push_str(&remaining[..at]);
        let escaped = remaining
            .get(at + 1..at + 4)
            .ok_or("cache mount escape is truncated")?;
        result.push(match escaped {
            "040" => ' ',
            "011" => '\t',
            "012" => '\n',
            "134" => '\\',
            _ => return Err("cache mount escape is unknown".into()),
        });
        remaining = &remaining[at + 4..];
    }
    result.push_str(remaining);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_cache_or_source_authority_cannot_retire_a_tree() {
        let cache = Identity {
            device: 7,
            inode: 8,
        };
        let mut proof = Retirement {
            schema_version: 1,
            nonce: "12345678-1234-1234-1234-123456789abc".into(),
            cache,
            source: Identity {
                device: 7,
                inode: 9,
            },
        };
        assert!(proof.validate(cache).is_ok());
        proof.cache.inode = 10;
        assert!(proof.validate(cache).is_err());
        proof.cache = cache;
        proof.source = cache;
        assert!(proof.validate(cache).is_err());
        proof.source.inode = 9;
        proof.nonce = "../foreign".into();
        assert!(proof.validate(cache).is_err());
    }
    #[test]
    fn mount_census_protects_same_device_bind_mounts_and_partial_tables() {
        let row = |path| format!("1 2 0:3 / {path} rw - tmpfs tmpfs rw\n");
        assert!(
            require_no_submounts(row("/bosn/cache").as_bytes(), "/bosn/cache/actions", None)
                .is_ok()
        );
        assert!(
            require_no_submounts(
                row("/bosn/cache/actions-neighbor").as_bytes(),
                "/bosn/cache/actions",
                None
            )
            .is_ok()
        );
        assert!(
            require_no_submounts(
                row("/bosn/cache/actions/repo.git").as_bytes(),
                "/bosn/cache/actions",
                None
            )
            .is_err()
        );
        assert!(require_no_submounts(b"", "/bosn/cache/actions", None).is_err());
        assert!(require_no_submounts(b"1 2 3", "/bosn/cache/actions", None).is_err());
    }
}
