//! Durable ownership exports with a crash marker preceding every transaction.

use super::*;
use std::io::Write;

impl Registry {
    /// Refresh a copy whose caller has proven it was never published.
    /// Both source and destination writer fences span the replacement.
    ///
    /// # Errors
    /// Refuses redirects, different identities, live writers, or journal files.
    pub fn refresh_unpublished_copy(&mut self, destination: &Path) -> Result<(), Error> {
        if Self::resolve_authority(destination)? != destination {
            return Err(Error::BadRow("unpublished copy has authority"));
        }
        let snapshot = Self::open_writer(destination)?;
        if snapshot.registry_id()? != self.registry_id()? {
            return Err(Error::BadRow("unpublished copy identity"));
        }
        if snapshot.connection.checkpoint()?.busy {
            return Err(Error::BadRow("unpublished copy checkpoint busy"));
        }
        let Registry {
            connection,
            _writer: writer_lock,
            prior_writers,
            ..
        } = snapshot;
        drop(connection);
        for suffix in ["-wal", "-journal", "-shm"] {
            let mut sidecar = destination.as_os_str().to_owned();
            sidecar.push(suffix);
            if PathBuf::from(sidecar).try_exists()? {
                return Err(Error::BadRow("unpublished copy has journal state"));
            }
        }
        let parent = destination
            .parent()
            .ok_or(Error::BadRow("copy directory"))?;
        let staging = fs::TemporaryDirectory::in_directory(parent, "refresh-")?;
        let staged = staging.path().join("registry.sqlite3");
        self.backup_ownership(&staged)?;
        std::fs::rename(staged, destination)?;
        fs::sync_directory(parent)?;
        let connection =
            Connection::open_with_busy_timeout(destination, std::time::Duration::from_secs(5))?;
        Self::validate(&connection, destination)?;
        let replacement = Registry {
            connection,
            _writer: writer_lock,
            prior_writers,
            ownership_backup: None,
            ownership_backup_dirty: false,
        };
        let previous = std::mem::replace(self, replacement);
        self.prior_writers.extend(previous.prior_writers);
        self.prior_writers.push(previous._writer);
        Ok(())
    }

    /// Resume a previously published stable database, retaining the old fence.
    ///
    /// # Errors
    /// Refuses a busy writer or a different immutable registry identity.
    pub fn resume_authoritative_writer(&mut self, destination: &Path) -> Result<(), Error> {
        if self.prior_writers.len() >= 8 {
            return Err(Error::BadRow("registry relocation limit"));
        }
        let replacement = Self::open_writer(destination)?;
        if replacement.registry_id()? != self.registry_id()? {
            return Err(Error::BadRow("resumed registry identity"));
        }
        let previous = std::mem::replace(self, replacement);
        self.prior_writers.extend(previous.prior_writers);
        self.prior_writers.push(previous._writer);
        Ok(())
    }

    /// Move future transactions to a machine-stable SQLite database while
    /// retaining writer exclusion on both databases throughout the handoff.
    /// The caller must durably publish the new location before acknowledging
    /// further work. The original database becomes a historical snapshot.
    ///
    /// # Errors
    /// Refuses an existing destination or any failed backup/identity check.
    pub fn relocate_writer(&mut self, destination: impl AsRef<Path>) -> Result<(), Error> {
        if self.prior_writers.len() >= 8 {
            return Err(Error::BadRow("registry relocation limit"));
        }
        let destination = destination.as_ref();
        let owner = self.registry_id()?;
        self.backup_ownership(destination)?;
        let replacement = Self::open_writer(destination)?;
        if replacement.registry_id()? != owner {
            return Err(Error::BadRow("relocated registry identity"));
        }
        let previous = std::mem::replace(self, replacement);
        self.prior_writers = previous.prior_writers;
        self.prior_writers.push(previous._writer);
        Ok(())
    }

    /// Configure a private, machine-stable directory while its owner lock is held.
    pub fn enable_ownership_backup(&mut self, directory: PathBuf) -> Result<(), Error> {
        self.ownership_backup = Some(directory);
        self.mark_ownership_backup_dirty()?;
        self.publish_ownership_backup()
    }

    pub(crate) fn mark_ownership_backup_dirty(&mut self) -> Result<(), Error> {
        if let Some(directory) = &self.ownership_backup {
            write_marker(directory, b"dirty\n")?;
            self.ownership_backup_dirty = true;
        }
        Ok(())
    }

    /// Publish before acknowledging a mutation. A crash or failed export leaves
    /// the dirty marker intact, so recovery cannot trust an older snapshot.
    pub fn publish_ownership_backup(&mut self) -> Result<(), Error> {
        if !self.ownership_backup_dirty {
            return Ok(());
        }
        let Some(directory) = &self.ownership_backup else {
            return Ok(());
        };
        let staging = fs::TemporaryDirectory::in_directory(directory, "ownership-")?;
        let staged = staging.path().join("registry.sqlite3");
        self.backup_ownership(&staged)?;
        std::fs::rename(staged, directory.join("registry.sqlite3"))?;
        fs::sync_directory(directory)?;
        write_marker(directory, b"clean\n")?;
        self.ownership_backup_dirty = false;
        Ok(())
    }
}

fn write_marker(directory: &Path, value: &[u8]) -> Result<(), Error> {
    let staging = fs::TemporaryDirectory::in_directory(directory, "marker-")?;
    let path = staging.path().join("status");
    let mut file = fs::create_private_file(&path)?;
    file.write_all(value)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(path, directory.join("ownership-status"))?;
    fs::sync_directory(directory)?;
    Ok(())
}
