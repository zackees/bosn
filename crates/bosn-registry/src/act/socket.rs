//! The engine's Docker socket directory (#547): a host directory bound into
//! the engine, where the engine's own `dockerd` listens beside the host-side
//! per-run Docker proxies, so a run inside the engine reaches Docker only
//! through its proxy.
use super::*;

/// Where the socket directory is mounted inside the engine. Short on
/// purpose: a Unix socket path must fit `sockaddr_un` (108 bytes).
pub const ENGINE_SOCKET_DIR: &str = "/bosn/sock";
/// The engine's own `dockerd` listener in that directory.
pub const ENGINE_DOCKER_SOCKET: &str = "/bosn/sock/engine.sock";
/// The longest host directory accepted, leaving room for a run's socket
/// name inside the 108-byte `sockaddr_un` path.
pub const SOCKET_DIR_MAX: usize = 80;

/// A frozen bind of the daemon's socket directory into the engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActEngineDockerSocket {
    /// The host directory, absolute and normalized.
    pub host_dir: String,
    /// The numeric group the engine's `dockerd` gives its socket, so the
    /// daemon's user can reach it (mode 0660).
    pub group: u32,
}

impl ActEngineDockerSocket {
    pub fn validate(&self) -> Result<(), Error> {
        let dir = &self.host_dir;
        let clean = dir.len() <= SOCKET_DIR_MAX
            && dir.len() > 1
            && dir.starts_with('/')
            && !dir.ends_with('/')
            && dir
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
            && dir[1..]
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        if !clean {
            return Err(Error::BadRow("act engine docker socket directory"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket(dir: &str) -> ActEngineDockerSocket {
        ActEngineDockerSocket {
            host_dir: dir.into(),
            group: 100,
        }
    }

    #[test]
    fn only_short_normalized_absolute_directories_are_accepted() {
        socket("/home/me/.local/state/bosn/s").validate().unwrap();
        socket("/tmp/bosn-1000-0a1b2c3d").validate().unwrap();
        for bad in [
            "",
            "/",
            "relative/s",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/b/",
            "/a b",
            "/a,b",
            "/a=b",
        ] {
            assert!(socket(bad).validate().is_err(), "{bad:?}");
        }
        let long = format!("/{}", "a".repeat(SOCKET_DIR_MAX));
        assert!(socket(&long).validate().is_err());
        let fits = format!("/{}", "a".repeat(SOCKET_DIR_MAX - 1));
        socket(&fits).validate().unwrap();
        assert!(fits.len() + "/0123456789ab.sock".len() < 108);
    }
}
