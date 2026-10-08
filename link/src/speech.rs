//! A reply turned into speech by OpenAI, delivered as it arrives so it can
//! be played while the rest is still on its way.

use crate::secure::{self, Secure};
use crate::{http, Lost, Net};
use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use rustls::ClientConfig;

const HOST: &str = "api.openai.com";
/// Samples a second in what `speak` delivers: 16-bit, mono, little endian.
pub const RATE: u32 = 24_000;
const LARGEST_HEAD: usize = 16 * 1024;
/// Keeps the audio of one answer to about half a minute.
const MOST_CHARACTERS: usize = 420;

#[derive(Debug, PartialEq)]
pub enum Failed {
    Network,
    Security,
    /// OpenAI answered with this HTTP status. 401 is a bad key.
    Refused(u16),
    /// The answer was not HTTP this client understands.
    Garbled,
}

impl From<Lost> for Failed {
    fn from(_: Lost) -> Self {
        Failed::Network
    }
}

impl From<secure::Error> for Failed {
    fn from(error: secure::Error) -> Self {
        match error {
            secure::Error::Tls(_) => Failed::Security,
            _ => Failed::Network,
        }
    }
}

/// The start of `text` that is worth speaking: all of it, or a long answer
/// cut at the end of a sentence.
pub fn spoken_part(text: &str) -> &str {
    if text.len() <= MOST_CHARACTERS {
        return text;
    }
    let mut cut = MOST_CHARACTERS;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let sentence_end = text[..cut]
        .char_indices()
        .filter(|&(at, c)| at >= MOST_CHARACTERS / 2 && matches!(c, '.' | '!' | '?') && text[at + 1..].starts_with(' '))
        .last();
    &text[..sentence_end.map_or(cut, |(at, _)| at + 1)]
}

/// Where a response body stands. HTTP sends it whole, counted, or in chunks
/// that each say their own size.
#[derive(Debug, PartialEq)]
enum Body {
    UntilClose,
    Counted(usize),
    ChunkSize(usize, bool),
    ChunkSkipLine(usize),
    Chunk(usize),
    ChunkEnd(usize),
    Done,
}

impl Body {
    fn of(head: &str) -> Result<Body, Failed> {
        if http::header(head, "transfer-encoding").is_some_and(|coding| coding.eq_ignore_ascii_case("chunked")) {
            return Ok(Body::ChunkSize(0, false));
        }
        match http::header(head, "content-length") {
            Some(length) => length.parse().map(Body::Counted).map_err(|_| Failed::Garbled),
            None => Ok(Body::UntilClose),
        }
    }

    /// Passes the body bytes in `wire` to `sink`.
    fn feed(&mut self, mut wire: &[u8], sink: &mut dyn FnMut(&[u8])) -> Result<(), Failed> {
        while !wire.is_empty() {
            let used = match self {
                Body::Done => return Ok(()),
                Body::UntilClose => {
                    sink(wire);
                    wire.len()
                }
                Body::Counted(left) | Body::Chunk(left) => {
                    let take = wire.len().min(*left);
                    sink(&wire[..take]);
                    *left -= take;
                    if *left == 0 {
                        *self = if matches!(self, Body::Chunk(_)) { Body::ChunkEnd(2) } else { Body::Done };
                    }
                    take
                }
                Body::ChunkSize(size, any) => {
                    match (wire[0], (wire[0] as char).to_digit(16)) {
                        (_, Some(digit)) => {
                            *size = size.checked_mul(16).and_then(|size| size.checked_add(digit as usize)).ok_or(Failed::Garbled)?;
                            *any = true;
                        }
                        (b'\r' | b';', _) if *any => *self = Body::ChunkSkipLine(*size),
                        _ => return Err(Failed::Garbled),
                    }
                    1
                }
                Body::ChunkSkipLine(size) => {
                    if wire[0] == b'\n' {
                        *self = if *size == 0 { Body::Done } else { Body::Chunk(*size) };
                    }
                    1
                }
                Body::ChunkEnd(left) => {
                    *left -= 1;
                    if *left == 0 {
                        *self = Body::ChunkSize(0, false);
                    }
                    1
                }
            };
            wire = &wire[used..];
        }
        Ok(())
    }
}

