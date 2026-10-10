//! Where this daemon keeps its engines' Docker socket directories (#547).
//!
//! Each engine binds its own directory, named after its registry key, at
//! [`ENGINE_SOCKET_DIR`]. The engine's `dockerd` listens there, and so will
//! the per-run Docker proxies the daemon serves for runs inside the engine.
//! A Unix socket path must fit `sockaddr_un` (108 bytes), so the directory
//! lives under the state directory only when that is short; otherwise under
//! a private `/tmp/bosn-sock-<hash>` owned by the daemon's user.
//!
//! Only a Linux daemon talking to its own machine's Docker engine binds one:
//! elsewhere (Docker Desktop's VM) a host Unix socket cannot be shared with
//! a container.
//!
//! [`ENGINE_SOCKET_DIR`]: bosn_registry::act::ENGINE_SOCKET_DIR

use std::path::Path;

use bosn_registry::act::{ActEngineDockerSocket, SOCKET_DIR_MAX};

/// The directory name an engine's sockets get under the base: the first
/// twelve hex digits of its registry key (a UUID).
fn engine_key(run_id: &str) -> Option<String> {
    let key: String = run_id.chars().filter(|c| *c != '-').take(12).collect();
    (key.len() == 12 && key.bytes().all(|b| b.is_ascii_hexdigit())).then_some(key)
}

/// The base directory for this daemon's engines: `<state>/sock` when short
/// enough, else `/tmp/bosn-sock-<hash of the state directory>`.
fn base(state_dir: &Path) -> String {
    let local = state_dir.join("sock").to_string_lossy().into_owned();
    if local.len() + 13 <= SOCKET_DIR_MAX && valid(&local) {
        return local;
    }
    let hash = kernal_api::hash::sha256_bytes(state_dir.as_os_str().as_encoded_bytes()).to_hex();
    format!("/tmp/bosn-sock-{}", &hash[..16])
}

fn valid(dir: &str) -> bool {
    socket(dir.into(), 0).validate().is_ok()
}

fn socket(host_dir: String, group: u32) -> ActEngineDockerSocket {
    ActEngineDockerSocket { host_dir, group }
}

/// The socket directory for the engine keyed `run_id`, or `None` when this
/// daemon cannot share one with its engines.
#[cfg(target_os = "linux")]
pub fn engine_socket(state_dir: &Path, run_id: &str) -> Option<ActEngineDockerSocket> {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(state_dir).ok()?;
    let base = base(state_dir);
    // A base someone else made (in /tmp) could be swapped under the engine.
    if let Ok(existing) = std::fs::symlink_metadata(&base)
        && (!existing.is_dir() || existing.uid() != owner.uid())
    {
        return None;
    }
    let dir = format!("{base}/{}", engine_key(run_id)?);
    valid(&dir).then(|| socket(dir, owner.gid()))
}

#[cfg(not(target_os = "linux"))]
pub fn engine_socket(_state_dir: &Path, _run_id: &str) -> Option<ActEngineDockerSocket> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_keys_are_twelve_hex_digits_of_the_uuid() {
        assert_eq!(
            engine_key("0a1b2c3d-4e5f-4a6b-8c7d-000000000001").as_deref(),
            Some("0a1b2c3d4e5f")
        );
        assert_eq!(engine_key("short"), None);
        assert_eq!(engine_key("zzzzzzzz-zzzz-4zzz-8zzz-zzzzzzzzzzzz"), None);
    }

    #[test]
    fn a_long_state_directory_moves_the_sockets_to_a_short_private_path() {
        assert_eq!(base(Path::new("/home/me/.bosn")), "/home/me/.bosn/sock");
        let long = Path::new("/home/me").join("x".repeat(80));
        let moved = base(&long);
        assert!(moved.starts_with("/tmp/bosn-sock-"), "{moved}");
        assert_eq!(moved.len(), "/tmp/bosn-sock-".len() + 16);
        assert_ne!(moved, base(&Path::new("/home/me").join("y".repeat(80))));
        let odd = base(Path::new("/home/me/my state"));
        assert!(odd.starts_with("/tmp/bosn-sock-"), "{odd}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn each_engine_gets_its_own_directory_with_the_owners_group() {
        use std::os::unix::fs::MetadataExt;
        let state = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let run = "0a1b2c3d-4e5f-4a6b-8c7d-000000000001";
        let socket = engine_socket(state.path(), run).unwrap();
        socket.validate().unwrap();
        assert!(socket.host_dir.ends_with("/0a1b2c3d4e5f"), "{socket:?}");
        let group = std::fs::metadata(state.path()).unwrap().gid();
        assert_eq!(socket.group, group);
        let other = engine_socket(state.path(), "ffffffff-4e5f-4a6b-8c7d-000000000001").unwrap();
        assert_ne!(socket.host_dir, other.host_dir);
    }
}
