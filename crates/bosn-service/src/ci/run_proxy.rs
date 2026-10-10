//! A run's Docker proxy (#547), served by the daemon in its engine's socket
//! directory. act and every job container reach the engine's `dockerd` only
//! through it, so whatever they create carries the run's label and lands in
//! the run's cgroup, and they cannot see or touch another run's objects.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use bosn_registry::act::{ActEngineDockerSocket, ENGINE_DOCKER_SOCKET};

use super::engine::RunScope;
use crate::{
    docker_api::LABEL_RUN,
    docker_proxy::{Activity, DockerProxy, NoVolumes, ProxySettings},
};

/// The proxy; stopped (and its socket removed) on drop.
pub struct RunProxy {
    _proxy: DockerProxy,
}

impl RunProxy {
    pub fn start(socket: &ActEngineDockerSocket, scope: &RunScope) -> Result<Self, String> {
        let dir = Path::new(&socket.host_dir);
        let engine_socket = ENGINE_DOCKER_SOCKET
            .rsplit('/')
            .next()
            .unwrap_or(ENGINE_DOCKER_SOCKET);
        let settings = ProxySettings {
            upstream: dir.join(engine_socket),
            run: scope.label().into(),
            name_suffix: scope.key().into(),
            labels: BTreeMap::from([(LABEL_RUN.to_owned(), scope.label().to_owned())]),
            nano_cpus: 0,
            memory: None,
            cgroup_parent: Some(scope.cgroup()),
            volumes: Arc::new(NoVolumes),
            activity: Arc::new(Activity::new()),
            notes: None,
        };
        let path = dir.join(scope.socket_name());
        // The engine's creation made it; a recovered or test engine may not have.
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("run Docker proxy {}: {error}", dir.display()))?;
        let proxy = DockerProxy::start(&path, settings)
            .map_err(|error| format!("run Docker proxy {}: {error}", path.display()))?;
        // Job containers run as an unprivileged user; the directory itself
        // stays private to the daemon's user and the engine.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                .map_err(|error| format!("run Docker proxy {}: {error}", path.display()))?;
        }
        Ok(Self { _proxy: proxy })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ci::engine::RunLimits;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_proxy_listens_in_the_engine_directory_for_any_job_user() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let socket = ActEngineDockerSocket {
            host_dir: dir.path().to_string_lossy().into_owned(),
            group: 0,
        };
        let limits = RunLimits::within_engine(4 << 30, 1_000_000_000, 1024);
        let scope = RunScope::new("0a1b2c3d-4e5f-4a6b-8c7d-000000000001", 0, limits).unwrap();
        let path = dir.path().join("0a1b2c3d4e5f.sock");
        {
            let _proxy = RunProxy::start(&socket, &scope).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o666);
        }
        assert!(!path.exists(), "the socket goes with the proxy");
    }
}
