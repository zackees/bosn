//! Docker and narrowly typed SSH client transports for Bosn's engine domain.
//!
//! This is a local Docker CLI transport, not a daemon IPC service or an
//! authorization boundary. Callers provide trusted Docker argv/environment;
//! all child lifecycle and I/O is delegated to the public `kernal-api` facade.

use std::{
    ffi::OsString,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use kernal_api::{
    BoundedProcessError, ProcessOutputChunk, ProcessOutputCompletion, ProcessOutputEvent,
    ProcessPostExitDrain, ProcessSessionOptions, SpawnSpec, StreamMode,
    async_engine::{self, CancellationToken},
};

const SESSION_POLL: Duration = Duration::from_millis(20);

/// One SSH invocation whose network endpoint and authentication shape are
/// deliberately finite.  It exists for the macOS guest transport; it is not
/// a general remote-command API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestSshCommand {
    /// The account selected by the manifest-derived guest receipt.
    pub user: String,
    /// The published loopback SSH port selected by that receipt.
    pub port: u16,
    /// An existing private key beneath daemon-owned state.  No ambient agent,
    /// password prompt, user SSH config, or caller-selected credential is used.
    pub identity_file: PathBuf,
    /// The already-declared manifest task command.
    pub command: String,
}

/// One SCP upload to the fixed loopback macOS guest. Like
/// [`GuestSshCommand`], this deliberately has no raw argv or endpoint surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestScpCommand {
    /// The manifest-derived guest account.
    pub user: String,
    /// The manifest-derived published loopback SSH port.
    pub port: u16,
    /// The daemon-owned guest private key.
    pub identity_file: PathBuf,
    /// One already-validated regular file beneath the canonical workspace.
    pub source: PathBuf,
    /// One already-normalized guest path.
    pub destination: String,
}

impl GuestSshCommand {
    fn args(&self) -> Vec<OsString> {
        // `-F /dev/null` is intentional: host-wide and per-user SSH config
        // could otherwise redirect this bounded loopback operation through a
        // ProxyCommand, alternate identity, or arbitrary host alias.
        [
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-i",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(std::iter::once(self.identity_file.clone().into_os_string()))
        .chain(
            ["-p"].into_iter().map(OsString::from).chain(
                [
                    self.port.to_string(),
                    format!("{}@127.0.0.1", self.user),
                    self.command.clone(),
                ]
                .into_iter()
                .map(OsString::from),
            ),
        )
        .collect()
    }
}

impl GuestScpCommand {
    fn args(&self) -> Vec<OsString> {
        // Keep this synchronized with GuestSshCommand: SCP must not consult
        // ambient SSH configuration, credentials, agents, or known-host state.
        [
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-i",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(std::iter::once(self.identity_file.clone().into_os_string()))
        .chain(["-P"].into_iter().map(OsString::from))
        .chain(
            [
                self.port.to_string(),
                self.source.to_string_lossy().into_owned(),
                format!("{}@127.0.0.1:{}", self.user, self.destination),
            ]
            .into_iter()
            .map(OsString::from),
        )
        .collect()
    }
}

/// Outcome of one read-only census read.
///
/// A refusal is modelled as data rather than an error: the caller must be able to tell
/// "the engine said nothing" from "the engine said zero", because a census that cannot be
/// read is a reason to keep everything, never a reason to reclaim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CensusRead {
    /// The engine returned a complete document.
    Document(String),
    /// The engine refused, or answered in a form that cannot be read.
    Unavailable { detail: String },
}

impl CensusRead {
    #[must_use]
    pub fn document(&self) -> Option<&str> {
        match self {
            Self::Document(text) => Some(text),
            Self::Unavailable { .. } => None,
        }
    }
}

/// The locally installed OpenSSH client, restricted to [`GuestSshCommand`].
/// It has no raw argv, host, port, or credential configuration surface.
#[derive(Clone, Debug)]
pub struct GuestSshEngine {
    binary: PathBuf,
}

impl GuestSshEngine {
    #[must_use]
    pub fn system() -> Self {
        Self {
            binary: "ssh".into(),
        }
    }

