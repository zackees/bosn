//! Daemon-owned task secrets (#308).
//!
//! A manifest task opts into a secret by name only (`secrets = ["github_token"]`).
//! The value lives in `STATE_DIR/secrets/<name>`: a regular, non-symlinked file
//! with no group or world permission bits, refused otherwise exactly like the
//! guest SSH identity. The value is handed to the Docker client through its
//! process environment and forwarded by name (`--env GITHUB_TOKEN`), so it is
//! never in argv. Task output is passed through [`SecretMasker`] before it is
//! logged, streamed, or used in an error message.

use std::path::{Path, PathBuf};

/// The only secret names Bosn knows, and the variable each is injected as.
pub use bosn_core::MANIFEST_TASK_SECRETS;

/// The public replacement for a secret in any output Bosn relays.
pub const MASK: &str = "***";

/// A secret value is a single token, not a document.
const MAX_SECRET_BYTES: usize = 4096;

/// The environment variable a known secret is injected as.
#[must_use]
pub fn secret_env_name(name: &str) -> Option<&'static str> {
    MANIFEST_TASK_SECRETS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, env)| *env)
}

fn known(name: &str) -> Result<(), String> {
    if secret_env_name(name).is_none() {
        return Err(format!(
            "unknown secret {name:?}; known secrets: {}",
            MANIFEST_TASK_SECRETS
                .iter()
                .map(|(known, _)| *known)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// `STATE_DIR/secrets`.
#[must_use]
pub fn secrets_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("secrets")
}

/// The presence of one secret, without its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretState {
    Missing,
    Present,
    /// Present on disk but refused (symlink, loose mode, bad content). The
    /// reason never contains the value.
    Refused(String),
}

/// Read one secret. `Ok(None)` means it is not provisioned. Every refusal
/// message is value-free.
pub fn read_secret(state_dir: &Path, name: &str) -> Result<Option<String>, String> {
    known(name)?;
    let dir = secrets_dir(state_dir);
    let path = dir.join(name);
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("secret directory cannot be inspected".into()),
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err("secret directory must be a real directory, not a symlink".into());
        }
        Ok(_) => {}
    }
    let meta = match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("secret {name} cannot be inspected")),
        Ok(meta) => meta,
    };
    if meta.file_type().is_symlink() {
        return Err(format!("secret {name} must not be a symlink"));
    }
    if !meta.file_type().is_file() {
        return Err(format!("secret {name} is not a regular file"));
    }
    // Reject a symlinked parent component as well, rather than trusting
    // where it happens to point today.
    let canonical_dir = std::fs::canonicalize(&dir)
        .map_err(|_| "secret directory cannot be canonicalized".to_owned())?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|_| format!("secret {name} cannot be canonicalized"))?;
    if canonical != canonical_dir.join(name) {
        return Err(format!("secret {name} must not traverse a symlink"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 {
            return Err(format!(
                "secret {name} must not be group- or world-accessible (chmod 600)"
            ));
        }
    }
    if meta.len() > MAX_SECRET_BYTES as u64 {
        return Err(format!("secret {name} is too large"));
    }
    let bytes = std::fs::read(&path).map_err(|_| format!("secret {name} cannot be read"))?;
    let value = validate_value(&bytes).map_err(|reason| format!("secret {name} {reason}"))?;
    Ok(Some(value))
}

fn validate_value(bytes: &[u8]) -> Result<String, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "is not UTF-8")?;
    let value = text.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        return Err("is empty");
    }
    if value.len() > MAX_SECRET_BYTES {
        return Err("is too large");
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must be a single token without whitespace");
    }
    Ok(value.to_owned())
}

/// Presence of every known secret; never a value.
#[must_use]
pub fn secret_status(state_dir: &Path) -> Vec<(&'static str, SecretState)> {
    MANIFEST_TASK_SECRETS
        .iter()
        .map(|(name, _)| {
            let state = match read_secret(state_dir, name) {
                Ok(Some(_)) => SecretState::Present,
                Ok(None) => SecretState::Missing,
                Err(reason) => SecretState::Refused(reason),
            };
            (*name, state)
        })
        .collect()
}

