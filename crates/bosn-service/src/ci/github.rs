//! `bosn ci run --github-proxy` (#650): one run's GitHub credential proxy.
//!
//! The job gets a fake `GITHUB_TOKEN` (also `secrets.GITHUB_TOKEN`) and a
//! first step ([`crate::github_bootstrap`]) that routes `api.github.com`
//! through the run's relay in the engine ([`crate::github_relay`]) and trusts
//! the run's CA. The daemon terminates that TLS ([`crate::github_connect`]),
//! and the run's [`GithubApiProxy`] accepts only the fake token, applies the
//! route and method allowlist ([`crate::github_ci_policy`]) and attaches the
//! host's real credential, which never leaves daemon memory.

use std::sync::Arc;

use kernal_api::async_engine;

use super::engine::{ActEngineBackend, RunScope};
use crate::github_ci_policy::GithubWrites;
use crate::github_proxy::ResponseCache;

/// Why a run cannot use the proxy, or what failed setting it up.
pub type GithubResult<T> = Result<T, String>;

/// The run's `owner/name`, or why `--github-proxy` cannot scope to it.
pub fn repository_slug(repository: &str) -> GithubResult<&str> {
    let ok = repository.split_once('/').is_some_and(|(o, n)| {
        let part = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        part(o) && part(n) && !n.contains('/')
    });
    if ok {
        Ok(repository)
    } else {
        Err("--github-proxy needs a checkout whose origin is a GitHub repository (owner/name): writes are scoped to it".into())
    }
}

#[cfg(target_os = "linux")]
pub use imp::RunGithub;
#[cfg(not(target_os = "linux"))]
pub use stub::RunGithub;

#[cfg(target_os = "linux")]
mod imp {
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::github_ci_policy::{CiPolicy, FakeToken};
    use crate::github_connect::{Front, INTERCEPTED_HOST, Inner, InterceptCa};
    use crate::github_proxy::{
        CredentialSource, GITHUB_API_UPSTREAM, GithubApiProxy, resolve_credential,
    };
    use crate::github_relay::{self, RelayPool};

    /// One run's proxy: started before the run is planned, attached to the
    /// engine once the run has a scope, detached before the scope closes.
    pub struct RunGithub {
        fake: FakeToken,
        /// The real token, for the output masker only.
        real: Option<String>,
        source: CredentialSource,
        writes: GithubWrites,
        _api: GithubApiProxy,
        front: Arc<Front>,
        logs: async_engine::Sender<String>,
        log_rx: Mutex<Option<async_engine::Receiver<String>>>,
        hello: String,
        relay: Mutex<Option<(RelayPool, String)>>,
    }

