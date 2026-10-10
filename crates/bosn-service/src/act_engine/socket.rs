//! The engine's bound Docker socket directory (#547). The engine's `dockerd`
//! listens there beside its usual socket, so the daemon's per-run Docker
//! proxies (on the host, in the same directory) can reach it, and a run
//! inside the engine reaches Docker only through its own proxy.

use super::*;
use bosn_registry::act::{ActEngineDockerSocket, ENGINE_DOCKER_SOCKET, ENGINE_SOCKET_DIR};
use serde::Deserialize;

/// The extra `dockerd` listener: the bound directory, owned by the daemon
/// user's group (mode 0660).
pub(super) fn listener_args(socket: Option<&ActEngineDockerSocket>) -> Vec<String> {
    socket
        .map(|socket| {
            vec![
                format!("--host=unix://{ENGINE_DOCKER_SOCKET}"),
                format!("--group={}", socket.group),
            ]
        })
        .unwrap_or_default()
}

/// `--mount` for the socket directory.
pub(super) fn bind_argument(socket: &ActEngineDockerSocket) -> String {
    format!(
        "type=bind,source={},target={ENGINE_SOCKET_DIR}",
        socket.host_dir
    )
}

/// The socket directory an intent's engine binds, if any.
pub(super) fn of(intent: &ActEngineIntent) -> Option<&ActEngineDockerSocket> {
    intent
        .creation_profile
        .as_ref()
        .and_then(|profile| profile.docker_socket.as_ref())
}

/// [`with_docker_socket`] when there is a socket directory.
pub(super) fn frozen_into(
    profile: ActEngineCreationProfile,
    socket: Option<ActEngineDockerSocket>,
) -> Result<ActEngineCreationProfile, ActEngineError> {
    match socket {
        Some(socket) => with_docker_socket(profile, socket),
        None => Ok(profile),
    }
}

/// Freeze `socket` into `profile`: the bind, and the command that listens on it.
pub(crate) fn with_docker_socket(
    mut profile: ActEngineCreationProfile,
    socket: ActEngineDockerSocket,
) -> Result<ActEngineCreationProfile, ActEngineError> {
    socket
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    profile.init_command_sha256 = command_digest(&engine_command_with_tools(
        profile.cache_volume.as_ref(),
        profile.tool_generation.as_ref(),
        Some(&socket),
    )?)?;
    profile.docker_socket = Some(socket);
    profile
        .validate()
        .map_err(|error| ActEngineError(error.to_string()))?;
    Ok(profile)
}

/// Make the engine's socket directory before the engine is created: Docker
/// refuses a bind whose source is missing. Private to the daemon's user
/// (0700); the engine's root reaches it anyway.
pub(super) fn ensure_dir(socket: &ActEngineDockerSocket) -> Result<(), ActEngineError> {
    let dir = std::path::Path::new(&socket.host_dir);
    let made = std::fs::create_dir_all(dir).and_then(|()| {
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    });
    made.map_err(|error| ActEngineError(format!("socket directory {}: {error}", socket.host_dir)))
}