/// Atomically write one secret with mode 0600 (directory 0700).
pub fn write_secret(state_dir: &Path, name: &str, raw: &[u8]) -> Result<(), String> {
    known(name)?;
    let value = validate_value(raw).map_err(|reason| format!("secret value {reason}"))?;
    let dir = secrets_dir(state_dir);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err("secret directory must be a real directory, not a symlink".into());
        }
        Ok(_) => {}
        Err(_) => {
            std::fs::create_dir_all(&dir)
                .map_err(|_| "secret directory cannot be created".to_owned())?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "secret directory permissions cannot be set".to_owned())?;
    }
    let target = dir.join(name);
    let staging = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        use std::io::Write;
        let mut file = options.open(&staging)?;
        file.write_all(value.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&staging, &target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("secret {name} cannot be written"));
    }
    Ok(())
}

/// Remove one secret. Removing a missing secret succeeds.
pub fn remove_secret(state_dir: &Path, name: &str) -> Result<(), String> {
    known(name)?;
    match std::fs::remove_file(secrets_dir(state_dir).join(name)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(format!("secret {name} cannot be removed")),
    }
}

/// Streaming replacement of secret values with [`MASK`].
///
/// Each stream keeps its own pending tail, so a value split across two writes
/// is still masked and bytes from stdout and stderr can never be joined into
/// one match. Only a proper prefix of a secret is ever held back.
#[derive(Default)]
pub struct SecretMasker {
    secrets: Vec<Vec<u8>>,
    pending: [Vec<u8>; 2],
}

impl std::fmt::Debug for SecretMasker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretMasker")
            .field("secrets", &self.secrets.len())
            .finish_non_exhaustive()
    }
}

/// Output stream selector for [`SecretMasker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskStream {
    Stdout = 0,
    Stderr = 1,
}

impl SecretMasker {
    #[must_use]
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut secrets: Vec<Vec<u8>> = secrets
            .into_iter()
            .map(|value| value.as_ref().as_bytes().to_vec())
            .filter(|value| !value.is_empty())
            .collect();
        // Longest first so an overlapping longer secret wins.
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self {
            secrets,
            pending: [Vec::new(), Vec::new()],
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Mask one chunk of `stream`; returns the bytes safe to emit now.
    pub fn push(&mut self, stream: MaskStream, chunk: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return chunk.to_vec();
        }
        let slot = stream as usize;
        let mut buffer = std::mem::take(&mut self.pending[slot]);
        buffer.extend_from_slice(chunk);
        let (output, rest) = self.scan(&buffer, false);
        self.pending[slot] = rest;
        output
    }

    /// Flush a stream's held-back tail at end of output.
    pub fn finish(&mut self, stream: MaskStream) -> Vec<u8> {
        let buffer = std::mem::take(&mut self.pending[stream as usize]);
        self.scan(&buffer, true).0
    }