    impl std::fmt::Debug for RunGithub {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RunGithub")
                .field("source", &self.source)
                .field("writes", &self.writes)
                .finish_non_exhaustive()
        }
    }

    impl RunGithub {
        /// Resolve the host credential, mint the fake token and the run's
        /// CA, and start the run's API proxy on loopback.
        pub async fn start(
            state_dir: &std::path::Path,
            repository: &str,
            writes: GithubWrites,
            cache: Arc<ResponseCache>,
        ) -> GithubResult<Self> {
            Self::start_with(
                state_dir,
                repository,
                writes,
                cache,
                GITHUB_API_UPSTREAM,
                "gh",
            )
            .await
        }

        pub(crate) async fn start_with(
            state_dir: &std::path::Path,
            repository: &str,
            writes: GithubWrites,
            cache: Arc<ResponseCache>,
            upstream: &str,
            gh: &str,
        ) -> GithubResult<Self> {
            let repository = repository_slug(repository)?.to_owned();
            let credential = resolve_credential(state_dir, &gh.into()).await?;
            let fake = FakeToken::generate().await?;
            let hello = FakeToken::generate().await?.secret_value()
                [crate::github_ci_policy::FAKE_TOKEN_PREFIX.len()..]
                .to_owned();
            let (logs, log_rx) = async_engine::channel::<String>(4096);
            let policy = CiPolicy {
                fake: fake.clone(),
                repository,
                writes,
            };
            let real = credential.secret_value().map(str::to_owned);
            let source = credential.source();
            let api = GithubApiProxy::start_with_policy(
                upstream,
                credential,
                cache,
                Some(logs.clone()),
                Some(policy),
            )
            .await
            .map_err(|e| format!("github proxy: {e}"))?;
            let ca =
                InterceptCa::mint(INTERCEPTED_HOST).map_err(|e| format!("github proxy CA: {e}"))?;
            let (addr, prefix) = api.listener();
            Ok(Self {
                fake,
                real,
                source,
                writes,
                _api: api,
                front: Arc::new(Front {
                    ca: Arc::new(ca),
                    inner: Inner { addr, prefix },
                }),
                logs,
                log_rx: Mutex::new(Some(log_rx)),
                hello,
                relay: Mutex::new(None),
            })
        }

        /// The job's `GITHUB_TOKEN`: the fake token, passed to act as a
        /// secret (so it is masked and never in argv).
        pub fn secret(&self) -> (String, String) {
            ("GITHUB_TOKEN".into(), self.fake.secret_value().to_owned())
        }

        /// Every value the run log must mask: the fake and the real token.
        pub fn masked(&self) -> Vec<String> {
            let mut values = vec![self.fake.secret_value().to_owned()];
            values.extend(self.real.clone());
            values
        }

        /// The step every job runs first.
        pub fn bootstrap_step(&self) -> serde_yaml::Value {
            crate::github_bootstrap::step(self.front.ca.ca_pem())
        }

        /// The run log's line about the proxy; names the credential source,
        /// never a value.
        pub fn summary(&self) -> String {
            format!(
                "GitHub proxy: jobs get a per-run stand-in GITHUB_TOKEN; api.github.com requests carrying it are sent with the host credential from {}; writes {}",
                self.source.label(),
                self.writes.as_str()
            )
        }

        /// The proxy's per-request log lines, taken once by the run's observer.
        pub fn take_logs(&self) -> Option<async_engine::Receiver<String>> {
            self.log_rx.lock().ok()?.take()
        }

        /// Start the run's relay in the engine and the daemon's channels to
        /// it. Returns the job port for [`crate::github_bootstrap::PORT_ENV`].
        pub async fn attach(
            &self,
            backend: &dyn ActEngineBackend,
            engine: &str,
            scope: &RunScope,
        ) -> GithubResult<u16> {
            let (channel_port, job_port) = github_relay::ports(scope.slot());
            let dir = github_relay::dir(super::super::engine::ENGINE_WORK, scope.key());
            let script = github_relay::start_script(&dir, channel_port, job_port);
            let stdout = backend
                .engine_script(engine, &script, format!("{}\n", self.hello).as_bytes())
                .await?;
            let ip: IpAddr = stdout
                .lines()
                .last()
                .unwrap_or("")
                .trim()
                .parse()
                .map_err(|_| format!("github relay: engine address unreadable: {stdout:?}"))?;
            let front = self.front.clone();
            let logs = self.logs.clone();
            let serve = Arc::new(move |stream: std::net::TcpStream| {
                let log = |line: String| {
                    let _ = logs.try_send(line);
                };
                crate::github_connect::serve_client(stream, &front.ca, &front.inner, &log);
            });
            let pool =
                RelayPool::start(SocketAddr::new(ip, channel_port), self.hello.clone(), serve);
            if let Ok(mut relay) = self.relay.lock() {
                *relay = Some((pool, dir));
            }
            // Give the pool a moment to park its first channels.
            async_engine::sleep(Duration::from_millis(300)).await;
            Ok(job_port)
        }

        /// Stop the relay in the engine and close the daemon's channels.
        pub async fn detach(
            &self,
            backend: &dyn ActEngineBackend,
            engine: &str,
        ) -> GithubResult<()> {
            let Some((pool, dir)) = self.relay.lock().ok().and_then(|mut r| r.take()) else {
                return Ok(());
            };
            let stopped = backend
                .engine_script(engine, &github_relay::stop_script(&dir), &[])
                .await;
            let _ = async_engine::launch_blocking(move || drop(pool)).await;
            stopped.map(|_| ())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod stub {
    use super::*;

    /// `--github-proxy` needs the Linux CONNECT front; elsewhere it refuses.
    #[derive(Debug)]
    pub enum RunGithub {}

    impl RunGithub {
        pub async fn start(
            _state_dir: &std::path::Path,
            _repository: &str,
            _writes: GithubWrites,
            _cache: Arc<ResponseCache>,
        ) -> GithubResult<Self> {
            Err("--github-proxy needs a Linux host".into())
        }
        pub fn secret(&self) -> (String, String) {
            match *self {}
        }
        pub fn masked(&self) -> Vec<String> {
            match *self {}
        }
        pub fn bootstrap_step(&self) -> serde_yaml::Value {
            match *self {}
        }
        pub fn summary(&self) -> String {
            match *self {}
        }
        pub fn take_logs(&self) -> Option<async_engine::Receiver<String>> {
            match *self {}
        }
        pub async fn attach(
            &self,
            _: &dyn ActEngineBackend,
            _: &str,
            _: &RunScope,
        ) -> GithubResult<u16> {
            match *self {}
        }
        pub async fn detach(&self, _: &dyn ActEngineBackend, _: &str) -> GithubResult<()> {
            match *self {}
        }
    }
}
