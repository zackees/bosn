//! Authenticated bounded RPC transport shared by typed client operations.
use super::*;

impl Client {
    pub(crate) async fn call(&self, request: Request) -> Result<Reply, Error> {
        self.call_within(request, IO_DEADLINE).await
    }
    /// One request whose reply may take up to `reply_deadline` to arrive.
    pub(crate) async fn call_within(
        &self,
        request: Request,
        reply_deadline: Duration,
    ) -> Result<Reply, Error> {
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
        let mut payload = Vec::new();
        request
            .encode(&mut payload)
            .map_err(|_| Error::Protocol("encode"))?;
        write_frame(
            &mut stream,
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(1),
        )
        .await?;
        let frame = read_frame_within(&mut stream, reply_deadline).await?;
        decode_response_frame(frame, 1)
    }
}
