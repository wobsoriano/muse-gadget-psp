//! WebSocket framing for a client (RFC 6455), without the socket: frames are
//! written into a buffer and read out of whatever bytes arrive.

use alloc::{string::String, vec::Vec};
use sha1::{Digest, Sha1};

pub(crate) const TEXT: u8 = 0x1;
pub(crate) const BINARY: u8 = 0x2;
pub(crate) const CLOSE: u8 = 0x8;
pub(crate) const PING: u8 = 0x9;
pub(crate) const PONG: u8 = 0xA;

/// One sealed Noise message is at most 64 KB, so nothing honest comes near this.
pub(crate) const LARGEST_MESSAGE: usize = 128 * 1024;
const LARGEST_CONTROL: usize = 125;
const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64(data: &[u8], out: &mut Vec<u8>) {
    for group in data.chunks(3) {
        let bits = group.iter().fold(0u32, |bits, &byte| bits << 8 | u32::from(byte)) << (8 * (3 - group.len()));
        for index in 0..4 {
            out.push(if index <= group.len() { ALPHABET[(bits >> (18 - 6 * index)) as usize & 63] } else { b'=' });
        }
    }
}

fn base64_text(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    base64(data, &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// The Sec-WebSocket-Key for 16 random bytes.
pub(crate) fn key(random: [u8; 16]) -> String {
    base64_text(&random)
}

/// The Sec-WebSocket-Accept a server must answer `key` with.
pub(crate) fn accept(key: &str) -> String {
    base64_text(&Sha1::new().chain_update(key).chain_update(GUID).finalize())
}

/// Starts a final frame of `len` payload bytes, which the caller appends.
/// The mask is zero, which leaves the payload as it is: masking guards
/// plaintext proxies against browsers and adds nothing under TLS.
pub(crate) fn begin(out: &mut Vec<u8>, opcode: u8, len: usize) {
    out.push(0x80 | opcode);
    match len {
        0..=125 => out.push(0x80 | len as u8),
        126..=65535 => {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(&[0; 4]);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bad;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Got {
    /// A whole text or binary message is in `Reader::message`.
    Message(u8),
    /// A ping to answer with the first this many bytes of `Reader::control`.
    Ping(usize),
    Pong,
    Close,
}

struct Frame {
    opcode: u8,
    last: bool,
    mask: [u8; 4],
    left: usize,
    done: usize,
}

/// Takes the bytes of a connection in any pieces and gives back messages.
pub(crate) struct Reader {
    head: [u8; 14],
    head_len: usize,
    frame: Option<Frame>,
    /// The kind of the message being put together, 0 between messages.
    kind: u8,
    pub message: Vec<u8>,
    pub control: [u8; LARGEST_CONTROL],
}

impl Reader {
    pub(crate) fn new() -> Reader {
        Reader { head: [0; 14], head_len: 0, frame: None, kind: 0, message: Vec::new(), control: [0; LARGEST_CONTROL] }
    }

    /// Consumes from the front of `input` up to the end of one frame. `None`
    /// means all of `input` was taken and nothing is whole yet.
    pub(crate) fn next(&mut self, input: &mut &[u8]) -> Result<Option<Got>, Bad> {
        loop {
            if self.frame.is_none() && !self.read_head(input)? {
                return Ok(None);
            }
            let Some(frame) = self.frame.as_mut() else { return Ok(None) };
            let count = frame.left.min(input.len());
            let (bytes, rest) = input.split_at(count);
            *input = rest;
            let into = if frame.opcode >= CLOSE {
                let into = &mut self.control[frame.done..frame.done + count];
                into.copy_from_slice(bytes);
                into
            } else {
                let start = self.message.len();
                self.message.extend_from_slice(bytes);
                &mut self.message[start..]
            };
            if frame.mask != [0; 4] {
                for (index, byte) in into.iter_mut().enumerate() {
                    *byte ^= frame.mask[(frame.done + index) & 3];
                }
            }
            frame.left -= count;
            frame.done += count;
            if frame.left > 0 {
                return Ok(None);
            }
            let Some(frame) = self.frame.take() else { return Ok(None) };
            match frame.opcode {
                PING => return Ok(Some(Got::Ping(frame.done))),
                PONG => return Ok(Some(Got::Pong)),
                CLOSE => return Ok(Some(Got::Close)),
                _ if frame.last => return Ok(Some(Got::Message(core::mem::take(&mut self.kind)))),
                _ => {}
            }
        }
    }

    /// True once a whole frame header has been read out of `input`.
    fn read_head(&mut self, input: &mut &[u8]) -> Result<bool, Bad> {
        loop {
            if let [_, second, ..] = self.head[..self.head_len] {
                let wide = match second & 0x7F {
                    126 => 2,
                    127 => 8,
                    _ => 0,
                };
                if self.head_len == 2 + wide + if second & 0x80 != 0 { 4 } else { 0 } {
                    break;
                }
            }
            let Some((&byte, rest)) = input.split_first() else { return Ok(false) };
            *input = rest;
            self.head[self.head_len] = byte;
            self.head_len += 1;
        }
        let head = &self.head[..self.head_len];
        self.head_len = 0;
        let (opcode, last, masked) = (head[0] & 0x0F, head[0] & 0x80 != 0, head[1] & 0x80 != 0);
        let length_end = head.len() - if masked { 4 } else { 0 };
        let len = match &head[2..length_end] {
            [] => u64::from(head[1] & 0x7F),
            wide => wide.iter().fold(0u64, |len, &byte| len << 8 | u64::from(byte)),
        };
        let mut mask = [0; 4];
        if masked {
            mask.copy_from_slice(&head[length_end..]);
        }
        match opcode {
            TEXT | BINARY => {
                self.kind = opcode;
                self.message.clear();
            }
            0 if self.kind != 0 => {}
            CLOSE | PING | PONG if last && len <= LARGEST_CONTROL as u64 => {}
            _ => return Err(Bad),
        }
        if opcode < CLOSE && len > (LARGEST_MESSAGE - self.message.len()) as u64 {
            return Err(Bad);
        }
        self.frame = Some(Frame { opcode, last, mask, left: len as usize, done: 0 });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn read_all(reader: &mut Reader, mut bytes: &[u8]) -> Vec<(Got, Vec<u8>)> {
        let mut got = Vec::new();
        while let Some(item) = reader.next(&mut bytes).unwrap() {
            let data = match item {
                Got::Message(_) => reader.message.clone(),
                Got::Ping(len) => reader.control[..len].to_vec(),
                _ => Vec::new(),
            };
            got.push((item, data));
        }
        assert!(bytes.is_empty());
        got
    }

    #[test]
    fn accept_matches_the_rfc_example() {
        assert_eq!(accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert_eq!(key(*b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
    }

    #[test]
    fn base64_pads_each_tail() {
        for (plain, coded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE=")] {
            assert_eq!(base64_text(plain.as_bytes()), coded);
        }
    }

    #[test]
    fn writes_each_length_form_with_a_zero_mask() {
        let mut out = Vec::new();
        begin(&mut out, BINARY, 5);
        assert_eq!(out, [0x82, 0x85, 0, 0, 0, 0]);
        out.clear();
        begin(&mut out, BINARY, 300);
        assert_eq!(out, [0x82, 0xFE, 0x01, 0x2C, 0, 0, 0, 0]);
        out.clear();
        begin(&mut out, PING, 70000);
        assert_eq!(out, [0x89, 0xFF, 0, 0, 0, 0, 0, 1, 0x11, 0x70, 0, 0, 0, 0]);
    }

    #[test]
    fn reads_back_what_it_writes_at_each_length() {
        for len in [0usize, 125, 126, 65535, 65536] {
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut wire = Vec::new();
            begin(&mut wire, BINARY, len);
            wire.extend_from_slice(&payload);
            assert_eq!(read_all(&mut Reader::new(), &wire), [(Got::Message(BINARY), payload)], "{len} bytes");
        }
    }

    #[test]
    fn joins_fragments_around_a_ping_one_byte_at_a_time() {
        let wire = [
            &[0x01, 0x03][..], b"Hel",
            &[0x89, 0x02], b"hi",
            &[0x80, 0x02], b"lo",
            &[0x8A, 0x00],
            &[0x88, 0x00],
        ]
        .concat();
        let mut reader = Reader::new();
        let mut got = Vec::new();
        for byte in wire {
            got.extend(read_all(&mut reader, &[byte]));
        }
        assert_eq!(
            got,
            [
                (Got::Ping(2), b"hi".to_vec()),
                (Got::Message(TEXT), b"Hello".to_vec()),
                (Got::Pong, vec![]),
                (Got::Close, vec![]),
            ]
        );
    }

    #[test]
    fn unmasks_a_masked_frame() {
        // The masked "Hello" of RFC 6455 section 5.7.
        let wire = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        assert_eq!(read_all(&mut Reader::new(), &wire), [(Got::Message(TEXT), b"Hello".to_vec())]);
    }

    #[test]
    fn rejects_frames_no_server_should_send() {
        let bad: [&[u8]; 5] = [
            &[0x80, 0x00],                         // a continuation of nothing
            &[0x09, 0x00],                         // a ping in pieces
            &[0x89, 0x7E, 0x00, 0x7E],             // a ping over 125 bytes
            &[0x83, 0x00],                         // an opcode that does not exist
            &[0x82, 0x7F, 0, 0, 0, 0, 0, 0x02, 0, 1], // over the largest message
        ];
        for wire in bad {
            let mut input = wire;
            assert_eq!(Reader::new().next(&mut input), Err(Bad), "{wire:02x?}");
        }
    }
}