    /// Mask a complete text (error messages, summaries).
    #[must_use]
    pub fn mask_text(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_owned();
        }
        String::from_utf8_lossy(&self.scan(text.as_bytes(), true).0).into_owned()
    }

    fn scan(&self, buffer: &[u8], at_end: bool) -> (Vec<u8>, Vec<u8>) {
        let mut output = Vec::with_capacity(buffer.len());
        let mut index = 0;
        'outer: while index < buffer.len() {
            let rest = &buffer[index..];
            for secret in &self.secrets {
                if rest.starts_with(secret) {
                    output.extend_from_slice(MASK.as_bytes());
                    index += secret.len();
                    continue 'outer;
                }
            }
            if !at_end
                && self
                    .secrets
                    .iter()
                    .any(|secret| rest.len() < secret.len() && secret.starts_with(rest))
            {
                return (output, rest.to_vec());
            }
            output.push(buffer[index]);
            index += 1;
        }
        (output, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "ghp_canary308SECRETvalue0123456789";

    fn private_state() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn masks_a_whole_and_a_split_canary_per_stream() {
        let mut masker = SecretMasker::new([CANARY]);
        let whole = masker.push(MaskStream::Stdout, format!("a {CANARY} b\n").as_bytes());
        assert_eq!(whole, b"a *** b\n");
        let (left, right) = CANARY.split_at(11);
        let mut out = masker.push(MaskStream::Stdout, format!("x {left}").as_bytes());
        // Interleaved stderr must neither leak nor complete the stdout half.
        let err = masker.push(MaskStream::Stderr, right.as_bytes());
        out.extend(masker.push(MaskStream::Stdout, format!("{right} y").as_bytes()));
        out.extend(masker.finish(MaskStream::Stdout));
        assert_eq!(out, b"x *** y");
        let mut err = err;
        err.extend(masker.finish(MaskStream::Stderr));
        assert_eq!(err, right.as_bytes());
        // Split byte by byte.
        let mut bytes = Vec::new();
        for byte in CANARY.as_bytes() {
            bytes.extend(masker.push(MaskStream::Stderr, &[*byte]));
        }
        bytes.extend(masker.finish(MaskStream::Stderr));
        assert_eq!(bytes, b"***");
    }

    #[test]
    fn a_held_prefix_that_never_completes_is_flushed_unchanged() {
        let mut masker = SecretMasker::new([CANARY]);
        let mut out = masker.push(MaskStream::Stdout, b"ghp_can");
        assert!(out.is_empty());
        out.extend(masker.push(MaskStream::Stdout, b"dle"));
        out.extend(masker.finish(MaskStream::Stdout));
        assert_eq!(out, b"ghp_candle");
        assert_eq!(masker.mask_text(&format!("e: {CANARY}")), "e: ***");
        assert_eq!(
            SecretMasker::new(Vec::<String>::new()).mask_text(CANARY),
            CANARY
        );
    }

    #[test]
    fn write_then_read_round_trips_with_private_mode() {
        let state = private_state();
        assert_eq!(read_secret(state.path(), "github_token").unwrap(), None);
        write_secret(
            state.path(),
            "github_token",
            format!("{CANARY}\n").as_bytes(),
        )
        .unwrap();
        assert_eq!(
            read_secret(state.path(), "github_token")
                .unwrap()
                .as_deref(),
            Some(CANARY)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let file = secrets_dir(state.path()).join("github_token");
            assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::metadata(secrets_dir(state.path())).unwrap().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            secret_status(state.path()),
            vec![("github_token", SecretState::Present)]
        );
        remove_secret(state.path(), "github_token").unwrap();
        assert_eq!(read_secret(state.path(), "github_token").unwrap(), None);
    }

    #[test]
    fn unknown_names_and_bad_values_are_refused() {
        let state = private_state();
        assert!(write_secret(state.path(), "aws_key", b"x").is_err());
        assert!(read_secret(state.path(), "../github_token").is_err());
        assert!(write_secret(state.path(), "github_token", b"").is_err());
        assert!(write_secret(state.path(), "github_token", b"two words").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_and_group_or_world_readable_secrets_are_refused_without_the_value() {
        use std::os::unix::fs::PermissionsExt;
        let state = private_state();
        write_secret(state.path(), "github_token", CANARY.as_bytes()).unwrap();
        let file = secrets_dir(state.path()).join("github_token");
        for mode in [0o640, 0o604, 0o660] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = read_secret(state.path(), "github_token").unwrap_err();
            assert!(error.contains("group- or world"), "{error}");
            assert!(!error.contains(CANARY));
        }
        std::fs::remove_file(&file).unwrap();

        let outside = state.path().join("outside");
        std::fs::write(&outside, CANARY).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&outside, &file).unwrap();
        let error = read_secret(state.path(), "github_token").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!error.contains(CANARY));
        assert!(matches!(
            secret_status(state.path())[0].1,
            SecretState::Refused(_)
        ));
        std::fs::remove_file(&file).unwrap();

        // A symlinked secrets directory is refused too.
        let real = state.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::remove_dir(secrets_dir(state.path())).unwrap();
        std::os::unix::fs::symlink(&real, secrets_dir(state.path())).unwrap();
        std::fs::write(real.join("github_token"), CANARY).unwrap();
        std::fs::set_permissions(
            real.join("github_token"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(read_secret(state.path(), "github_token").is_err());
        assert!(write_secret(state.path(), "github_token", CANARY.as_bytes()).is_err());
    }
}
