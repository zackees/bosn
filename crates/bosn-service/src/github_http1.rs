//! The few HTTP/1.1 pieces the GitHub proxy's CONNECT front needs (#650):
//! reading a message head, sizing and copying a body. Blocking I/O over any
//! `Read`/`Write`; the front runs one thread per relayed connection.

use std::io::{self, BufRead, Read, Write};

/// The largest message head accepted (request line or status line plus
/// headers).
pub const MAX_HEAD: usize = 64 * 1024;

/// A whole response: status, headers and body.
pub type Reply = (u16, Vec<(String, String)>, Vec<u8>);

/// A request or response head: its first line and its headers in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub start: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// The first value of `name` (case-insensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether `name` lists `token` (comma-separated, case-insensitive).
    #[must_use]
    pub fn lists(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .flat_map(|(_, v)| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case(token))
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Read one head. `Ok(None)` is a clean end of stream before any byte.
pub fn read_head(reader: &mut impl BufRead) -> io::Result<Option<Head>> {
    let mut lines = Vec::new();
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let read = reader
            .take((MAX_HEAD - total + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return if lines.is_empty() && total == 0 {
                Ok(None)
            } else {
                Err(invalid("stream ended inside a message head"))
            };
        }
        total += read;
        if total > MAX_HEAD {
            return Err(invalid("message head too large"));
        }
        if !line.ends_with(b"\n") {
            return Err(invalid("stream ended inside a message head"));
        }
        while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            line.pop();
        }
        if line.is_empty() {
            if lines.is_empty() {
                // Tolerate a stray CRLF between messages (RFC 9112 2.2).
                continue;
            }
            break;
        }
        lines.push(String::from_utf8(line).map_err(|_| invalid("head is not UTF-8"))?);
    }
    let mut lines = lines.into_iter();
    let start = lines.next().unwrap_or_default();
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("malformed header line"))?;
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(invalid("malformed header name"));
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    Ok(Some(Head { start, headers }))
}

/// How a message's body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyLen {
    None,
    Length(u64),
    Chunked,
    /// Until the peer closes (a response with neither length nor chunking).
    UntilClose,
}

/// The body length of a request head. A request never runs until close.
pub fn request_body(head: &Head) -> io::Result<BodyLen> {
    if head.lists("transfer-encoding", "chunked") {
        return Ok(BodyLen::Chunked);
    }
    match head.get("content-length") {
        Some(n) => n
            .parse()
            .map(BodyLen::Length)
            .map_err(|_| invalid("bad content-length")),
        None => Ok(BodyLen::None),
    }
}

/// The body length of a response to a request with `method`.
pub fn response_body(method: &str, head: &Head) -> io::Result<BodyLen> {
    let status: u16 = head
        .start
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    if method == "HEAD" || (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok(BodyLen::None);
    }
    if head.lists("transfer-encoding", "chunked") {
        return Ok(BodyLen::Chunked);
    }
    match head.get("content-length") {
        Some(n) => n
            .parse()
            .map(BodyLen::Length)
            .map_err(|_| invalid("bad content-length")),
        None => Ok(BodyLen::UntilClose),
    }
}

/// Read a whole (request) body into memory, decoding chunking, refusing
/// more than `limit` bytes.
pub fn read_body(reader: &mut impl BufRead, len: BodyLen, limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    match len {
        BodyLen::None => {}
        BodyLen::Length(n) => {
            if n > limit as u64 {
                return Err(invalid("body too large"));
            }
            reader.take(n).read_to_end(&mut out)?;
            if out.len() as u64 != n {
                return Err(invalid("body shorter than its content-length"));
            }
        }
        BodyLen::Chunked => {
            let mut sink = io::sink();
            copy_chunked(reader, &mut sink, Some((&mut out, limit)))?;
        }
        BodyLen::UntilClose => {
            reader.take(limit as u64 + 1).read_to_end(&mut out)?;
            if out.len() > limit {
                return Err(invalid("body too large"));
            }
        }
    }
    Ok(out)
}

/// Copy a body from `reader` to `writer` as it is framed on the wire
/// (chunked bodies stay chunked).
pub fn copy_body(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    len: BodyLen,
) -> io::Result<()> {
    match len {
        BodyLen::None => Ok(()),
        BodyLen::Length(n) => {
            let copied = io::copy(&mut reader.take(n), writer)?;
            if copied == n {
                Ok(())
            } else {
                Err(invalid("body shorter than its content-length"))
            }
        }
        BodyLen::Chunked => copy_chunked(reader, writer, None),
        BodyLen::UntilClose => io::copy(reader, writer).map(|_| ()),
    }
}

