//! Container creation identities deliberately differ from shared image identities.

use std::path::Path;

pub(crate) const MAX_OBSERVATION_BYTES: usize = 256 * 1024;

/// Parse one complete bounded JSON value, refusing duplicate keys even in
/// nested objects. Docker observations never have a last-key-wins authority.
pub(crate) fn bounded_json(bytes: &[u8]) -> Result<serde_json::Value, ()> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err(());
    }
    let value = serde_json::from_slice(bytes).map_err(|_| ())?;
    let mut objects = Vec::<std::collections::BTreeSet<String>>::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => objects.push(Default::default()),
            b'}' => {
                objects.pop().ok_or(())?;
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index += 2;
                        continue;
                    }
                    if bytes[index] == b'"' {
                        break;
                    }
                    index += 1;
                }
                let mut next = index + 1;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                if bytes.get(next) == Some(&b':') {
                    let key: String =
                        serde_json::from_slice(&bytes[start..=index]).map_err(|_| ())?;
                    if !objects.last_mut().ok_or(())?.insert(key) {
                        return Err(());
                    }
                }
            }
            _ => {}
        }
        index += 1;
    }
    if !objects.is_empty() {
        return Err(());
    }
    Ok(value)
}

/// Hash the exact, validated creation arguments with a separate canonical
/// workspace binding. Arguments contain no execution-time passthrough secrets.
/// Length framing avoids delimiter collisions; callers sort unordered mounts.
pub(crate) fn creation_digest(workspace: &Path, arguments: &[String]) -> String {
    let mut bytes = Vec::new();
    for field in std::iter::once("bosn-setup-creation-v2")
        .chain(std::iter::once(
            workspace.to_str().expect("validated UTF-8 workspace"),
        ))
        .chain(arguments.iter().map(String::as_str))
    {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    kernal_api::hash::sha256_bytes(&bytes).to_hex()
}