/// Speaks `text` in `voice`. `heard` is given the audio in pieces as it
/// arrives, at [`RATE`]. A piece can end in the middle of a sample.
pub fn speak<N: Net>(
    net: &mut N,
    settings: Arc<ClientConfig>,
    key: &str,
    voice: &str,
    text: &str,
    heard: &mut dyn FnMut(&[u8]),
) -> Result<(), Failed> {
    let request = serde_json::json!({
        "model": "gpt-4o-mini-tts",
        "voice": voice,
        "input": spoken_part(text),
        "instructions": "Warm, friendly and a little playful, like a small cheerful companion. Natural pace.",
        "response_format": "pcm",
    })
    .to_string();
    let head = format!(
        "POST /v1/audio/speech HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        request.len()
    );
    let mut secure = Secure::open(net.open(HOST, 443)?, HOST, settings)?;
    secure.write_all(head.as_bytes())?;
    secure.write_all(request.as_bytes())?;

    let mut buffer: Vec<u8> = alloc::vec![0; 4096];
    let mut filled = 0;
    let (mut body, start) = loop {
        if filled == buffer.len() {
            if buffer.len() >= LARGEST_HEAD {
                return Err(Failed::Garbled);
            }
            buffer.resize(buffer.len() * 2, 0);
        }
        let got = secure.read(&mut buffer[filled..])?;
        if got == 0 {
            return Err(Failed::Network);
        }
        filled += got;
        if let Some(end) = http::head_end(&buffer[..filled]) {
            let head = core::str::from_utf8(&buffer[..end]).map_err(|_| Failed::Garbled)?;
            match http::status(head).ok_or(Failed::Garbled)? {
                200 => break (Body::of(head)?, end),
                status => return Err(Failed::Refused(status)),
            }
        }
    };
    body.feed(&buffer[start..filled], heard)?;
    while body != Body::Done {
        let got = secure.read(&mut buffer)?;
        if got == 0 {
            return if body == Body::UntilClose { Ok(()) } else { Err(Failed::Network) };
        }
        body.feed(&buffer[..got], heard)?;
    }
    Ok(())
}

/// What is on the Memory Stick or in a file is the key and maybe a line end.
pub fn key_from(file: &[u8]) -> Option<String> {
    let key = core::str::from_utf8(file).ok()?.trim();
    (!key.is_empty() && key.bytes().all(|byte| byte.is_ascii_graphic())).then(|| key.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(mut body: Body, wire: &[u8], piece: usize) -> (Vec<u8>, Body) {
        let mut out = Vec::new();
        for part in wire.chunks(piece) {
            body.feed(part, &mut |bytes| out.extend_from_slice(bytes)).unwrap();
        }
        (out, body)
    }

    #[test]
    fn a_chunked_body_comes_out_the_same_however_it_is_cut() {
        let wire = b"5\r\nhello\r\n6;ext=1\r\n world\r\n1A\r\nabcdefghijklmnopqrstuvwxyz\r\n0\r\n\r\n";
        for piece in [1, 2, 3, 7, wire.len()] {
            let (out, body) = fed(Body::ChunkSize(0, false), wire, piece);
            assert_eq!(out, b"hello worldabcdefghijklmnopqrstuvwxyz", "pieces of {piece}");
            assert_eq!(body, Body::Done);
        }
    }

    #[test]
    fn a_counted_body_stops_at_its_count() {
        let (out, body) = fed(Body::Counted(5), b"helloEXTRA", 3);
        assert_eq!((out.as_slice(), body), (&b"hello"[..], Body::Done));
    }

    #[test]
    fn a_bad_chunk_size_is_garbled() {
        let mut body = Body::ChunkSize(0, false);
        assert_eq!(body.feed(b"zz\r\n", &mut |_| {}), Err(Failed::Garbled));
        let mut body = Body::ChunkSize(0, false);
        assert_eq!(body.feed(b"\r\n", &mut |_| {}), Err(Failed::Garbled));
    }

    #[test]
    fn the_kind_of_body_comes_from_the_head() {
        assert_eq!(Body::of("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"), Ok(Body::ChunkSize(0, false)));
        assert_eq!(Body::of("HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n"), Ok(Body::Counted(12)));
        assert_eq!(Body::of("HTTP/1.1 200 OK\r\n\r\n"), Ok(Body::UntilClose));
    }

    #[test]
    fn a_long_answer_is_cut_at_a_sentence() {
        let short = "Doing great, thanks!";
        assert_eq!(spoken_part(short), short);
        let long = format!("{} End of the first part. {}", "word ".repeat(60), "more ".repeat(60));
        let part = spoken_part(&long);
        assert!(part.ends_with("first part.") && part.len() <= MOST_CHARACTERS, "{part:?}");
        let unbroken = "é".repeat(400);
        assert!(spoken_part(&unbroken).len() <= MOST_CHARACTERS);
    }

    #[test]
    fn a_key_is_one_printable_word() {
        assert_eq!(key_from(b"sk-abc123\r\n").as_deref(), Some("sk-abc123"));
        assert_eq!(key_from(b"\n"), None);
        assert_eq!(key_from(b"two words"), None);
    }
}