/// Pass a chunked body through verbatim; with `decoded`, also collect its
/// payload (bounded).
fn copy_chunked(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    mut decoded: Option<(&mut Vec<u8>, usize)>,
) -> io::Result<()> {
    loop {
        let mut size_line = Vec::new();
        reader.take(4096).read_until(b'\n', &mut size_line)?;
        if !size_line.ends_with(b"\n") {
            return Err(invalid("bad chunk size line"));
        }
        writer.write_all(&size_line)?;
        let text = String::from_utf8_lossy(&size_line);
        let hex = text.trim().split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(hex, 16).map_err(|_| invalid("bad chunk size"))?;
        if size == 0 {
            // Trailers, then the final empty line.
            loop {
                let mut line = Vec::new();
                reader.take(MAX_HEAD as u64).read_until(b'\n', &mut line)?;
                if !line.ends_with(b"\n") {
                    return Err(invalid("stream ended inside chunk trailers"));
                }
                writer.write_all(&line)?;
                if line == b"\r\n" || line == b"\n" {
                    return Ok(());
                }
            }
        }
        let mut chunk = Vec::new();
        reader.take(size).read_to_end(&mut chunk)?;
        if chunk.len() as u64 != size {
            return Err(invalid("stream ended inside a chunk"));
        }
        if let Some((out, limit)) = decoded.as_mut() {
            if out.len() + chunk.len() > *limit {
                return Err(invalid("body too large"));
            }
            out.extend_from_slice(&chunk);
        }
        writer.write_all(&chunk)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        if &crlf != b"\r\n" {
            return Err(invalid("chunk not followed by CRLF"));
        }
        writer.write_all(&crlf)?;
    }
}

/// Serialize a head with `\r\n` line ends.
#[must_use]
pub fn render_head(start: &str, headers: &[(String, String)]) -> Vec<u8> {
    let mut out = String::with_capacity(256);
    out.push_str(start);
    out.push_str("\r\n");
    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn heads_bodies_and_chunking_round_trip() {
        let wire = b"\r\nPOST /graphql HTTP/1.1\r\nHost: api.github.com\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n2;x=y\r\nef\r\n0\r\nX-T: 1\r\n\r\nNEXT";
        let mut reader = BufReader::new(&wire[..]);
        let head = read_head(&mut reader).unwrap().unwrap();
        assert_eq!(head.start, "POST /graphql HTTP/1.1");
        assert_eq!(head.get("host"), Some("api.github.com"));
        assert_eq!(request_body(&head).unwrap(), BodyLen::Chunked);
        assert_eq!(
            read_body(&mut reader, BodyLen::Chunked, 100).unwrap(),
            b"abcdef"
        );
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "NEXT");
        assert!(
            read_body(
                &mut BufReader::new(&b"6\r\nabcdef\r\n0\r\n\r\n"[..]),
                BodyLen::Chunked,
                3
            )
            .is_err()
        );
    }

    #[test]
    fn response_lengths_follow_rfc_9112() {
        let head = |start: &str, headers: &[(&str, &str)]| Head {
            start: start.into(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        };
        assert_eq!(
            response_body("GET", &head("HTTP/1.1 204 No Content", &[])).unwrap(),
            BodyLen::None
        );
        assert_eq!(
            response_body("HEAD", &head("HTTP/1.1 200 OK", &[("content-length", "9")])).unwrap(),
            BodyLen::None
        );
        assert_eq!(
            response_body("GET", &head("HTTP/1.1 200 OK", &[("Content-Length", "9")])).unwrap(),
            BodyLen::Length(9)
        );
        assert_eq!(
            response_body("GET", &head("HTTP/1.1 200 OK", &[])).unwrap(),
            BodyLen::UntilClose
        );
        let mut out = Vec::new();
        copy_body(
            &mut BufReader::new(&b"3\r\nabc\r\n0\r\n\r\n"[..]),
            &mut out,
            BodyLen::Chunked,
        )
        .unwrap();
        assert_eq!(out, b"3\r\nabc\r\n0\r\n\r\n");
        assert!(read_head(&mut BufReader::new(&b""[..])).unwrap().is_none());
        assert!(read_head(&mut BufReader::new(&b"GET / HTTP/1.1\r\nbad\r\n\r\n"[..])).is_err());
    }
}
