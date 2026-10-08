//! Inspect every existing cache attachment before changing repository routing.
use super::{CACHE_VOLUME, CONTROL_DEADLINE, DockerActBackend, owned};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const COORDINATION: &str = "com.zackees.bosn.act.cache-coordination";
const PARTICIPATING: &str = "shared-legacy-lease-v1";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CacheWriter {
    id: String,
    config: Config,
    mounts: Vec<Mount>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    labels: Option<BTreeMap<String, String>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Mount {
    #[serde(rename = "Type")]
    kind: String,
    name: Option<String>,
    #[serde(rename = "RW")]
    writable: bool,
}

impl DockerActBackend {
    pub(super) async fn require_coordinated_cache_writers(&self) -> Result<(), String> {
        let filter = format!("volume={CACHE_VOLUME}");
        let output = self
            .checked(
                "cache writer inventory",
                owned(&["ps", "-a", "-q", "--no-trunc", "--filter", &filter]),
                CONTROL_DEADLINE,
            )
            .await?;
        let ids = parse_ids(&output)?;
        for batch in ids.iter().collect::<Vec<_>>().chunks(32) {
            let mut args = owned(&["container", "inspect"]);
            args.extend(batch.iter().map(|id| (*id).clone()));
            let inspected = self
                .checked("cache writer details", args, CONTROL_DEADLINE)
                .await?;
            require_participants(inspected.as_bytes(), batch)?;
        }
        Ok(())
    }
}

fn parse_ids(output: &str) -> Result<BTreeSet<String>, String> {
    let mut ids = BTreeSet::new();
    for id in output.lines() {
        if id.len() != 64
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !ids.insert(id.into())
            || ids.len() > 1024
        {
            return Err("cache writer inventory is invalid or exceeds 1024 attachments".into());
        }
    }
    Ok(ids)
}

fn require_participants(document: &[u8], expected: &[&String]) -> Result<(), String> {
    let writers: Vec<CacheWriter> = serde_json::from_slice(document)
        .map_err(|_| "cache writer detail is incomplete or invalid")?;
    let mut observed = BTreeSet::new();
    for writer in writers {
        if !expected.iter().any(|id| **id == writer.id) || !observed.insert(writer.id.clone()) {
            return Err("cache writer detail does not match the requested inventory".into());
        }
        let attachments: Vec<_> = writer
            .mounts
            .iter()
            .filter(|mount| mount.kind == "volume" && mount.name.as_deref() == Some(CACHE_VOLUME))
            .collect();
        if attachments.is_empty() {
            return Err("cache attachment changed during writer inventory; retry".into());
        }
        let coordination = writer
            .config
            .labels
            .as_ref()
            .and_then(|labels| labels.get(COORDINATION))
            .map(String::as_str);
        if attachments.iter().any(|mount| mount.writable)
            && !matches!(
                coordination,
                Some(PARTICIPATING | "shared-machine-maintenance-v1")
            )
        {
            return Err(format!(
                "cache enrollment held by uncoordinated container {}; finish its work and upgrade its Bosn producer before retrying",
                writer.id
            ));
        }
    }
    if observed.len() != expected.len() {
        return Err("cache writer detail omitted a requested attachment".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_old_and_incomplete_writer_proofs_refuse_enrollment() {
        let id = "a".repeat(64);
        for mode in [
            "coordinated",
            "read-only",
            "legacy",
            "missing-labels",
            "missing-mounts",
            "missing-id",
        ] {
            let document = serde_json::json!([{
                "Id": id,
                "Config": {"Labels": if mode == "missing-labels" { None } else {
                    Some(BTreeMap::from([(COORDINATION, if mode == "legacy" {"older"} else {PARTICIPATING})]))
                }},
                "Mounts": [{"Type":"volume", "Name":CACHE_VOLUME,"RW":mode != "read-only"}]
            }]);
            let mut document = serde_json::to_string(&document).unwrap();
            if mode == "missing-mounts" {
                document = document.replace("Mounts", "Absent");
            }
            if mode == "missing-id" {
                document = document.replace(&id, &"b".repeat(64));
            }
            assert_eq!(
                require_participants(document.as_bytes(), &[&id]).is_ok(),
                matches!(mode, "coordinated" | "read-only"),
                "{mode}"
            );
        }
        assert!(require_participants(b"[]", &[&id]).is_err());
        assert!(parse_ids(&format!("{id}\n{id}")).is_err());
        assert!(parse_ids("short").is_err());
    }
}
