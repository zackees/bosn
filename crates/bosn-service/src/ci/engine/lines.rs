//! Bounded line splitting for act's streamed output.

/// The longest line kept whole. act wraps each output line in one JSON object, so a line must
/// fit whole to be parsed: a step's ~64 KiB output-evidence payload plus its JSON envelope and
/// escaping is far past the old 64 KiB bound, and splitting it produced malformed records that
/// turned a green run incomplete (#563).
pub(super) const MAX_LINE: usize = 16 * 1024 * 1024;

/// One line of output.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Line {
    Whole(String),
    /// The first [`MAX_LINE`] bytes of a longer line; the rest of it was discarded.
    Truncated(String),
}

/// Splits a byte stream into bounded UTF-8 lines.
#[derive(Default)]
pub(super) struct LineBuffer {
    pending: Vec<u8>,
    /// Dropping the rest of an over-long line, up to its newline.
    discarding: bool,
}
impl LineBuffer {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }
    pub(super) fn drain_lines(&mut self) -> Vec<Line> {
        let mut out = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if std::mem::take(&mut self.discarding) {
                continue;
            }
            out.push(Line::Whole(
                String::from_utf8_lossy(&line[..line.len() - 1]).into_owned(),
            ));
        }
        if self.discarding {
            self.pending.clear();
        } else if self.pending.len() > MAX_LINE {
            let line: Vec<u8> = self.pending.drain(..MAX_LINE).collect();
            out.push(Line::Truncated(String::from_utf8_lossy(&line).into_owned()));
            self.pending.clear();
            self.discarding = true;
        }
        out
    }
    pub(super) fn finish(&mut self) -> Vec<Line> {
        let mut out = self.drain_lines();
        let rest = std::mem::take(&mut self.pending);
        if !self.discarding && !rest.is_empty() {
            out.push(Line::Whole(String::from_utf8_lossy(&rest).into_owned()));
        }
        out
    }
}
