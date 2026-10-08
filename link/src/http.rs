//! HTTP/1.1 as the device API and the WebSocket upgrade speak it: one request
//! on a fresh connection, and the answer read until it is whole.

use crate::channel::{Channel, Fault};
use crate::url::Endpoint;
use crate::{Clock, Wire};
use alloc::{string::String, vec, vec::Vec};
use core::fmt::Write;

pub(crate) const LARGEST_HEAD: usize = 8 * 1024;
const LARGEST_BODY: usize = 256 * 1024;
const READ_SIZE: usize = 4096;
const PATIENCE_MS: u64 = 15_000;

/// The request line and headers, ending with the blank line.
pub(crate) fn head(method: &str, to: &Endpoint, headers: &[(&str, &str)]) -> String {
    let mut head = String::with_capacity(256);
    let _ = write!(head, "{method} {} HTTP/1.1\r\nHost: {}\r\n", to.path, to.authority());
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    head
}

/// Where the head ends and the body starts, once the blank line has arrived.
pub(crate) fn head_end(buffer: &[u8]) -> Option<usize> {
    let mut lines = buffer.split_inclusive(|&byte| byte == b'\n');
    let mut at = lines.next()?.len();
    for line in lines {
        at += line.len();
        if line == b"\n" || line == b"\r\n" {
            return Some(at);
        }
    }
    None
}

/// The status of "HTTP/x.y <3 digits>[ reason]".
pub(crate) fn status(head: &str) -> Option<u16> {
    let mut words = head.lines().next()?.strip_prefix("HTTP/")?.split_ascii_whitespace();
    let code = words.nth(1)?;
    (code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit())).then(|| code.parse().ok())?
}

/// The value of a header, by its name in lower case. The last one wins.
pub(crate) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).filter_map(|line| line.split_once(':')).filter(|(key, _)| key.trim().eq_ignore_ascii_case(name)).last().map(|(_, value)| value.trim())
}

#[derive(Debug, PartialEq)]
pub(crate) struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

fn chunked(mut rest: &[u8], closed: bool) -> Result<Option<Vec<u8>>, Fault> {
    let unfinished = if closed { Err(Fault::Closed) } else { Ok(None) };
    let mut body = Vec::new();
    loop {
        let Some(line_end) = rest.iter().position(|&byte| byte == b'\n') else { return unfinished };
        let line = core::str::from_utf8(&rest[..line_end]).map_err(|_| Fault::Protocol)?;
        let digits = line.split(';').next().unwrap_or_default().trim();
        // An empty size line counts as zero, as it does to the reference client.
        let size = if digits.is_empty() { 0 } else { usize::from_str_radix(digits, 16).map_err(|_| Fault::Protocol)? };
        if size == 0 {
            return Ok(Some(body));
        }
        if size > LARGEST_BODY - body.len() {
            return Err(Fault::TooBig);
        }
        rest = &rest[line_end + 1..];
        let Some(data) = rest.get(..size) else { return unfinished };
        body.extend_from_slice(data);
        let Some(after) = rest[size..].iter().position(|&byte| byte == b'\n') else { return unfinished };
        rest = &rest[size + after + 1..];
    }
}

/// Reads a response out of everything received so far. `None` means more is
/// still to come. `closed` is whether the server has hung up, which is how a
/// body with no stated length ends.
pub(crate) fn parse(buffer: &[u8], closed: bool) -> Result<Option<Response>, Fault> {
    let end = match head_end(buffer) {
        Some(end) => end,
        None if closed => buffer.len(),
        None if buffer.len() >= LARGEST_HEAD => return Err(Fault::Protocol),
        None => return Ok(None),
    };
    if end > LARGEST_HEAD {
        return Err(Fault::Protocol);
    }
    let head = core::str::from_utf8(&buffer[..end]).map_err(|_| Fault::Protocol)?;
    let status = status(head).ok_or(Fault::Protocol)?;
    let rest = &buffer[end..];
    let body = if header(head, "transfer-encoding").is_some_and(|coding| coding.eq_ignore_ascii_case("chunked")) {
        chunked(rest, closed)?
    } else if let Some(length) = header(head, "content-length") {
        let plain = !length.is_empty() && length.bytes().all(|byte| byte.is_ascii_digit());
        let length: usize = length.parse().ok().filter(|_| plain).ok_or(Fault::Protocol)?;
        if length > LARGEST_BODY {
            return Err(Fault::TooBig);
        }
        match rest.get(..length) {
            Some(body) => Some(body.to_vec()),
            None if closed => return Err(Fault::Closed),
            None => None,
        }
    } else if rest.len() > LARGEST_BODY {
        return Err(Fault::TooBig);
    } else {
        closed.then(|| rest.to_vec())
    };
    Ok(body.map(|body| Response { status, body }))
}

