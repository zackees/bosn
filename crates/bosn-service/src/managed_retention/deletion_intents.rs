//! Write deletion proof before Docker mutates the exact physical object.

use super::*;
use std::io::Write;

const LIMIT: usize = 1024;
const MAX_BYTES: usize = 16 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    physical_id: String,
    physical_name: String,
    labels: BTreeMap<String, String>,
    observed_at: f64,
}

fn document(receipt: &DeletionReceipt) -> Result<Vec<u8>, String> {
    let digest = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let identity_valid = match receipt.labels.kind {
        ResourceKind::Container => digest(&receipt.physical_id),
        ResourceKind::Image => receipt
            .physical_id
            .strip_prefix("sha256:")
            .is_some_and(digest),
        ResourceKind::Volume => {
            !receipt.physical_id.is_empty()
                && receipt.physical_id == receipt.physical_name
                && receipt
                    .physical_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        }
        _ => false,
    };
    if !identity_valid
        || receipt.physical_name.is_empty()
        || receipt
            .labels
            .created
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .or_else(|| parse_docker_time(&receipt.labels.created))
            .is_none()
    {
        return Err("deletion intent has invalid physical identity".into());
    }
    let stored = Stored {
        physical_id: receipt.physical_id.clone(),
        physical_name: receipt.physical_name.clone(),
        labels: receipt
            .labels
            .to_map()
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
        observed_at: receipt.observed_at,
    };
    let bytes = serde_json::to_vec(&stored).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_BYTES || !receipt.observed_at.is_finite() {
        return Err("deletion intent exceeds proof bounds".into());
    }
    Ok(bytes)
}

fn path(receipt: &DeletionReceipt) -> Result<std::path::PathBuf, String> {
    let database =
        bosn_registry::Registry::resolve_authority(&receipt.state_dir.join("registry.sqlite3"))
            .map_err(|error| error.to_string())?;
    let parent = database
        .parent()
        .ok_or("deletion authority has no parent")?;
    let identity = serde_json::to_vec(&(
        &receipt.labels.registry,
        receipt.labels.kind.as_str(),
        receipt.labels.scope.as_str(),
        &receipt.labels.created,
        &receipt.physical_id,
        &receipt.labels.stack,
        &receipt.labels.generation,
        &receipt.labels.workspace,
    ))
    .map_err(|error| error.to_string())?;
    Ok(parent.join("deletion-intents").join(format!(
        "{}.json",
        kernal_api::hash::sha256_bytes(&identity).to_hex()
    )))
}

pub(super) fn load(state: &Path) -> Result<Vec<DeletionReceipt>, String> {
    use std::io::Read;
    let database = bosn_registry::Registry::resolve_authority(&state.join("registry.sqlite3"))
        .map_err(|error| error.to_string())?;
    let directory = database
        .parent()
        .ok_or("deletion authority has no parent")?
        .join("deletion-intents");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut receipts = Vec::new();
    for entry in entries {
        budget::check()?;
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        if receipts.len() >= LIMIT {
            return Err("deletion intent enumeration limit".into());
        }
        let metadata =
            std::fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
            return Err("invalid deletion intent file".into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(entry.path())
            .map_err(|error| error.to_string())?
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("deletion intent grew past bound".into());
        }
        let stored: Stored = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let labels = bosn_core::ResourceLabels::parse(&stored.labels)
            .map_err(|error| format!("{error:?}"))?;
        let receipt = DeletionReceipt {
            state_dir: state.to_path_buf(),
            physical_id: stored.physical_id,
            physical_name: stored.physical_name,
            labels,
            observed_at: stored.observed_at,
        };
        if path(&receipt)? != entry.path() || !receipt.observed_at.is_finite() {
            return Err("deletion intent identity mismatch".into());
        }
        document(&receipt)?;
        receipts.push(receipt);
    }
    Ok(receipts)
}

pub(super) fn record(receipt: &DeletionReceipt) -> Result<(), String> {
    let document = document(receipt)?;
    let destination = path(receipt)?;
    let directory = destination
        .parent()
        .ok_or("deletion intent has no directory")?;
    crate::ipc::ensure_owner_private_directory(directory).map_err(|error| error.to_string())?;
    if !destination
        .try_exists()
        .map_err(|error| error.to_string())?
    {
        let mut count = 0;
        for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                count += 1;
            }
            if count >= LIMIT {
                return Err("deletion intent ceiling reached; recovery required".into());
            }
        }
    }
    let staging =
        kernal_api::platform::fs::TemporaryDirectory::in_directory(directory, "deletion-")
            .map_err(|error| error.to_string())?;
    let staged = staging.path().join("intent");
    let mut file = std::fs::File::create(&staged).map_err(|error| error.to_string())?;
    file.write_all(&document)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    std::fs::rename(staged, &destination).map_err(|error| error.to_string())?;
    kernal_api::platform::fs::sync_directory(directory).map_err(|error| error.to_string())
}

pub(crate) fn acknowledge(receipt: &DeletionReceipt) -> Result<(), String> {
    let destination = path(receipt)?;
    let bytes = match std::fs::read(&destination) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if bytes != document(receipt)? {
        return Err("deletion intent changed before acknowledgement".into());
    }
    std::fs::remove_file(&destination).map_err(|error| error.to_string())?;
    kernal_api::platform::fs::sync_directory(
        destination
            .parent()
            .ok_or("deletion directory unavailable")?,
    )
    .map_err(|error| error.to_string())
}
