//! Bounded framed I/O while a remote command holds its coordination lease.
use kernal_api::{ProcessOutputChunk, ProcessOutputEvent, ProcessSession, async_engine};
use std::time::{Duration, Instant};

pub(super) struct ProcessControl {
    pub(super) session: ProcessSession,
    pub(super) expires: Instant,
    label: &'static str,
    end_marker: &'static [u8],
    pending: Vec<u8>,
    stderr: Vec<u8>,
    received: usize,
}

impl ProcessControl {
    pub(super) fn new(
        session: ProcessSession,
        label: &'static str,
        end_marker: &'static [u8],
        lifetime: Duration,
    ) -> Self {
        Self {
            session,
            expires: Instant::now() + lifetime,
            label,
            end_marker,
            pending: Vec::new(),
            stderr: Vec::new(),
            received: 0,
        }
    }

    pub(super) fn remaining(&self) -> Result<Duration, String> {
        let remaining = self.expires.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(format!("{} session deadline exceeded", self.label))
        } else {
            Ok(remaining)
        }
    }

    pub(super) fn diagnostic(&self) -> String {
        String::from_utf8_lossy(&self.stderr)
            .chars()
            .take(256)
            .collect()
    }

    pub(super) async fn send(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > 64 * 1024 {
            return Err(format!("{} command exceeds 64 KiB", self.label));
        }
        async_engine::timeout(self.remaining()?, self.session.write_stdin(bytes))
            .await
            .map_err(|_| format!("{} write deadline exceeded", self.label))?
            .map_err(|error| error.to_string())
    }

    pub(super) async fn line(&mut self) -> Result<Vec<u8>, String> {
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=end).collect();
                line.pop();
                return Ok(line);
            }
            let event = async_engine::timeout(self.remaining()?, self.session.next_output())
                .await
                .map_err(|_| format!("{} read deadline exceeded", self.label))?;
            match event {
                Some(ProcessOutputEvent::Chunk(chunk)) => {
                    let (destination, bytes) = match chunk {
                        ProcessOutputChunk::Stdout(bytes) => (&mut self.pending, bytes),
                        ProcessOutputChunk::Stderr(bytes) => (&mut self.stderr, bytes),
                    };
                    self.received = self.received.saturating_add(bytes.len());
                    if self.received > 256 * 1024
                        || destination.len().saturating_add(bytes.len()) > 64 * 1024
                    {
                        return Err(format!(
                            "{} protocol output exceeds bounded evidence",
                            self.label
                        ));
                    }
                    destination.extend_from_slice(&bytes);
                }
                Some(ProcessOutputEvent::Completion(_)) => {}
                None => {
                    return Err(format!(
                        "{} protocol ended before acknowledgement: {}",
                        self.label,
                        self.diagnostic()
                    ));
                }
            }
        }
    }

    pub(super) async fn command(&mut self, command: &[u8]) -> Result<(i32, Vec<u8>), String> {
        self.send(command).await?;
        let mut body = Vec::new();
        loop {
            let line = self.line().await?;
            if let Some(code) = line.strip_prefix(self.end_marker) {
                let code: i32 = std::str::from_utf8(code)
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .filter(|code| (0..=255).contains(code))
                    .ok_or_else(|| format!("invalid {} exit evidence", self.label))?;
                return Ok((code, body));
            }
            if body.len().saturating_add(line.len()).saturating_add(1) > 64 * 1024 {
                return Err(format!("{} command evidence exceeds 64 KiB", self.label));
            }
            body.extend_from_slice(&line);
            body.push(b'\n');
        }
    }
}
