//! Cross-daemon workload admission versus machine retention.
//!
//! Shared locks permit concurrent workloads. Exclusive retention locks prevent
//! a new ensure or exec between the final ownership read and Docker removal.

use kernal_api::async_engine;
use std::{fs::File, path::Path, time::Duration};

pub(crate) struct Guard(Option<File>);

#[derive(Debug)]
enum AdmissionError {
    Busy,
    Io(std::io::Error),
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("machine retention admission busy"),
            Self::Io(error) => write!(formatter, "machine retention admission failed: {error}"),
        }
    }
}

fn path() -> Option<std::path::PathBuf> {
    super::peers::machine_root().map(|root| root.join("retention-admission.lock"))
}

fn acquire(path: &Path, shared: bool) -> Result<File, AdmissionError> {
    let parent = path
        .parent()
        .ok_or_else(|| AdmissionError::Io(std::io::Error::other("retention gate has no parent")))?;
    crate::ipc::ensure_owner_private_directory(parent).map_err(AdmissionError::Io)?;
    let file = kernal_api::platform::fs::open_lock_file(path).map_err(AdmissionError::Io)?;
    let locked = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    locked.map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => AdmissionError::Busy,
        std::fs::TryLockError::Error(error) => AdmissionError::Io(error),
    })?;
    Ok(file)
}

/// Nonblocking: maintenance defers while any daemon is creating or executing work.
pub(super) fn retention() -> Result<Guard, String> {
    path()
        .map(|path| acquire(&path, false))
        .transpose()
        .map(Guard)
        .map_err(|error| error.to_string())
}

/// Wait asynchronously without preventing the job actor from draining logs.
pub(crate) async fn workload(
    deadline: Duration,
    cancellation: &async_engine::CancellationToken,
) -> Result<Guard, String> {
    let Some(path) = path() else {
        return Ok(Guard(None));
    };
    let deadline = async_engine::Deadline::after(deadline);
    loop {
        if cancellation.is_cancelled() {
            return Err("workload admission cancelled".into());
        }
        match acquire(&path, true) {
            Ok(file) => return Ok(Guard(Some(file))),
            Err(AdmissionError::Busy) if !deadline.remaining().is_zero() => {
                async_engine::sleep(Duration::from_millis(50)).await
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Unlock before close: a concurrent spawn can briefly inherit the
        // open-file description, delaying close-only lock release until exec.
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
        drop(self.0.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workloads_overlap_but_exclude_retention_until_the_last_reader_finishes() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = root.path().join("admission.lock");
        let first = acquire(&path, true).unwrap();
        let second = acquire(&path, true).unwrap();
        assert!(acquire(&path, false).is_err());
        drop(first);
        assert!(acquire(&path, false).is_err());
        drop(second);
        let retention = acquire(&path, false).unwrap();
        assert!(acquire(&path, true).is_err());
        assert!(acquire(&path, false).is_err());
        drop(retention);
        assert!(acquire(&path, true).is_ok());
    }
}
