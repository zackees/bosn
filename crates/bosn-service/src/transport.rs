//! The owner-only IPC endpoint: socket naming, framing and peer checks.

use super::*;

/// Reclaim the endpoint a dead daemon left behind, or refuse it.
///
/// Called only while this process holds the registry's sole writer, so no
/// other daemon for this state directory can be alive. Even so, only a Unix
/// socket file that refuses connections (nothing listening) is removed; any
/// other file, a live listener, or an unclassifiable probe stays occupied.
pub(crate) fn retire_stale_socket(ep: &Endpoint) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        let is_socket = std::fs::symlink_metadata(ep.display())
            .is_ok_and(|metadata| metadata.file_type().is_socket());
        if is_socket && ep.is_stale() {
            ep.retire()?;
            eprintln!(
                "bosn daemon serve: removed stale socket {} (no daemon was listening)",
                ep.display()
            );
            return Ok(());
        }
    }
    Err(Error::EndpointOccupied(ep.display().into()))
}

/// The daemon wire protocol this build speaks (#509). Every daemon reports it
/// on a ping reply, so a later client can decide compatibility by protocol
/// rather than by exact release. Bump it on any change to the request/reply
/// wire or the JSON documents it carries; `protocol_surface_is_pinned` fails
/// until the bump and its fingerprint are recorded together.
pub const DAEMON_PROTOCOL: u32 = 1;

/// The oldest client protocol this daemon still serves (#509 phase 3). The
/// daemon serves the window `[DAEMON_PROTOCOL_MIN, DAEMON_PROTOCOL]`: a bump of
/// [`DAEMON_PROTOCOL`] keeps the previous protocol's decoders, so this trails it
/// by one (N-1) and never by more.
pub const DAEMON_PROTOCOL_MIN: u32 = 1;

/// What a daemon says about itself on a ping reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonIdentity {
    /// The release version. Empty from bosn 0.1.5 and older.
    pub release: String,
    /// The newest wire protocol it serves. Zero from a daemon that predates #509.
    pub protocol: u32,
    /// The oldest wire protocol it serves. Zero from a daemon that predates the
    /// protocol window (#509 phase 3), which serves only `protocol`.
    pub protocol_min: u32,
}

impl DaemonIdentity {
    /// The `[min, max]` protocol window this daemon serves. A daemon from
    /// before the window serves exactly the one protocol it reports.
    pub fn protocol_window(&self) -> (u32, u32) {
        match self.protocol_min {
            0 => (self.protocol, self.protocol),
            min => (min, self.protocol),
        }
    }
    /// Whether this daemon serves a client speaking `protocol`. Protocol zero
    /// (a daemon that predates the handshake) serves no declared protocol.
    pub fn serves(&self, protocol: u32) -> bool {
        let (min, max) = self.protocol_window();
        max != 0 && (min..=max).contains(&protocol)
    }
}

/// Describe a protocol window for a message: `protocol 2` or `protocols 1-2`.
pub(crate) fn describe_window((min, max): (u32, u32)) -> String {
    if min == max {
        format!("protocol {max}")
    } else {
        format!("protocols {min}-{max}")
    }
}

/// The daemon's typed refusal of a client outside its protocol window (#509
/// phase 3): both windows and the remedy, in one sentence.
pub(crate) fn protocol_refusal(daemon: &DaemonIdentity, client_protocol: u32) -> String {
    let release = match daemon.release.as_str() {
        "" => "an unknown release".to_owned(),
        release => format!("bosn {release}"),
    };
    format!(
        "protocol_unsupported: the bosn daemon ({release}) serves {}, but this client speaks \
         protocol {client_protocol}; upgrade whichever side is older, or stop the daemon with \
         `bosn daemon stop` (this also cancels any job it is running for another session) \
         so a matching daemon starts on demand",
        describe_window(daemon.protocol_window())
    )
}

/// Explain a daemon this client cannot talk to instead of letting it misread
/// this client's requests (it may answer with a reset connection or a refusal).
///
/// Compatibility is by wire protocol (#509): a daemon whose protocol window
/// holds this client's [`DAEMON_PROTOCOL`] is accepted whatever its release
/// (see [`daemon_release_skew`]). Release equality survives only as the
/// legacy guard for a protocol-0 daemon (see [`legacy_release_matches`]).
/// `None` when compatible. An empty release is a daemon from bosn 0.1.5 or older.
pub fn daemon_version_mismatch(
    state_dir: &Path,
    client_version: &str,
    daemon: &DaemonIdentity,
) -> Option<String> {
    let compatible = match daemon.protocol {
        0 => legacy_release_matches(client_version, daemon),
        _ => daemon.serves(DAEMON_PROTOCOL),
    };
    if compatible {
        return None;
    }
    let release = if daemon.release.is_empty() {
        "an older bosn (0.1.5 or earlier, which does not report its version)".to_owned()
    } else {
        format!("bosn {}", daemon.release)
    };
    let protocol = match daemon.protocol {
        0 => "no protocol (it predates the protocol handshake)".to_owned(),
        _ => describe_window(daemon.protocol_window()),
    };
    let state = state_dir.display();
    Some(format!(
        "the bosn daemon for {state} is {release} speaking {protocol}, but this client is \
         bosn {client_version} speaking protocol {DAEMON_PROTOCOL}; a daemon on another \
         protocol can misread this client's requests. \
         A newer bosn restarts an idle older daemon by itself; otherwise stop it with \
         `bosn daemon stop --state-dir {state}` (this also cancels any job it is running \
         for another session), then retry: a matching daemon starts on demand"
    ))
}