    /// Stream a declared guest task. Killing the local client cannot prove
    /// that its remote command stopped; callers retain that uncertainty.
    pub async fn stream(
        &self,
        command: &GuestSshCommand,
        options: RunOptions,
        cancellation: Option<&CancellationToken>,
        events: &async_engine::Sender<EngineEvent>,
    ) -> Result<CommandResult, CommandError> {
        // The shared bounded process machinery is intentionally reused here;
        // its caller cannot reach `DockerEngine::with_args` because this
        // conversion stays inside the typed SSH adapter.
        let transport = DockerEngine::from_parts(&self.binary, command.args());
        transport.stream(options, cancellation, events).await
    }
}

/// The locally installed OpenSSH SCP client, restricted to
/// [`GuestScpCommand`].
#[derive(Clone, Debug)]
pub struct GuestScpEngine {
    binary: PathBuf,
}

impl GuestScpEngine {
    #[must_use]
    pub fn system() -> Self {
        Self {
            binary: "scp".into(),
        }
    }

    /// Stream one bounded manifest payload upload. A failed upload prevents
    /// the subsequent task from starting, so callers need not retain task
    /// completion uncertainty for this operation.
    pub async fn stream(
        &self,
        command: &GuestScpCommand,
        options: RunOptions,
        cancellation: Option<&CancellationToken>,
        events: &async_engine::Sender<EngineEvent>,
    ) -> Result<CommandResult, CommandError> {
        let transport = DockerEngine::from_parts(&self.binary, command.args());
        transport.stream(options, cancellation, events).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandResult {
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

#[derive(Debug)]
pub enum CommandError {
    Spawn(io::Error),
    Deadline {
        reaped_pid: Option<u32>,
        cleanup: Option<String>,
    },
    Cancelled {
        reaped_pid: Option<u32>,
        cleanup: Option<String>,
    },
    OutputLimit {
        limit: usize,
        reaped_pid: Option<u32>,
        cleanup: Option<String>,
    },
    OutputCompletion {
        detail: String,
        reaped_pid: Option<u32>,
        cleanup: Option<String>,
    },
    OutputConsumerSlow {
        reaped_pid: Option<u32>,
        cleanup: Option<String>,
    },
    OutputConsumerClosed {
        reaped_pid: Option<u32>,
        cleanup: Option<String>,
    },
    Io(io::Error),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "Docker CLI could not start: {error}"),
            Self::Deadline {
                reaped_pid,
                cleanup,
            } => write!(
                f,
                "Docker CLI exceeded its deadline{}",
                outcome_suffix(*reaped_pid, cleanup)
            ),
            Self::Cancelled {
                reaped_pid,
                cleanup,
            } => write!(
                f,
                "Docker CLI was cancelled{}",
                outcome_suffix(*reaped_pid, cleanup)
            ),
            Self::OutputLimit {
                limit,
                reaped_pid,
                cleanup,
            } => write!(
                f,
                "Docker CLI output exceeded {limit} bytes{}",
                outcome_suffix(*reaped_pid, cleanup)
            ),
            Self::OutputCompletion {
                detail,
                reaped_pid,
                cleanup,
            } => write!(
                f,
                "Docker CLI output did not complete cleanly: {detail}{}",
                outcome_suffix(*reaped_pid, cleanup)
            ),
            Self::OutputConsumerSlow {
                reaped_pid,
                cleanup,
            } => write!(
                f,
                "Docker CLI output consumer was too slow{}",
                outcome_suffix(*reaped_pid, cleanup)
            ),
            Self::OutputConsumerClosed {
                reaped_pid,
                cleanup,
            } => write!(
                f,
                "Docker CLI output consumer closed{}",
                outcome_suffix(*reaped_pid, cleanup)
            ),
            Self::Io(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for CommandError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub deadline: Duration,
    pub output_limit: usize,
}
impl RunOptions {
    #[must_use]
    pub const fn bounded(deadline: Duration, output_limit: usize) -> Self {
        Self {
            deadline,
            output_limit,
        }
    }
    #[must_use]
    pub const fn streaming(deadline: Duration, output_limit: usize) -> Self {
        Self {
            deadline,
            output_limit,
        }
    }
}

/// A Docker CLI endpoint. Product code starts from [`Self::docker`]; this
/// crate has no daemon IPC surface and is not a sandbox for trusted callers.
#[derive(Debug, Clone)]
pub struct DockerEngine {
    binary: PathBuf,
    prefix: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}
impl DockerEngine {
    #[must_use]
    pub fn docker() -> Self {
        Self::from_parts("docker", std::iter::empty::<OsString>())
    }
    #[cfg(feature = "native-test-helper")]
    #[doc(hidden)]
    #[must_use]
    pub fn synthetic_for_test(
        binary: impl Into<PathBuf>,
        prefix: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self::from_parts(binary, prefix)
    }
    fn from_parts(
        binary: impl Into<PathBuf>,
        prefix: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            binary: binary.into(),
            prefix: prefix.into_iter().map(Into::into).collect(),
            current_dir: None,
            env: Vec::new(),
        }
    }
    fn spec(&self) -> SpawnSpec {
        let spec = self.prefix.iter().fold(
            SpawnSpec::new(self.binary.as_os_str())
                .stdin(StreamMode::Null)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped)
                .create_process_group(true)
                .kill_when_owner_dies(true),
            |spec, arg| spec.arg(arg),
        );
        let spec = if let Some(path) = &self.current_dir {
            spec.current_dir(path)
        } else {
            spec
        };
        self.env
            .iter()
            .fold(spec, |spec, (key, value)| spec.env(key, value))
    }
    /// Bounded, separated capture for read-only Docker diagnostics/control calls.
    pub fn capture(&self, options: RunOptions) -> Result<CommandResult, CommandError> {
        let result =
            kernal_api::run_bounded_command(self.spec(), options.deadline, options.output_limit);
        result
            .map(|output| CommandResult {
                exit_code: output.exit.raw_code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
            .map_err(map_bounded)
    }
    /// Async form of [`Self::capture`] for daemon actors; it uses the kernel
    /// blocking lane rather than constructing a runtime or exposing Tokio.
    pub async fn capture_async(&self, options: RunOptions) -> Result<CommandResult, CommandError> {
        kernal_api::run_bounded_command_async(self.spec(), options.deadline, options.output_limit)
            .await
            .map_err(|error| match error {
                kernal_api::BoundedProcessAsyncError::Bounded(error) => map_bounded(error),
                kernal_api::BoundedProcessAsyncError::Task(error) => {
                    CommandError::Io(io::Error::other(error.to_string()))
                }
            })
            .map(|output| CommandResult {
                exit_code: output.exit.raw_code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
    }
    /// Stream tagged stdout/stderr until the direct Docker client exits.
    ///
    /// Cancellation/deadline kill and reap only that client. In particular,
    /// killing `docker exec` does not establish that a remote container command
    /// stopped; later job integration must use ownership-validated container
    /// cancellation before reporting remote cancellation.
    /// Events use a bounded kernal-api channel. A full queue returns
    /// `OutputConsumerSlow` and reaps the client rather than pinning the
    /// runtime; a dropped receiver returns `OutputConsumerClosed`.
    pub async fn stream(
        &self,
        options: RunOptions,
        cancellation: Option<&CancellationToken>,
        events: &async_engine::Sender<EngineEvent>,
    ) -> Result<CommandResult, CommandError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(CommandError::Cancelled {
                reaped_pid: None,
                cleanup: None,
            });
        }
        let started = Instant::now();
        let spawn = self.spec().spawn_session(ProcessSessionOptions {
            max_queued_chunks: 64,
            max_chunk_bytes: 8 * 1024,
            post_exit_drain: ProcessPostExitDrain::AbandonAfter(Duration::from_millis(250)),
            kill_on_drop: true,
        });
        let session = match async_engine::timeout(options.deadline, spawn).await {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => return Err(CommandError::Spawn(error)),
            Err(_) => {
                return Err(CommandError::Deadline {
                    reaped_pid: None,
                    cleanup: None,
                });
            }
        };
        let pid = session.id();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return reap(
                    session,
                    CommandError::Cancelled {
                        reaped_pid: Some(pid),
                        cleanup: None,
                    },
                )
                .await;
            }
            if started.elapsed() >= options.deadline {
                return reap(
                    session,
                    CommandError::Deadline {
                        reaped_pid: Some(pid),
                        cleanup: None,
                    },
                )
                .await;
            }
            match async_engine::timeout(SESSION_POLL, session.next_output()).await {
                Ok(Some(ProcessOutputEvent::Chunk(chunk))) => {
                    let event = match chunk {
                        ProcessOutputChunk::Stdout(bytes) => EngineEvent::Stdout(bytes),
                        ProcessOutputChunk::Stderr(bytes) => EngineEvent::Stderr(bytes),
                    };
                    let event_len = event_bytes(&event).len();
                    let used = stdout.len().saturating_add(stderr.len());
                    if used.saturating_add(event_len) > options.output_limit {
                        return reap(
                            session,
                            CommandError::OutputLimit {
                                limit: options.output_limit,
                                reaped_pid: Some(pid),
                                cleanup: None,
                            },
                        )
                        .await;
                    }
                    match &event {
                        EngineEvent::Stdout(bytes) => stdout.extend_from_slice(bytes),
                        EngineEvent::Stderr(bytes) => stderr.extend_from_slice(bytes),
                    }
                    match events.try_send(event) {
                        Ok(()) => {}
                        Err(async_engine::TrySendError::Full(_)) => {
                            return reap(
                                session,
                                CommandError::OutputConsumerSlow {
                                    reaped_pid: Some(pid),
                                    cleanup: None,
                                },
                            )
                            .await;
                        }
                        Err(async_engine::TrySendError::Closed(_)) => {
                            return reap(
                                session,
                                CommandError::OutputConsumerClosed {
                                    reaped_pid: Some(pid),
                                    cleanup: None,
                                },
                            )
                            .await;
                        }
                    }
                }
                Ok(Some(ProcessOutputEvent::Completion(completion))) => {
                    if !matches!(
                        completion,
                        ProcessOutputCompletion::StdoutEof | ProcessOutputCompletion::StderrEof
                    ) {
                        return reap(
                            session,
                            CommandError::OutputCompletion {
                                detail: format!("{completion:?}"),
                                reaped_pid: Some(pid),
                                cleanup: None,
                            },
                        )
                        .await;
                    }
                }
                Ok(None) => {
                    if let Some(exit) = session.poll().await.map_err(CommandError::Io)? {
                        return Ok(CommandResult {
                            exit_code: exit.exit_code().unwrap_or(-exit.signal().unwrap_or(0)),
                            stdout,
                            stderr,
                        });
                    }
                    async_engine::sleep(SESSION_POLL).await;
                }
                Err(_) => {}
            }
        }
    }
    /// Read-only Docker client/server version plus daemon information.
    pub fn diagnostics(&self, options: RunOptions) -> Result<DockerDiagnostics, CommandError> {
        let client = self
            .with_args(["version", "--format", "{{.Client.Version}}"])
            .capture(options)?;
        let server = self
            .with_args(["version", "--format", "{{.Server.Version}}"])
            .capture(options)?;
        let info = self
            .with_args(["info", "--format", "{{.SystemTime}}"])
            .capture(options)?;
        Ok(DockerDiagnostics {
            client,
            server,
            info,
        })
    }
    /// Run Bosn's fixed, read-only Docker health probe.  Unlike [`Self::capture`]
    /// and [`Self::diagnostics`], this is safe to expose through a product
    /// diagnostic boundary: callers cannot select argv and no engine output is
    /// returned.  The single `docker version` invocation is bounded by the
    /// supplied deadline/output limit and does not pull, inspect, or mutate
    /// engine state.
    pub async fn doctor_async(&self, options: RunOptions) -> DockerDoctorReport {
        let result = self
            .with_args([
                "version",
                "--format",
                "{{.Client.Version}}|{{.Server.Version}}",
            ])
            .capture_async(options)
            .await;
        doctor_report(result)
    }
    /// Read-only Docker accounting, as one bounded `docker system df -v --format json`.
    ///
    /// This reports images, containers, volumes, and build cache together. Unlike
    /// [`Self::capture`] it does not let the caller select argv, and it never pulls,
    /// prunes, creates, inspects, or otherwise mutates engine state. Parsing and
    /// classification are pure and live in `bosn-core`.
    pub fn system_df_verbose(&self, options: RunOptions) -> Result<CensusRead, CommandError> {
        let result = self
            .with_args(["system", "df", "-v", "--format", "json"])
            .capture(options)?;
        Ok(census_read(result, "docker system df -v"))
    }

    /// Read-only image IDs Docker itself considers dangling.
    ///
    /// This is the authoritative signal. An untagged image that is still the parent of a
    /// tagged one is *not* dangling, and inferring "untagged" from the accounting document
    /// would over-report it as reclaimable.
    pub fn image_ids_dangling(&self, options: RunOptions) -> Result<CensusRead, CommandError> {
        let result = self
            .with_args([
                "image",
                "ls",
                "-a",
                "-q",
                "--no-trunc",
                "--filter",
                "dangling=true",
            ])
            .capture(options)?;
        Ok(census_read(result, "docker image ls --filter dangling"))
    }

    /// Read-only detail for specific volumes, as one bounded `docker volume inspect`.
    ///
    /// The accounting document reports no volume creation time, so volume age is only
    /// available here. Callers batch the names they need rather than probing one at a time.
    pub fn inspect_volumes(
        &self,
        names: &[String],
        options: RunOptions,
    ) -> Result<CensusRead, CommandError> {
        let mut args: Vec<OsString> = ["volume", "inspect"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(names.iter().cloned().map(OsString::from));
        let result = self.with_args(args).capture(options)?;
        Ok(census_read(result, "docker volume inspect"))
    }

    /// Read-only image IDs carrying one label key, as a bounded `docker image ls`.
    ///
    /// `docker image ls` does not expose labels, so image ownership cannot be read from the
    /// accounting document alone. Callers query the keys they care about and union the
    /// results; the engine itself knows nothing about Bosn's label contract.
    pub fn image_ids_with_label(
        &self,
        key: &str,
        options: RunOptions,
    ) -> Result<CensusRead, CommandError> {
        let filter = format!("label={key}");
        let result = self
            .with_args([
                "image",
                "ls",
                "-a",
                "-q",
                "--no-trunc",
                "--filter",
                filter.as_str(),
            ])
            .capture(options)?;
        Ok(census_read(result, "docker image ls --filter label"))
    }

    /// Append trusted, product-selected Docker CLI arguments. This local
    /// transport is not a sandbox or authorization boundary and is not RPC.
    #[must_use]
    pub fn with_args(&self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        let mut next = self.clone();
        next.prefix.extend(args.into_iter().map(Into::into));
        next
    }
    /// Set a Docker build/context directory without shell interpolation.
    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }
    /// Add one explicit Docker CLI environment override.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerDiagnostics {
    pub client: CommandResult,
    pub server: CommandResult,
    pub info: CommandResult,
}

/// Stable, redacted outcome of the fixed Docker health probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerDoctorReport {
    pub state: DockerDoctorState,
    pub client_version: Option<String>,
    pub server_version: Option<String>,
}

/// No raw Docker stderr, endpoint, environment, or process error crosses this
/// type.  `Unavailable` includes absent binaries and unreachable daemons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerDoctorState {
    Ready,
    Unavailable,
    Deadline,
    OutputLimit,
    InvalidResponse,
}

