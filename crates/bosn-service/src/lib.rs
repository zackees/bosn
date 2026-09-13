//! Small, authenticated Rust daemon foundation. Product protobuf remains private.

use bosn_registry::{Registry, RegistryStatus};
use kernal_api::{
    async_engine::{self, CancellationSource},
    daemon_frame_v1::{
        DaemonFrame, DaemonFrameCodec, DaemonFrameDecode, DaemonFrameKind, DaemonPayloadEncoding,
    },
    platform::ipc::{self, AsyncListener, AsyncStream, Endpoint, EndpointAddressCandidates},
};
use prost::Message;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub const PROTOCOL_VERSION: u32 = 1;
const PAYLOAD_PROTOCOL: u32 = 0x4253_4e01;
const MAX_FRAME: usize = 1024 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub registry_id: String,
    pub schema_version: u32,
    pub resources: u64,
    pub leases: u64,
    pub sessions: u64,
    pub reconciliation_required: bool,
}
impl From<RegistryStatus> for Status {
    fn from(v: RegistryStatus) -> Self {
        Self {
            registry_id: v.registry_id,
            schema_version: v.schema_version,
            resources: v.resources,
            leases: v.leases,
            sessions: v.sessions,
            reconciliation_required: v.reconciliation_required,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    state_dir: PathBuf,
}
impl Client {
    pub fn for_state(state: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            state_dir: state.as_ref().to_path_buf(),
        })
    }
    pub async fn ping(&self) -> Result<(), Error> {
        match self.call(1).await? {
            Reply::Pong => Ok(()),
            _ => Err(Error::Protocol("unexpected ping response")),
        }
    }
    pub async fn status(&self) -> Result<Status, Error> {
        match self.call(2).await? {
            Reply::Status(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected status response")),
        }
    }
    pub async fn shutdown(&self) -> Result<(), Error> {
        match self.call(3).await? {
            Reply::Shutdown => Ok(()),
            _ => Err(Error::Protocol("unexpected shutdown response")),
        }
    }
    async fn call(&self, operation: u32) -> Result<Reply, Error> {
        // Resolve on every call: a Client may have been constructed while a
        // fresh daemon was still creating its registry, before an inode-based
        // alias-stable endpoint name existed.
        let ep = endpoint(&self.state_dir)?;
        let mut stream = async_engine::timeout(IO_DEADLINE, AsyncStream::connect(&ep))
            .await
            .map_err(|_| Error::Deadline)??;
        if !peer_is_authorized(&stream.peer_identity()?.user_id, &ipc::current_user_id()?) {
            return Err(Error::Unauthorized);
        }
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            operation,
        };
        let mut payload = Vec::new();
        request
            .encode(&mut payload)
            .map_err(|_| Error::Protocol("encode"))?;
        write_frame(
            &mut stream,
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(1),
        )
        .await?;
        let frame = read_frame(&mut stream).await?;
        decode_response_frame(frame, 1)
    }
}