/// The retired release guard (#324), kept only for a daemon that reports
/// protocol 0: it predates the protocol handshake, so its release is the only
/// evidence of what it decodes. Never consulted for a daemon with a protocol.
fn legacy_release_matches(client_version: &str, daemon: &DaemonIdentity) -> bool {
    debug_assert_eq!(daemon.protocol, 0);
    daemon.release == client_version
}

/// A one-line note for a compatible daemon from another release (#509): the
/// protocols match, so the client proceeds. `None` when the releases match or
/// the daemon is not compatible (see [`daemon_version_mismatch`]).
pub fn daemon_release_skew(client_version: &str, daemon: &DaemonIdentity) -> Option<String> {
    (daemon.serves(DAEMON_PROTOCOL) && daemon.release != client_version).then(|| {
        format!(
            "note: the bosn daemon is release {}, this client is {client_version}; \
             the daemon serves {} and this client speaks protocol {DAEMON_PROTOCOL}, \
             so proceeding",
            daemon.release,
            describe_window(daemon.protocol_window())
        )
    })
}

pub(crate) fn peer_is_authorized(peer_user_id: &str, expected_user_id: &str) -> bool {
    !peer_user_id.is_empty() && peer_user_id == expected_user_id
}
pub(crate) async fn read_frame(s: &mut AsyncStream) -> Result<DaemonFrame, Error> {
    read_frame_within(s, IO_DEADLINE).await
}
/// Read one frame, which must arrive in full within `limit`.
pub(crate) async fn read_frame_within(
    s: &mut AsyncStream,
    limit: Duration,
) -> Result<DaemonFrame, Error> {
    let mut b = Vec::new();
    let deadline = async_engine::Deadline::after(limit);
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
pub(crate) async fn write_frame(s: &mut AsyncStream, f: DaemonFrame) -> Result<(), Error> {
    let b = DaemonFrameCodec::encode(&f).map_err(|_| Error::Protocol("frame encode"))?;
    if b.len() > MAX_FRAME {
        return Err(Error::Protocol("frame too large"));
    }
    async_engine::timeout(IO_DEADLINE, s.write_all(&b))
        .await
        .map_err(|_| Error::Deadline)??;
    Ok(())
}
pub(crate) fn decode_response_frame(frame: DaemonFrame, request_id: u64) -> Result<Reply, Error> {
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

pub(crate) fn endpoint(state: &Path) -> Result<Endpoint, Error> {
    // A database inode is stable across spelling aliases (including the
    // Windows namespace rules supplied by kernal-api).  Before the database
    // exists, retain the supplied path spelling so two fresh state roots do
    // not collide.  The current user is always part of the namespace.
    let db = state.join("registry.sqlite3");
    let uid = ipc::current_user_id()?;
    let mut identity = uid.clone().into_bytes();
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
    let hash = kernal_api::hash::blake3_bytes(&identity).to_hex();
    let name = format!("com.zackees.bosn.{hash}");
    let address = EndpointAddressCandidates::new(Some(name), Some(socket_path(state, &uid, &hash)))
        .select()
        .ok_or(Error::Protocol("no local IPC transport"))?;
    Ok(Endpoint::new(address)?)
}
/// The filesystem socket: beside the registry when it fits `sun_path`, else a
/// hashed leaf in a short owner-private directory, so a long state dir (a deep
/// TMPDIR, a nested checkout) still gets an endpoint. The hash is the same
/// alias-stable identity as the kernel-namespace name, and the parent is
/// checked owner-only (0700, real dir, our uid) before the daemon binds.
fn socket_path(state: &Path, uid: &str, hash: &str) -> PathBuf {
    let beside = state.join("bosn-rs.sock");
    if beside.as_os_str().len() < ipc::endpoint_name_limit().max_bytes {
        return beside;
    }
    PathBuf::from(format!("/tmp/bosn-{uid}")).join(format!("{}.sock", &hash[..32]))
}
pub(crate) fn uuid(bytes: &[u8]) -> String {
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
