//! Bounded line splitting for act's streamed output.

/// Splits a byte stream into bounded UTF-8 lines.
#[derive(Default)]
pub(super) struct LineBuffer {
    pending: Vec<u8>,
}
pub(super) const MAX_LINE: usize = 64 * 1024;
impl LineBuffer {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }
    pub(super) fn drain_lines(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            out.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        if self.pending.len() > MAX_LINE {
            let line: Vec<u8> = self.pending.drain(..MAX_LINE).collect();
            out.push(String::from_utf8_lossy(&line).into_owned());
        }
        out
    }
    pub(super) fn finish(&mut self) -> Vec<String> {
        let mut out = self.drain_lines();
        if !self.pending.is_empty() {
            out.push(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned());
        }
        out
    }
}
