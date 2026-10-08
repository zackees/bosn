//! Durable location proof for a relocated registry. Never follow redirect chains.

use super::*;
use std::io::Write;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema: u32,
    registry_id: String,
    database: PathBuf,
}

fn marker(path: &Path) -> PathBuf {
    path.with_extension("authority.json")
}

impl Registry {
    /// Publish a relocated writer's identity and location before accepting work.
    ///
    /// # Errors
    /// Refuses a different registry identity or a redirect chain.
    pub fn publish_authority(&self, original: &Path, database: &Path) -> Result<(), Error> {
        let database = std::fs::canonicalize(database)?;
        if marker(&database).try_exists()? {
            return Err(Error::BadRow("registry authority chain"));
        }
        let target = Self::open_read_only(&database)?;
        let registry_id = self.registry_id()?;
        if target.registry_id()? != registry_id {
            return Err(Error::BadRow("registry authority identity"));
        }
        let original_owner = Self::open_read_only(original)?.registry_id()?;
        if original_owner != registry_id {
            return Err(Error::BadRow("original registry authority identity"));
        }
        let destination = marker(original);
        let parent = destination
            .parent()
            .ok_or(Error::BadRow("authority directory"))?;
        let staging = fs::TemporaryDirectory::in_directory(parent, "authority-")?;
        let staged = staging.path().join("record");
        let authority = Authority {
            schema: 1,
            registry_id,
            database,
        };
        let bytes = serde_json::to_vec(&authority)
            .map_err(|_| Error::BadRow("registry authority serialization"))?;
        if bytes.len() > 16 * 1024 {
            return Err(Error::BadRow("registry authority record limit"));
        }
        let mut file = fs::create_private_file(&staged)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(staged, &destination)?;
        fs::sync_directory(parent)?;
        Ok(())
    }

    /// Resolve a bounded authority record and verify its immutable registry UUID.
    ///
    /// # Errors
    /// Refuses malformed, missing, chained, or identity-mismatched proof.
    pub fn resolve_authority(original: &Path) -> Result<PathBuf, Error> {
        let path = marker(original);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(original.into());
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.len() > 16 * 1024 {
            return Err(Error::BadRow("registry authority record limit"));
        }
        let authority: Authority = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|_| Error::BadRow("registry authority record"))?;
        if authority.schema != 1
            || !is_uuid(&authority.registry_id)
            || !authority.database.is_absolute()
            || marker(&authority.database).try_exists()?
        {
            return Err(Error::BadRow("registry authority schema or chain"));
        }
        let target = Connection::open_read_only_with_busy_timeout(
            &authority.database,
            std::time::Duration::from_secs(5),
        )?;
        Self::validate(&target, &authority.database)?;
        if meta(&target, "registry_id")?.as_deref() != Some(authority.registry_id.as_str()) {
            return Err(Error::BadRow("registry authority identity"));
        }
        if original.try_exists()? {
            let source = Connection::open_read_only_with_busy_timeout(
                original,
                std::time::Duration::from_secs(5),
            )?;
            Self::validate(&source, original)?;
            if meta(&source, "registry_id")?.as_deref() != Some(authority.registry_id.as_str()) {
                return Err(Error::BadRow("original registry authority identity"));
            }
        }
        Ok(authority.database)
    }
}