/// Sends one request and reads the whole answer.
pub(crate) fn request<W: Wire>(
    channel: &mut Channel<W>,
    clock: &impl Clock,
    method: &str,
    to: &Endpoint,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<Response, Fault> {
    let deadline = clock.millis() + PATIENCE_MS;
    let mut message = head(method, to, headers).into_bytes();
    // The blank line is taken back so the two headers every request carries
    // can follow the caller's.
    message.truncate(message.len() - 2);
    message.extend_from_slice(b"Connection: close\r\n");
    if let Some(body) = body {
        let mut length = String::new();
        let _ = write!(length, "Content-Length: {}\r\n", body.len());
        message.extend_from_slice(length.as_bytes());
    }
    message.extend_from_slice(b"\r\n");
    message.extend_from_slice(body.unwrap_or_default());
    channel.write_all(&message)?;

    let mut buffer = vec![0u8; READ_SIZE];
    let mut filled = 0;
    loop {
        if clock.millis() >= deadline {
            return Err(Fault::Timeout);
        }
        if filled == buffer.len() {
            if buffer.len() > LARGEST_HEAD + 2 * LARGEST_BODY {
                return Err(Fault::TooBig);
            }
            buffer.resize(buffer.len() * 2, 0);
        }
        let Some(got) = channel.read_soon(&mut buffer[filled..], 250)? else { continue };
        filled += got;
        if let Some(response) = parse(&buffer[..filled], got == 0)? {
            return Ok(response);
        }
        if got == 0 {
            return Err(Fault::Closed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_request_head() {
        let to = Endpoint::parse("http://127.0.0.1:8080/fetch_vms").unwrap();
        assert_eq!(
            head("GET", &to, &[("Authorization", "Bearer t"), ("X-API-Version", "1.0.0")]),
            "GET /fetch_vms HTTP/1.1\r\nHost: 127.0.0.1:8080\r\nAuthorization: Bearer t\r\nX-API-Version: 1.0.0\r\n\r\n"
        );
    }

    #[test]
    fn reads_status_and_headers() {
        let head = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept:  abc= \r\nX: 1\r\nx: 2\r\n\r\n";
        assert_eq!(status(head), Some(101));
        assert_eq!(header(head, "sec-websocket-accept"), Some("abc="));
        assert_eq!(header(head, "x"), Some("2"));
        assert_eq!(header(head, "missing"), None);
        assert_eq!(status("HTTP/1.1 403\r\n"), Some(403));
        for bad in ["", "HTTP/1.1", "HTTP/1.1 20 OK", "HTTP/1.1 2000 OK", "ICY 200 OK", "HTTP/1.1 abc", "HTTP/1.1 +20"] {
            assert_eq!(status(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_chunked_body_is_whole_at_its_last_chunk_however_it_arrives() {
        let wire = b"HTTP/1.1 200 X\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        for cut in 0..wire.len() - 4 {
            assert_eq!(parse(&wire[..cut], false), Ok(None), "cut at {cut}");
        }
        let whole = Response { status: 200, body: b"hello world".to_vec() };
        assert_eq!(parse(wire, false), Ok(Some(whole)));
        assert_eq!(parse(&wire[..60], true), Err(Fault::Closed), "hung up half way");
    }

    #[test]
    fn a_counted_body_and_a_body_ended_by_hanging_up() {
        let counted = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}extra";
        assert_eq!(parse(&counted[..counted.len() - 6], false), Ok(None));
        assert_eq!(parse(counted, false), Ok(Some(Response { status: 401, body: b"{}".to_vec() })));
        let open_ended = b"HTTP/1.0 200 OK\n\nabc";
        assert_eq!(parse(open_ended, false), Ok(None));
        assert_eq!(parse(open_ended, true), Ok(Some(Response { status: 200, body: b"abc".to_vec() })));
        assert_eq!(parse(b"HTTP/1.1 403 Forbidden\r\n", true), Ok(Some(Response { status: 403, body: Vec::new() })));
    }

    #[test]
    fn rejects_what_is_not_an_answer_or_is_too_large() {
        assert_eq!(parse(b"hello\r\n\r\n", false), Err(Fault::Protocol));
        assert_eq!(parse(&[b'a'; LARGEST_HEAD], false), Err(Fault::Protocol));
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n", false), Err(Fault::TooBig));
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\nhello", false), Err(Fault::Protocol));
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nfffffff\r\n", false), Err(Fault::TooBig));
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", false), Err(Fault::Protocol));
    }
}