pub struct Service {
    state_dir: PathBuf,
    stop: CancellationSource,
}
#[derive(Clone)]
struct RegistryActor {
    sender: async_engine::Sender<DbCommand>,
}
enum DbCommand {
    Status(async_engine::OneshotSender<Result<Status, Error>>),
    Stop(async_engine::OneshotSender<()>),
}
impl RegistryActor {
    async fn status(&self) -> Result<Status, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::Status(reply))
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    async fn stop(&self) {
        let (reply, wait) = async_engine::oneshot_channel();
        if self.sender.send(DbCommand::Stop(reply)).await.is_ok() {
            let _ = wait.await;
        }
    }
}
async fn registry_actor(mut registry: Registry, mut receiver: async_engine::Receiver<DbCommand>) {
    while let Some(command) = receiver.recv().await {
        match command {
            DbCommand::Status(reply) => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.status().map(Status::from);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::Stop(reply) => {
                let _ = reply.send(());
                return;
            }
        }
    }
}
impl Service {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            stop: CancellationSource::new(),
        }
    }
    /// Foreground lifecycle: acquires the sole registry writer before binding.
    pub async fn serve(self) -> Result<(), Error> {
        ipc::ensure_owner_private_directory(&self.state_dir)?;
        let db = self.state_dir.join("registry.sqlite3");
        let registry = match kernal_api::platform::fs::path_identity(&db) {
            Ok(Some(_)) => async_engine::launch_blocking(move || Registry::open_writer(&db))
                .await
                .map_err(|_| Error::ActorClosed)??,
            Ok(None) => return Err(Error::Protocol("registry identity unavailable")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let bytes = kernal_api::random::SecureRandom::new(1, IO_DEADLINE)
                    .map_err(|_| Error::Random)?
                    .bytes(16)
                    .await
                    .map_err(|_| Error::Random)?;
                async_engine::launch_blocking(move || Registry::create_writer(&db, &uuid(&bytes)))
                    .await
                    .map_err(|_| Error::ActorClosed)??
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let ep = endpoint(&self.state_dir)?;
        if ep.target_exists()? {
            return Err(Error::EndpointOccupied(ep.display().into()));
        }
        let listener = AsyncListener::bind_owner_only(&ep)?;
        let (sender, receiver) = async_engine::channel(16);
        let actor = RegistryActor { sender };
        let worker = async_engine::launch(registry_actor(registry, receiver));
        let mut clients = async_engine::TaskGroup::new();
        while !self.stop.is_cancelled() {
            // TaskGroup retains completed tasks until collected.  Reap only
            // ready completions: awaiting one here would let 32 slow peers
            // prevent the accept loop from serving everyone else.
            while matches!(
                async_engine::timeout(Duration::ZERO, clients.join_next()).await,
                Ok(Some(_))
            ) {}
            let accepted =
                async_engine::timeout(Duration::from_millis(100), listener.accept()).await;
            let stream = match accepted {
                Ok(Ok(stream)) => stream,
                Ok(Err(_)) => {
                    // A persistent listener failure must not turn this foreground
                    // process into a hot loop.  The listener remains owned, so
                    // yield before observing it again.
                    async_engine::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                Err(_) => continue,
            };
            if clients.len() >= 32 {
                // Admission is bounded. Dropping this newly accepted stream is
                // intentional; do not wait for a slow peer to make capacity.
                continue;
            }
            let actor = actor.clone();
            let stop = self.stop.clone();
            clients.spawn(async move { handle(stream, actor, stop).await });
        }
        while clients.join_next().await.is_some() {}
        actor.stop().await;
        let _ = worker.await;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Registry(bosn_registry::Error),
    Deadline,
    Unauthorized,
    Random,
    EndpointOccupied(String),
    ActorClosed,
    Protocol(&'static str),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<bosn_registry::Error> for Error {
    fn from(e: bosn_registry::Error) -> Self {
        Self::Registry(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

fn endpoint(state: &Path) -> Result<Endpoint, Error> {
    // A database inode is stable across spelling aliases (including the
    // Windows namespace rules supplied by kernal-api).  Before the database
    // exists, retain the supplied path spelling so two fresh state roots do
    // not collide.  The current user is always part of the namespace.
    let db = state.join("registry.sqlite3");
    let mut identity = ipc::current_user_id()?.into_bytes();
    identity.push(0);
    match kernal_api::platform::fs::path_identity(&db) {
        Ok(Some(file)) => {
            identity.extend_from_slice(&file.device.to_le_bytes());
            identity.extend_from_slice(&file.file.to_le_bytes());
        }
        Ok(None) => {
            identity.extend_from_slice(&kernal_api::platform::ipc::endpoint_scope_bytes(state));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            identity.extend_from_slice(&kernal_api::platform::ipc::endpoint_scope_bytes(state));
        }
        Err(error) => return Err(Error::Io(error)),
    }
    let name = format!(
        "com.zackees.bosn.{}",
        kernal_api::hash::blake3_bytes(&identity).to_hex()
    );
    let address = EndpointAddressCandidates::new(Some(name), Some(state.join("bosn-rs.sock")))
        .select()
        .ok_or(Error::Protocol("no local IPC transport"))?;
    Ok(Endpoint::new(address)?)
}
fn uuid(bytes: &[u8]) -> String {
    let mut b = [0_u8; 16];
    b.copy_from_slice(&bytes[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}
async fn handle(
    mut s: AsyncStream,
    actor: RegistryActor,
    stop: CancellationSource,
) -> Result<(), Error> {
    if !peer_is_authorized(&s.peer_identity()?.user_id, &ipc::current_user_id()?) {
        return Err(Error::Unauthorized);
    }
    let f = read_frame(&mut s).await?;
    if f.payload_protocol() != PAYLOAD_PROTOCOL
        || f.kind_classification() != DaemonFrameKind::Request
        || f.payload_encoding_classification() != DaemonPayloadEncoding::None
    {
        return Err(Error::Protocol("request frame"));
    }
    let r = Request::decode(f.payload()).map_err(|_| Error::Protocol("request decode"))?;
    let reply = if r.protocol_version != PROTOCOL_VERSION {
        ReplyWire {
            code: 1,
            registry_id: String::new(),
            schema_version: 0,
            resources: 0,
            leases: 0,
            sessions: 0,
            reconciliation_required: false,
        }
    } else {
        match r.operation {
            1 => ReplyWire {
                code: 10,
                ..Default::default()
            },
            2 => {
                let status = actor.status().await?;
                ReplyWire {
                    code: 20,
                    registry_id: status.registry_id,
                    schema_version: status.schema_version,
                    resources: status.resources,
                    leases: status.leases,
                    sessions: status.sessions,
                    reconciliation_required: status.reconciliation_required,
                }
            }
            3 => {
                stop.cancel();
                ReplyWire {
                    code: 30,
                    ..Default::default()
                }
            }
            _ => ReplyWire {
                code: 2,
                ..Default::default()
            },
        }
    };
    let mut p = Vec::new();
    reply
        .encode(&mut p)
        .map_err(|_| Error::Protocol("reply encode"))?;
    write_frame(&mut s, DaemonFrame::response_to(&f, p)).await
}
fn peer_is_authorized(peer_user_id: &str, expected_user_id: &str) -> bool {
    !peer_user_id.is_empty() && peer_user_id == expected_user_id
}
async fn read_frame(s: &mut AsyncStream) -> Result<DaemonFrame, Error> {
    let mut b = Vec::new();
    let deadline = async_engine::Deadline::after(IO_DEADLINE);
    loop {
        if b.len() > MAX_FRAME {
            return Err(Error::Protocol("frame too large"));
        }
        match DaemonFrameCodec::decode(&b).map_err(|_| Error::Protocol("bad frame"))? {
            DaemonFrameDecode::Frame { frame, consumed } if consumed <= MAX_FRAME => {
                return Ok(frame);
            }
            DaemonFrameDecode::Frame { .. } => return Err(Error::Protocol("frame too large")),
            DaemonFrameDecode::NeedMoreBytes => {
                let mut chunk = [0; 4096];
                let n = async_engine::timeout_at(deadline, s.read(&mut chunk))
                    .await
                    .map_err(|_| Error::Deadline)??;
                if n == 0 {
                    return Err(Error::Protocol("eof"));
                }
                b.extend_from_slice(&chunk[..n]);
            }
        }
    }
}
async fn write_frame(s: &mut AsyncStream, f: DaemonFrame) -> Result<(), Error> {
    let b = DaemonFrameCodec::encode(&f).map_err(|_| Error::Protocol("frame encode"))?;
    if b.len() > MAX_FRAME {
        return Err(Error::Protocol("frame too large"));
    }
    async_engine::timeout(IO_DEADLINE, s.write_all(&b))
        .await
        .map_err(|_| Error::Deadline)??;
    Ok(())
}
fn decode_response_frame(frame: DaemonFrame, request_id: u64) -> Result<Reply, Error> {
    if frame.request_id() != request_id
        || frame.kind_classification() != DaemonFrameKind::Response
        || frame.payload_protocol() != PAYLOAD_PROTOCOL
        || frame.payload_encoding_classification() != DaemonPayloadEncoding::None
    {
        return Err(Error::Protocol("response frame"));
    }
    let reply = ReplyWire::decode(frame.payload()).map_err(|_| Error::Protocol("reply decode"))?;
    decode_reply(reply)
}
#[derive(Message)]
struct Request {
    #[prost(uint32, tag = "1")]
    protocol_version: u32,
    #[prost(uint32, tag = "2")]
    operation: u32,
}
#[derive(Message)]
struct ReplyWire {
    #[prost(uint32, tag = "1")]
    code: u32,
    #[prost(string, tag = "2")]
    registry_id: String,
    #[prost(uint32, tag = "3")]
    schema_version: u32,
    #[prost(uint64, tag = "4")]
    resources: u64,
    #[prost(uint64, tag = "5")]
    leases: u64,
    #[prost(uint64, tag = "6")]
    sessions: u64,
    #[prost(bool, tag = "7")]
    reconciliation_required: bool,
}
enum Reply {
    Pong,
    Status(Status),
    Shutdown,
}
fn decode_reply(v: ReplyWire) -> Result<Reply, Error> {
    match v.code {
        10 => Ok(Reply::Pong),
        20 => Ok(Reply::Status(Status {
            registry_id: v.registry_id,
            schema_version: v.schema_version,
            resources: v.resources,
            leases: v.leases,
            sessions: v.sessions,
            reconciliation_required: v.reconciliation_required,
        })),
        30 => Ok(Reply::Shutdown),
        1 => Err(Error::Protocol("unsupported protocol")),
        2 => Err(Error::Protocol("unknown operation")),
        _ => Err(Error::Protocol("daemon error")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::async_engine::RuntimeBuilder;

    #[test]
    fn fresh_daemon_serves_typed_client_and_releases_writer() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let first = async_engine::launch(Service::new(state.clone()).serve());
            async_engine::sleep(Duration::from_millis(100)).await;
            let client = Client::for_state(&state).unwrap();
            client.ping().await.unwrap();
            let before = client.status().await.unwrap();
            assert_eq!(before.schema_version, 5);
            assert!(!before.registry_id.is_empty());
            client.shutdown().await.unwrap();
            stopped(first).await;
            let second = async_engine::launch(Service::new(state.clone()).serve());
            async_engine::sleep(Duration::from_millis(100)).await;
            let after = Client::for_state(&state).unwrap().status().await.unwrap();
            assert_eq!(after.registry_id, before.registry_id);
            Client::for_state(&state).unwrap().shutdown().await.unwrap();
            stopped(second).await;
        });
    }

    #[test]
    fn second_daemon_is_refused_while_first_holds_writer() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let first = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let second = Service::new(state.clone()).serve().await;
            assert!(matches!(
                second,
                Err(Error::Registry(bosn_registry::Error::WriterAlreadyHeld(_)))
            ));
            client.shutdown().await.unwrap();
            stopped(first).await;
        });
    }

    #[test]
    fn status_actor_serves_concurrent_typed_requests_before_clean_stop() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let mut requests = async_engine::TaskGroup::new();
            for _ in 0..8 {
                let client = client.clone();
                requests.spawn(async move { client.status().await });
            }
            while let Some(result) = requests.join_next().await {
                let status = result.unwrap().unwrap();
                assert_eq!(status.schema_version, 5);
            }
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn response_envelope_rejects_wrong_correlation_protocol_kind_and_encoding() {
        let mut payload = Vec::new();
        ReplyWire {
            code: 10,
            ..Default::default()
        }
        .encode(&mut payload)
        .unwrap();
        let request = DaemonFrame::request(PAYLOAD_PROTOCOL, Vec::new()).with_request_id(7);
        assert!(matches!(
            decode_response_frame(DaemonFrame::response_to(&request, payload.clone()), 7),
            Ok(Reply::Pong)
        ));
        for frame in [
            DaemonFrame::response_to(&request, payload.clone()).with_request_id(8),
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload.clone()).with_request_id(7),
            DaemonFrame::response_to(&request, payload.clone()).with_raw_payload_encoding(1),
            DaemonFrame::response_to(
                &DaemonFrame::request(PAYLOAD_PROTOCOL + 1, Vec::new()),
                payload,
            ),
        ] {
            assert!(matches!(
                decode_response_frame(frame, 7),
                Err(Error::Protocol("response frame"))
            ));
        }
    }

    #[test]
    fn peer_authorization_fails_closed_for_empty_or_other_user() {
        assert!(peer_is_authorized("current-user", "current-user"));
        assert!(!peer_is_authorized("", "current-user"));
        assert!(!peer_is_authorized("other-user", "current-user"));
    }

    #[test]
    fn unsupported_request_protocol_returns_typed_error_response() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let mut payload = Vec::new();
            Request {
                protocol_version: PROTOCOL_VERSION + 1,
                operation: 1,
            }
            .encode(&mut payload)
            .unwrap();
            let mut stream = AsyncStream::connect(&endpoint(&state).unwrap())
                .await
                .unwrap();
            write_frame(
                &mut stream,
                DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(42),
            )
            .await
            .unwrap();
            let response = read_frame(&mut stream).await.unwrap();
            assert_eq!(response.request_id(), 42);
            assert!(matches!(
                decode_response_frame(response, 42),
                Err(Error::Protocol("unsupported protocol"))
            ));
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn malformed_oversized_and_stalled_clients_do_not_block_a_healthy_client() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;

            // Each bad peer is handled independently and dropped.  In
            // particular, partial input has an absolute per-frame deadline;
            // it cannot keep resetting a deadline by dribbling bytes.
            send_raw(&state, vec![0xff, 0xff]).await;
            send_raw(
                &state,
                DaemonFrameCodec::encode(
                    &DaemonFrame::request(PAYLOAD_PROTOCOL, Vec::new())
                        .with_raw_payload_encoding(1),
                )
                .unwrap(),
            )
            .await;
            send_raw(
                &state,
                DaemonFrameCodec::encode(&DaemonFrame::request(
                    PAYLOAD_PROTOCOL,
                    vec![0; MAX_FRAME],
                ))
                .unwrap(),
            )
            .await;
            let _stalled = AsyncStream::connect(&endpoint(&state).unwrap())
                .await
                .unwrap();

            async_engine::timeout(Duration::from_millis(500), client.ping())
                .await
                .expect("stalled peer blocked ping")
                .unwrap();
            async_engine::timeout(Duration::from_millis(500), client.status())
                .await
                .expect("stalled peer blocked status")
                .unwrap();
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn slow_drip_frame_has_one_absolute_deadline() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let bytes =
                DaemonFrameCodec::encode(&DaemonFrame::request(PAYLOAD_PROTOCOL, vec![0; 64]))
                    .unwrap();
            let mut stream = AsyncStream::connect(&endpoint(&state).unwrap())
                .await
                .unwrap();
            for byte in bytes.iter().take(5) {
                let _ = stream.write_all(&[*byte]).await;
                async_engine::sleep(Duration::from_millis(800)).await;
            }
            // More than IO_DEADLINE elapsed since the first byte. A per-chunk
            // timeout would retain this client; the absolute deadline releases it.
            async_engine::timeout(Duration::from_millis(500), client.ping())
                .await
                .expect("slow drip blocked ping")
                .unwrap();
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn existing_database_aliases_share_endpoint_identity() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let alias = state.join(".");
            let alias_client = Client::for_state(&alias).unwrap();
            assert_eq!(
                alias_client.status().await.unwrap().registry_id,
                client.status().await.unwrap().registry_id
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    }

    #[test]
    fn regular_preexisting_endpoint_is_preserved_and_writer_is_released() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        // Create the database first so this uses the same inode-keyed endpoint
        // the service would select after acquiring its writer.
        std::fs::create_dir_all(&state).unwrap();
        let db = state.join("registry.sqlite3");
        let registry =
            Registry::create_writer(&db, "00000000-0000-4000-8000-000000000001").unwrap();
        drop(registry);
        let ep = endpoint(&state).unwrap();
        std::fs::write(ep.display(), b"do not remove").unwrap();

        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            assert!(matches!(
                Service::new(state.clone()).serve().await,
                Err(Error::EndpointOccupied(_))
            ));
        });
        assert_eq!(std::fs::read(ep.display()).unwrap(), b"do not remove");
        drop(Registry::open_writer(&db).expect("failed startup retained writer"));
    }

    #[test]
    fn legacy_or_reconciliation_gated_registry_refuses_before_listening_or_mutation() {
        for reconciliation_required in [false, true] {
            let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
            let state = temporary.path().join("state");
            std::fs::create_dir_all(&state).unwrap();
            let db = state.join("registry.sqlite3");
            if reconciliation_required {
                let registry =
                    Registry::create_writer(&db, "00000000-0000-4000-8000-000000000002").unwrap();
                drop(registry);
                let connection = kernal_api::sqlite::Connection::open(&db).unwrap();
                connection.execute("INSERT INTO meta(key,value) VALUES('migration.reconciliation_required','true')", &[]).unwrap();
            } else {
                let connection = kernal_api::sqlite::Connection::open(&db).unwrap();
                connection
                    .execute(
                        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                        &[],
                    )
                    .unwrap();
                connection
                    .execute("INSERT INTO meta VALUES ('schema_version','4')", &[])
                    .unwrap();
            }
            let before = std::fs::read(&db).unwrap();
            let ep = endpoint(&state).unwrap();
            let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
            runtime.run(async {
                let result = Service::new(state.clone()).serve().await;
                if reconciliation_required {
                    assert!(matches!(
                        result,
                        Err(Error::Registry(
                            bosn_registry::Error::ReconciliationRequired
                        ))
                    ));
                } else {
                    assert!(matches!(
                        result,
                        Err(Error::Registry(bosn_registry::Error::LegacyImportRequired(
                            4
                        )))
                    ));
                }
            });
            assert!(!ep.target_exists().unwrap());
            assert_eq!(std::fs::read(&db).unwrap(), before);
        }
    }

    #[test]
    fn independent_state_directories_serve_concurrently() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let left = temporary.path().join("left");
        let right = temporary.path().join("right");
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let left_server = async_engine::launch(Service::new(left.clone()).serve());
            let right_server = async_engine::launch(Service::new(right.clone()).serve());
            let left_client = wait_for_client(&left).await;
            let right_client = wait_for_client(&right).await;
            assert_ne!(
                left_client.status().await.unwrap().registry_id,
                right_client.status().await.unwrap().registry_id
            );
            left_client.shutdown().await.unwrap();
            right_client.shutdown().await.unwrap();
            stopped(left_server).await;
            stopped(right_server).await;
        });
    }

    async fn send_raw(state: &Path, bytes: Vec<u8>) {
        let mut stream = AsyncStream::connect(&endpoint(state).unwrap())
            .await
            .unwrap();
        // A daemon can reject a malformed or oversized frame before accepting
        // the whole write. Either outcome is expected; the caller proves the
        // healthy peer remains available afterwards.
        let _ = stream.write_all(&bytes).await;
    }

    async fn stopped(server: async_engine::Task<Result<(), Error>>) {
        async_engine::timeout(Duration::from_secs(5), server)
            .await
            .expect("service did not stop in time")
            .expect("service task failed")
            .expect("service returned error");
    }

    async fn wait_for_client(state: &Path) -> Client {
        let client = Client::for_state(state).unwrap();
        for _ in 0..30 {
            if client.ping().await.is_ok() {
                return client;
            }
            async_engine::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon did not become ready")
    }
}