/// Convert one bounded capture into a census read, so a refusal is never mistaken for an
/// empty result.
fn census_read(result: CommandResult, what: &str) -> CensusRead {
    if !result.ok() {
        return CensusRead::Unavailable {
            detail: format!("{what} exited with {}", result.exit_code),
        };
    }
    match String::from_utf8(result.stdout) {
        Ok(text) => CensusRead::Document(text),
        Err(_) => CensusRead::Unavailable {
            detail: format!("{what} returned non-UTF-8 output"),
        },
    }
}

fn doctor_report(result: Result<CommandResult, CommandError>) -> DockerDoctorReport {
    let unavailable = || DockerDoctorReport {
        state: DockerDoctorState::Unavailable,
        client_version: None,
        server_version: None,
    };
    let result = match result {
        Ok(result) if result.ok() => result,
        Ok(_) | Err(CommandError::Spawn(_)) | Err(CommandError::Io(_)) => return unavailable(),
        Err(CommandError::Deadline { .. }) => {
            return DockerDoctorReport {
                state: DockerDoctorState::Deadline,
                client_version: None,
                server_version: None,
            };
        }
        Err(CommandError::OutputLimit { .. }) => {
            return DockerDoctorReport {
                state: DockerDoctorState::OutputLimit,
                client_version: None,
                server_version: None,
            };
        }
        Err(_) => return unavailable(),
    };
    let Ok(text) = std::str::from_utf8(&result.stdout) else {
        return invalid_doctor_response();
    };
    let Some((client, server)) = text.trim_end_matches(['\r', '\n']).split_once('|') else {
        return invalid_doctor_response();
    };
    if !valid_version(client) || !valid_version(server) || server.contains('|') {
        return invalid_doctor_response();
    }
    DockerDoctorReport {
        state: DockerDoctorState::Ready,
        client_version: Some(client.into()),
        server_version: Some(server.into()),
    }
}

