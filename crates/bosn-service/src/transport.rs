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

/// What a daemon says about itself on a ping reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonIdentity {
    /// The release version. Empty from bosn 0.1.5 and older.
    pub release: String,
    /// The wire protocol. Zero from a daemon that predates #509.
    pub protocol: u32,
}

/// Explain a daemon from another release instead of letting it misread this
/// client's requests (it may answer with a reset connection or a refusal).
/// `None` when the versions match. An empty `daemon_version` is a daemon that
/// predates the version handshake (bosn 0.1.5 and older).
pub fn daemon_version_mismatch(
    state_dir: &Path,
    client_version: &str,
    daemon_version: &str,
) -> Option<String> {
    if daemon_version == client_version {
        return None;
    }
    let daemon = if daemon_version.is_empty() {
        "an older bosn (0.1.5 or earlier, which does not report its version)".to_owned()
    } else {
        format!("bosn {daemon_version}")
    };
    let state = state_dir.display();
    Some(format!(
        "the bosn daemon for {state} is {daemon}, but this client is bosn {client_version}; \
         a daemon from another release can misread this client's requests. \
         Stop it with `bosn daemon stop --state-dir {state}` (this also cancels any job it \
         is running for another session), then retry: a matching daemon starts on demand"
    ))
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
