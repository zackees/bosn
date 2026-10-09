//! Read-only engine reads used by age-gated reclamation (`bosn gc owned`, #456).
//!
//! These live apart from `lib.rs` because they are one responsibility — answering "what does
//! this engine hold, and is anything still using it?" — and `lib.rs` is already at the
//! project's file-length ceiling. None of them mutates engine state.

use super::{
    CensusRead, CommandError, CommandResult, DockerEngine, RunOptions, census_read, split_lines,
};
use std::ffi::OsString;

impl DockerEngine {
    /// Read-only: the ids of containers carrying one label key, running or stopped.
    ///
    /// `-a` is essential: a stopped container is the thing an age-gated reaper must be able to
    /// find, and without it a reaper could only ever see live work.
    pub fn container_ids_with_label(
        &self,
        key: &str,
        options: RunOptions,
    ) -> Result<CensusRead, CommandError> {
        let filter = format!("label={key}");
        let result = self
            .with_args(["ps", "-a", "-q", "--no-trunc", "--filter", filter.as_str()])
            .capture(options)?;
        Ok(census_read(result, "docker ps -a --filter label"))
    }

    /// Read-only: the names of volumes carrying one label key.
    pub fn volume_names_with_label(
        &self,
        key: &str,
        options: RunOptions,
    ) -> Result<CensusRead, CommandError> {
        let filter = format!("label={key}");
        let result = self
            .with_args(["volume", "ls", "-q", "--filter", filter.as_str()])
            .capture(options)?;
        Ok(census_read(result, "docker volume ls --filter label"))
    }

    /// Read-only: detail for specific containers, as one bounded `docker inspect`.
    ///
    /// `docker ps --format` joins labels into a comma-separated string and renders the creation
    /// time for humans. Inspect is the read that returns them as real JSON, which is what the
    /// typed observation structs deserialize against.
    pub fn inspect_containers(
        &self,
        ids: &[String],
        options: RunOptions,
    ) -> Result<CensusRead, CommandError> {
        // `--size`: Docker reports `SizeRw` only when asked (#549).
        let mut args: Vec<OsString> = ["inspect", "--type", "container", "--size"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(ids.iter().cloned().map(OsString::from));
        let result = self.with_args(args).capture(options)?;
        Ok(inspect_read(result, "docker inspect"))
    }

    /// Read-only: detail for specific images, as one bounded `docker image inspect`.
    pub fn inspect_images(
        &self,
        ids: &[String],
        options: RunOptions,
    ) -> Result<CensusRead, CommandError> {
        let mut args: Vec<OsString> = ["image", "inspect"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(ids.iter().cloned().map(OsString::from));
        let result = self.with_args(args).capture(options)?;
        Ok(inspect_read(result, "docker image inspect"))
    }

    /// Read-only: whether one container is running right now.
    pub fn container_is_running(&self, id: &str, options: RunOptions) -> bool {
        self.with_args(["inspect", "--format", "{{.State.Running}}", id])
            .capture(options)
            .is_ok_and(|result| {
                result.ok() && String::from_utf8_lossy(&result.stdout).trim() == "true"
            })
    }

    /// Read-only: container ids currently mounting one volume.
    ///
    /// An empty list is Docker's own verdict that nothing references the volume. A caller must
    /// treat a failed read as "still in use", because "I could not check" is not "unused".
    pub fn container_ids_using_volume(
        &self,
        name: &str,
        options: RunOptions,
    ) -> Result<Vec<String>, CommandError> {
        let filter = format!("volume={name}");
        let result = self
            .with_args(["ps", "-a", "-q", "--no-trunc", "--filter", filter.as_str()])
            .capture(options)?;
        Ok(split_lines(result))
    }

    /// Read-only: container ids created from one image.
    pub fn container_ids_using_image(
        &self,
        id: &str,
        options: RunOptions,
    ) -> Result<Vec<String>, CommandError> {
        let filter = format!("ancestor={id}");
        let result = self
            .with_args(["ps", "-a", "-q", "--no-trunc", "--filter", filter.as_str()])
            .capture(options)?;
        Ok(split_lines(result))
    }
}

/// A read of several named objects. Docker prints the ones it found and names each missing one
/// on stderr, exiting 1, so an object removed between a listing and this read is simply absent
/// from the document (#550). Any other failure leaves the read unavailable.
pub(crate) fn inspect_read(result: CommandResult, what: &str) -> CensusRead {
    if result.reports_missing() {
        return match String::from_utf8(result.stdout) {
            Ok(text) if text.trim().is_empty() => CensusRead::Document("[]".to_owned()),
            Ok(text) => CensusRead::Document(text),
            Err(_) => CensusRead::Unavailable {
                detail: format!("{what} returned non-UTF-8 output"),
            },
        };
    }
    census_read(result, what)
}