fn invalid_doctor_response() -> DockerDoctorReport {
    DockerDoctorReport {
        state: DockerDoctorState::InvalidResponse,
        client_version: None,
        server_version: None,
    }
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

fn event_bytes(event: &EngineEvent) -> &[u8] {
    match event {
        EngineEvent::Stdout(bytes) | EngineEvent::Stderr(bytes) => bytes,
    }
}
fn outcome_suffix(reaped_pid: Option<u32>, cleanup: &Option<String>) -> String {
    let reaped = if reaped_pid.is_some() {
        " and was reaped"
    } else {
        ""
    };
    match cleanup {
        Some(detail) => format!("{reaped}; cleanup failed: {detail}"),
        None => reaped.into(),
    }
}
async fn reap(
    session: kernal_api::ProcessSession,
    error: CommandError,
) -> Result<CommandResult, CommandError> {
    let cleanup = match session.kill().await {
        Ok(()) => session.wait().await.map(|_| ()),
        Err(error) => Err(error),
    };
    // `kill` is documented to reap; still wait to preserve an explicit terminal
    // observation if the facade implementation changes its internal sequencing.
    Err(with_cleanup(
        error,
        cleanup.err().map(|error| error.to_string()),
    ))
}
fn with_cleanup(error: CommandError, cleanup: Option<String>) -> CommandError {
    match error {
        CommandError::Deadline { reaped_pid, .. } => CommandError::Deadline {
            reaped_pid: cleanup.is_none().then_some(reaped_pid).flatten(),
            cleanup,
        },
        CommandError::Cancelled { reaped_pid, .. } => CommandError::Cancelled {
            reaped_pid: cleanup.is_none().then_some(reaped_pid).flatten(),
            cleanup,
        },
        CommandError::OutputLimit {
            limit, reaped_pid, ..
        } => CommandError::OutputLimit {
            limit,
            reaped_pid: cleanup.is_none().then_some(reaped_pid).flatten(),
            cleanup,
        },
        CommandError::OutputCompletion {
            detail, reaped_pid, ..
        } => CommandError::OutputCompletion {
            detail,
            reaped_pid: cleanup.is_none().then_some(reaped_pid).flatten(),
            cleanup,
        },
        CommandError::OutputConsumerSlow { reaped_pid, .. } => CommandError::OutputConsumerSlow {
            reaped_pid: cleanup.is_none().then_some(reaped_pid).flatten(),
            cleanup,
        },
        CommandError::OutputConsumerClosed { reaped_pid, .. } => {
            CommandError::OutputConsumerClosed {
                reaped_pid: cleanup.is_none().then_some(reaped_pid).flatten(),
                cleanup,
            }
        }
        other => other,
    }
}
fn map_bounded(error: BoundedProcessError) -> CommandError {
    match error {
        BoundedProcessError::TimedOut => CommandError::Deadline {
            reaped_pid: None,
            cleanup: None,
        },
        BoundedProcessError::OutputLimitExceeded { limit } => CommandError::OutputLimit {
            limit,
            reaped_pid: None,
            cleanup: None,
        },
        BoundedProcessError::Spawn(error) => CommandError::Spawn(error),
        BoundedProcessError::Io(error) => CommandError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandError, CommandResult, DockerDoctorState, GuestScpCommand, GuestSshCommand,
        doctor_report,
    };
    use std::path::PathBuf;

    #[test]
    fn guest_ssh_command_ignores_ambient_config_and_fixes_loopback_target() {
        let command = GuestSshCommand {
            user: "runner".into(),
            port: 2222,
            identity_file: PathBuf::from("/state/guest-ssh/key"),
            command: "echo declared".into(),
        };
        let args = command
            .args()
            .into_iter()
            .map(|value| value.into_string().unwrap())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-F", "/dev/null"]));
        assert!(args.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert!(args.contains(&"runner@127.0.0.1".into()));
        assert!(args.contains(&"BatchMode=yes".into()));
        assert!(args.contains(&"IdentitiesOnly=yes".into()));
        assert_eq!(args.last().unwrap(), "echo declared");
        assert!(!args.iter().any(|value| value.contains("ProxyCommand")));
    }

    #[test]
    fn guest_scp_command_is_fixed_to_loopback_and_uses_scp_port_spelling() {
        let command = GuestScpCommand {
            user: "runner".into(),
            port: 2222,
            identity_file: PathBuf::from("/state/guest-ssh/key"),
            source: PathBuf::from("/workspace/out/archive.tar.zst"),
            destination: "~/archive.tar.zst".into(),
        };
        let args = command
            .args()
            .into_iter()
            .map(|value| value.into_string().unwrap())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-F", "/dev/null"]));
        assert!(args.windows(2).any(|pair| pair == ["-P", "2222"]));
        assert!(args.contains(&"/workspace/out/archive.tar.zst".into()));
        assert_eq!(args.last().unwrap(), "runner@127.0.0.1:~/archive.tar.zst");
        assert!(args.contains(&"IdentitiesOnly=yes".into()));
        assert!(!args.iter().any(|value| value.contains("ProxyCommand")));
    }

    #[test]
    fn error_display_claims_reaping_only_after_confirmed_cleanup() {
        assert_eq!(
            CommandError::Deadline {
                reaped_pid: None,
                cleanup: None
            }
            .to_string(),
            "Docker CLI exceeded its deadline"
        );
        assert_eq!(
            CommandError::Cancelled {
                reaped_pid: Some(7),
                cleanup: None
            }
            .to_string(),
            "Docker CLI was cancelled and was reaped"
        );
        assert_eq!(
            CommandError::Deadline {
                reaped_pid: None,
                cleanup: Some("wait failed".into())
            }
            .to_string(),
            "Docker CLI exceeded its deadline; cleanup failed: wait failed"
        );
    }

    #[test]
    fn all_output_consumer_states_have_specific_display() {
        assert_eq!(
            CommandError::OutputLimit {
                limit: 3,
                reaped_pid: Some(7),
                cleanup: Some("wait failed".into())
            }
            .to_string(),
            "Docker CLI output exceeded 3 bytes and was reaped; cleanup failed: wait failed"
        );
        assert_eq!(
            CommandError::OutputCompletion {
                detail: "fault".into(),
                reaped_pid: None,
                cleanup: Some("wait failed".into())
            }
            .to_string(),
            "Docker CLI output did not complete cleanly: fault; cleanup failed: wait failed"
        );
        assert_eq!(
            CommandError::OutputConsumerSlow {
                reaped_pid: None,
                cleanup: None
            }
            .to_string(),
            "Docker CLI output consumer was too slow"
        );
        assert_eq!(
            CommandError::OutputConsumerClosed {
                reaped_pid: Some(7),
                cleanup: None
            }
            .to_string(),
            "Docker CLI output consumer closed and was reaped"
        );
    }

    #[test]
    fn doctor_result_is_structured_and_never_retains_raw_output() {
        let ready = doctor_report(Ok(CommandResult {
            exit_code: 0,
            stdout: b"29.0.1|29.0.1\n".to_vec(),
            stderr: b"secret engine warning".to_vec(),
        }));
        assert_eq!(ready.state, DockerDoctorState::Ready);
        assert_eq!(ready.client_version.as_deref(), Some("29.0.1"));
        assert_eq!(ready.server_version.as_deref(), Some("29.0.1"));

        let invalid = doctor_report(Ok(CommandResult {
            exit_code: 0,
            stdout: b"version|value|extra".to_vec(),
            stderr: Vec::new(),
        }));
        assert_eq!(invalid.state, DockerDoctorState::InvalidResponse);
        assert_eq!(invalid.client_version, None);

        let limited = doctor_report(Err(CommandError::OutputLimit {
            limit: 1,
            reaped_pid: None,
            cleanup: None,
        }));
        assert_eq!(limited.state, DockerDoctorState::OutputLimit);
        assert_eq!(limited.server_version, None);
    }
}