/// Remove the socket directory of an engine proven gone: its `dockerd`
/// socket (owned by the engine's root, but in the daemon's directory) and
/// any proxy socket left behind. Best-effort; a leftover directory holds
/// only dead sockets.
pub(super) fn remove_dir(socket: &ActEngineDockerSocket) {
    let dir = std::path::Path::new(&socket.host_dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// One bind mount as `docker inspect` reports it, in `Mounts` (`Destination`,
/// `RW`) or in `HostConfig.Mounts` (`Target`, `ReadOnly`).
#[derive(Deserialize)]
struct Bind {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Source", default)]
    source: String,
    #[serde(rename = "Destination", alias = "Target", default)]
    target: String,
    #[serde(rename = "RW", default)]
    rw: Option<bool>,
    #[serde(rename = "ReadOnly", default)]
    read_only: Option<bool>,
}

impl Bind {
    fn is(&self, socket: &ActEngineDockerSocket) -> bool {
        self.kind == "bind"
            && self.source == socket.host_dir
            && self.target == ENGINE_SOCKET_DIR
            && self.rw != Some(false)
            && self.read_only != Some(true)
    }
}

/// `mounts` without the expected socket bind; `None` when a bind is there
/// that is not exactly the frozen one, or the frozen one is missing.
pub(super) fn without_socket_bind(
    mounts: &[Value],
    socket: Option<&ActEngineDockerSocket>,
) -> Option<Vec<Value>> {
    let mut rest = Vec::with_capacity(mounts.len());
    let mut found = 0;
    for mount in mounts {
        if mount["Type"] != "bind" {
            rest.push(mount.clone());
            continue;
        }
        let bind: Bind = serde_json::from_value(mount.clone()).ok()?;
        if !socket.is_some_and(|socket| bind.is(socket)) {
            return None;
        }
        found += 1;
    }
    (found == usize::from(socket.is_some())).then_some(rest)
}

/// [`without_socket_bind`] over `HostConfig.Mounts`, which Docker reports as
/// `null` when nothing is mounted.
pub(super) fn host_without_socket_bind(
    host_mounts: &Value,
    socket: Option<&ActEngineDockerSocket>,
) -> Option<Value> {
    match host_mounts.as_array() {
        Some(mounts) => without_socket_bind(mounts, socket).map(|rest| {
            if rest.is_empty() {
                Value::Null
            } else {
                Value::Array(rest)
            }
        }),
        None if socket.is_none() => Some(host_mounts.clone()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn socket() -> ActEngineDockerSocket {
        ActEngineDockerSocket {
            host_dir: "/run/user/1000/bosn-ci".into(),
            group: 100,
        }
    }

    #[test]
    fn the_listener_and_bind_name_the_frozen_directory() {
        assert!(listener_args(None).is_empty());
        assert_eq!(
            listener_args(Some(&socket())),
            ["--host=unix:///bosn/sock/engine.sock", "--group=100"]
        );
        assert_eq!(
            bind_argument(&socket()),
            "type=bind,source=/run/user/1000/bosn-ci,target=/bosn/sock"
        );
    }

    #[test]
    fn only_the_exact_frozen_bind_is_accepted() {
        let volume = json!({"Type":"volume","Name":"x","Destination":"/var/lib/docker"});
        let bind = json!({"Type":"bind","Source":"/run/user/1000/bosn-ci","Destination":"/bosn/sock","RW":true,"Propagation":"rprivate"});
        let mounts = [volume.clone(), bind.clone()];
        assert_eq!(
            without_socket_bind(&mounts, Some(&socket())),
            Some(vec![volume.clone()])
        );
        // A bind nobody froze, or a missing frozen bind, is refused.
        assert_eq!(without_socket_bind(&mounts, None), None);
        assert_eq!(
            without_socket_bind(std::slice::from_ref(&volume), Some(&socket())),
            None
        );
        let other = json!({"Type":"bind","Source":"/","Destination":"/bosn/sock","RW":true});
        assert_eq!(without_socket_bind(&[other], Some(&socket())), None);
        let read_only = json!({"Type":"bind","Source":"/run/user/1000/bosn-ci","Destination":"/bosn/sock","RW":false});
        assert_eq!(without_socket_bind(&[read_only], Some(&socket())), None);
        let twice = [bind.clone(), bind];
        assert_eq!(without_socket_bind(&twice, Some(&socket())), None);
    }

    #[test]
    fn host_mounts_drop_the_bind_and_keep_docker_null() {
        let bind = json!([{"Type":"bind","Source":"/run/user/1000/bosn-ci","Target":"/bosn/sock"}]);
        assert_eq!(
            host_without_socket_bind(&bind, Some(&socket())),
            Some(Value::Null)
        );
        assert_eq!(
            host_without_socket_bind(&Value::Null, None),
            Some(Value::Null)
        );
        assert_eq!(
            host_without_socket_bind(&Value::Null, Some(&socket())),
            None
        );
        let volume = json!([{"Type":"volume","Source":"x","Target":"/bosn/cache"}]);
        assert_eq!(
            host_without_socket_bind(&volume, None),
            Some(volume.clone())
        );
    }
}
