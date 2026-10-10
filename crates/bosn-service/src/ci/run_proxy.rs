//! A run's Docker proxy (#547), served by the daemon in its engine's socket
//! directory. act and every job container reach the engine's `dockerd` only
//! through it, so whatever they create carries the run's label and lands in
//! the run's cgroup, and they cannot see or touch another run's objects.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use bosn_registry::act::{ActEngineDockerSocket, ENGINE_DOCKER_SOCKET};

use super::engine::RunScope;
use crate::{
    docker_api::LABEL_RUN,
    docker_proxy::{Activity, DockerProxy, ProxySettings, VolumePolicy},
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
            volumes: Arc::new(RunVolumes {
                upstream: dir.join(engine_socket),
                label: scope.label().into(),
                key: scope.key().into(),
            }),
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

/// act's shared tool cache, seeded once per engine and never a run's.
const TOOLCACHE: &str = "act-toolcache";

/// Every named volume a run's containers use carries the run's label, so
/// closing the scope removes it. act names its per-job volumes after the
/// workflow and job only (`act-<workflow>-<job>-<hash>`, and `…-env`), so
/// two runs of one workflow would share them: those get the run's key as a
/// suffix. A volume Docker would create implicitly is created first, with
/// the label.
struct RunVolumes {
    upstream: PathBuf,
    label: String,
    key: String,
}

impl RunVolumes {
    fn name(&self, name: &str) -> String {
        if name != TOOLCACHE && name.starts_with("act-") && !name.ends_with(&self.key) {
            format!("{name}-{}", self.key)
        } else {
            name.to_owned()
        }
    }
}

impl VolumePolicy for RunVolumes {
    fn map_volume(&self, name: &str) -> io::Result<String> {
        let mapped = self.name(name);
        if mapped != TOOLCACHE {
            create_volume(&self.upstream, &mapped, &self.label)?;
        }
        Ok(mapped)
    }
    fn injected_mounts(&self) -> io::Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
}

/// `POST /volumes/create` with the run's label. Docker returns an existing
/// volume of that name unchanged.
#[cfg(unix)]
fn create_volume(upstream: &Path, name: &str, label: &str) -> io::Result<()> {
    use std::io::{Read, Write};
    let body = serde_json::json!({ "Name": name, "Labels": { LABEL_RUN: label } }).to_string();
    let mut stream = std::os::unix::net::UnixStream::connect(upstream)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    write!(
        stream,
        "POST /volumes/create HTTP/1.1\r\nHost: docker\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut head = [0u8; 12];
    stream.read_exact(&mut head)?;
    match &head[9..10] {
        b"2" => Ok(()),
        _ => Err(io::Error::other(format!(
            "volume {name} not created: {}",
            String::from_utf8_lossy(&head)
        ))),
    }
}

#[cfg(not(unix))]
fn create_volume(_upstream: &Path, _name: &str, _label: &str) -> io::Result<()> {
    Err(io::Error::other("the run Docker proxy needs Unix sockets"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ci::engine::RunLimits;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn acts_job_volumes_get_the_run_key_and_the_tool_cache_stays_shared() {
        let volumes = RunVolumes {
            upstream: "/nonexistent".into(),
            label: "r".into(),
            key: "0a1b2c3d4e5f".into(),
        };
        assert_eq!(
            volumes.name("act-ci-build-abc"),
            "act-ci-build-abc-0a1b2c3d4e5f"
        );
        assert_eq!(
            volumes.name("act-ci-build-abc-env"),
            "act-ci-build-abc-env-0a1b2c3d4e5f"
        );
        assert_eq!(
            volumes.name("act-ci-build-abc-0a1b2c3d4e5f"),
            "act-ci-build-abc-0a1b2c3d4e5f"
        );
        assert_eq!(volumes.name("act-toolcache"), "act-toolcache");
        assert_eq!(volumes.name("my-volume"), "my-volume");
        assert_eq!(
            volumes.map_volume("act-toolcache").unwrap(),
            "act-toolcache"
        );
        assert!(
            volumes.map_volume("my-volume").is_err(),
            "the engine is unreachable"
        );
    }

    #[test]
    fn a_volume_is_created_with_the_run_label() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let socket = dir.path().join("engine.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0u8; 4096];
            let n = stream.read(&mut request).unwrap();
            stream.write_all(b"HTTP/1.1 201 Created\r\n\r\n{}").unwrap();
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        create_volume(&socket, "v-0a1b2c3d4e5f", "run-1").unwrap();
        let request = server.join().unwrap();
        assert!(
            request.starts_with("POST /volumes/create HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(r#""com.zackees.bosn.run":"run-1""#),
            "{request}"
        );
        assert!(request.contains(r#""Name":"v-0a1b2c3d4e5f""#), "{request}");
    }

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
