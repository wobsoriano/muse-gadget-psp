//! The client's half of a Muse Noise session: the Noise_XX_25519_AESGCM_SHA256
//! handshake as initiator, then sealed frames that carry requests to the VM
//! and its answers back. Ported from the Muse Gadget SDK's noise package.

use crate::proto::{self, Malformed};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use alloc::{borrow::Cow, string::String, vec::Vec};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};
use zeroize::Zeroize;

#[cfg(test)]
mod vectors;

const PROTOCOL: &[u8] = b"Noise_XX_25519_AESGCM_SHA256";
const KEY: usize = 32;
const TAG: usize = 16;
const SMALLEST_MESSAGE_2: usize = KEY + (KEY + TAG) + TAG;
const LARGEST_HANDSHAKE_MESSAGE: usize = 65535;
/// Past this a nonce no longer fits the JavaScript number the server counts in.
const LAST_NONCE: u64 = (1 << 53) - 1;

pub(crate) const CHUNK_PAYLOAD: usize = 65489;
const MOST_CHUNKS: usize = 256;
const MOST_ASSEMBLIES: usize = 4;
const LARGEST_ASSEMBLY: usize = 2 * 1024 * 1024;
const ASSEMBLY_LIFE_MS: u64 = 60_000;

/// The session can no longer be trusted or continued.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Broken;

impl From<Malformed> for Broken {
    fn from(_: Malformed) -> Self {
        Broken
    }
}

enum Cipher {
    /// Before the first key of a handshake, bytes pass through unchanged.
    Clear,
    Keyed { aead: Aes256Gcm, nonce: u64 },
}

impl Cipher {
    fn keyed(key: &[u8; KEY]) -> Cipher {
        Cipher::Keyed { aead: Aes256Gcm::new(key.into()), nonce: 0 }
    }

    fn next_nonce(nonce: &mut u64) -> Result<[u8; 12], Broken> {
        if *nonce >= LAST_NONCE {
            return Err(Broken);
        }
        let mut iv = [0u8; 12];
        iv[4..].copy_from_slice(&nonce.to_be_bytes());
        *nonce += 1;
        Ok(iv)
    }

    /// Encrypts `buffer[from..]` where it lies and appends the tag.
    fn seal(&mut self, ad: &[u8], buffer: &mut Vec<u8>, from: usize) -> Result<(), Broken> {
        let Cipher::Keyed { aead, nonce } = self else { return Ok(()) };
        let iv = Self::next_nonce(nonce)?;
        let tag = aead.encrypt_in_place_detached(Nonce::from_slice(&iv), ad, &mut buffer[from..]).map_err(|_| Broken)?;
        buffer.extend_from_slice(&tag);
        Ok(())
    }

    /// Decrypts `data` where it lies and returns how much of it is plaintext.
    fn open(&mut self, ad: &[u8], data: &mut [u8]) -> Result<usize, Broken> {
        let Cipher::Keyed { aead, nonce } = self else { return Ok(data.len()) };
        let iv = Self::next_nonce(nonce)?;
        let plain_len = data.len().checked_sub(TAG).ok_or(Broken)?;
        let (plain, tag) = data.split_at_mut(plain_len);
        aead.decrypt_in_place_detached(Nonce::from_slice(&iv), ad, plain, Tag::from_slice(tag)).map_err(|_| Broken)?;
        Ok(plain_len)
    }
}

fn hmac(key: &[u8; KEY], parts: &[&[u8]]) -> [u8; KEY] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// HKDF with two outputs, as Noise defines it.
fn derive(chaining: &[u8; KEY], input: &[u8]) -> ([u8; KEY], [u8; KEY]) {
    let mut temp = hmac(chaining, &[input]);
    let first = hmac(&temp, &[&[1]]);
    let second = hmac(&temp, &[&first, &[2]]);
    temp.zeroize();
    (first, second)
}

/// A handshake that has sent its first message and waits for the answer.
pub(crate) struct Handshake {
    ephemeral: [u8; KEY],
    fixed: [u8; KEY],
    fixed_public: [u8; KEY],
    hash: [u8; KEY],
    chaining: [u8; KEY],
    cipher: Cipher,
}

impl Drop for Handshake {
    fn drop(&mut self) {
        self.ephemeral.zeroize();
        self.fixed.zeroize();
        self.hash.zeroize();
        self.chaining.zeroize();
    }
}

impl Handshake {
    /// Makes both key pairs and the first message, which is 32 bytes. This is
    /// the slow part on a small CPU, so it is done before the socket opens.
    pub(crate) fn begin() -> Result<(Handshake, [u8; KEY]), Broken> {
        let (mut ephemeral, mut fixed) = ([0u8; KEY], [0u8; KEY]);
        getrandom::getrandom(&mut ephemeral).map_err(|_| Broken)?;
        getrandom::getrandom(&mut fixed).map_err(|_| Broken)?;
        Ok(Self::with_keys(ephemeral, fixed))
    }

    fn with_keys(ephemeral: [u8; KEY], fixed: [u8; KEY]) -> (Handshake, [u8; KEY]) {
        let mut hash = [0u8; KEY];
        hash[..PROTOCOL.len()].copy_from_slice(PROTOCOL);
        let mut handshake = Handshake {
            ephemeral,
            fixed,
            fixed_public: x25519(fixed, X25519_BASEPOINT_BYTES),
            hash,
            chaining: hash,
            cipher: Cipher::Clear,
        };
        let ephemeral_public = x25519(ephemeral, X25519_BASEPOINT_BYTES);
        handshake.mix_hash(&[]);
        handshake.mix_hash(&ephemeral_public);
        handshake.mix_hash(&[]);
        (handshake, ephemeral_public)
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.hash = Sha256::new().chain_update(self.hash).chain_update(data).finalize().into();
    }

    fn mix_shared(&mut self, mut private: [u8; KEY], public: &[u8]) -> Result<(), Broken> {
        let public: [u8; KEY] = public.try_into().map_err(|_| Broken)?;
        let mut shared = x25519(private, public);
        private.zeroize();
        // Every low order point gives all zeros, which would hand the key to anyone.
        let usable = shared != [0; KEY];
        let (chaining, mut key) = derive(&self.chaining, &shared);
        shared.zeroize();
        if usable {
            self.chaining = chaining;
            self.cipher = Cipher::keyed(&key);
        }
        key.zeroize();
        usable.then_some(()).ok_or(Broken)
    }

    fn seal_and_hash(&mut self, buffer: &mut Vec<u8>, from: usize) -> Result<(), Broken> {
        self.cipher.seal(&self.hash, buffer, from)?;
        self.mix_hash(&buffer[from..]);
        Ok(())
    }

    fn open_and_hash(&mut self, data: &mut [u8]) -> Result<usize, Broken> {
        let before = self.hash;
        self.mix_hash(data);
        self.cipher.open(&before, data)
    }

    /// Reads the server's message and returns the third one, 64 bytes, with
    /// the two directions of the session it opens.
    pub(crate) fn finish(mut self, message: &[u8]) -> Result<(Vec<u8>, Sender, Receiver), Broken> {
        if !(SMALLEST_MESSAGE_2..=LARGEST_HANDSHAKE_MESSAGE).contains(&message.len()) {
            return Err(Broken);
        }
        let (their_ephemeral, rest) = message.split_at(KEY);
        let mut rest = rest.to_vec();
        let (their_fixed, payload) = rest.split_at_mut(KEY + TAG);
        self.mix_hash(their_ephemeral);
        self.mix_shared(self.ephemeral, their_ephemeral)?;
        let fixed_len = self.open_and_hash(their_fixed)?;
        self.mix_shared(self.ephemeral, &their_fixed[..fixed_len])?;
        self.open_and_hash(payload)?;

        // The bearer already said who this is at the upgrade, so the payload
        // is empty and only its tag goes out.
        let mut reply = self.fixed_public.to_vec();
        self.seal_and_hash(&mut reply, 0)?;
        self.mix_shared(self.fixed, their_ephemeral)?;
        self.seal_and_hash(&mut reply, KEY + TAG)?;

        let (mut sending, mut receiving) = derive(&self.chaining, &[]);
        let sender = Sender { cipher: Cipher::keyed(&sending), next_stream: 1, message: Vec::new(), request: Vec::new(), chunk_id: 0 };
        let receiver = Receiver { cipher: Cipher::keyed(&receiving), pending: Vec::new() };
        sending.zeroize();
        receiving.zeroize();
        Ok((reply, sender, receiver))
    }
}

/// One frame of a stream, on its way to the VM.
pub(crate) enum Part<'a> {
    /// Opens the stream. Every request this client makes is a POST.
    Request { path: &'a str, headers: &'a [(&'a str, &'a str)], body: &'a [u8], end: bool },
    Body { data: &'a [u8], end: bool },
}

pub(crate) struct Sender {
    cipher: Cipher,
    next_stream: i64,
    message: Vec<u8>,
    request: Vec<u8>,
    chunk_id: u64,
}

impl Sender {
    /// The number of the next stream. The VM expects them in order.
    pub(crate) fn new_stream(&mut self) -> i64 {
        self.next_stream += 1;
        self.next_stream - 1
    }

    /// Encodes `part` and returns how many sealed messages carry it. Each is
    /// then made by `seal`, in order, and must be sent in that order: they
    /// take consecutive nonces.
    pub(crate) fn stage(&mut self, stream: i64, part: &Part) -> Result<usize, Broken> {
        let mut id = [0u8; 8];
        getrandom::getrandom(&mut id).map_err(|_| Broken)?;
        self.stage_as(stream, part, u64::from_be_bytes(id))
    }

    fn stage_as(&mut self, stream: i64, part: &Part, chunk_id: u64) -> Result<usize, Broken> {
        let message = &mut self.message;
        message.clear();
        let field = match *part {
            Part::Request { path, headers, body, end } => {
                proto::put_bytes(message, 1, b"POST");
                proto::put_bytes(message, 2, path.as_bytes());
                for (name, value) in headers {
                    proto::put_delimited(message, 3, proto::bytes_len(name.len()) + proto::bytes_len(value.len()));
                    proto::put_bytes(message, 1, name.as_bytes());
                    proto::put_bytes(message, 2, value.as_bytes());
                }
                if !body.is_empty() {
                    proto::put_bytes(message, 4, body);
                }
                if end {
                    proto::put_uint(message, 5, 1);
                }
                2
            }
            Part::Body { data, end } => {
                if !data.is_empty() {
                    proto::put_bytes(message, 1, data);
                }
                if end {
                    proto::put_uint(message, 2, 1);
                }
                4
            }
        };
        // A ServiceRequest: field 2 wraps the stream's number and the frame.
        // Its service is the daemon, which is zero and so left out.
        let stream_len = if stream != 0 { 1 + proto::varint_len(stream as u64) } else { 0 };
        self.request.clear();
        proto::put_delimited(&mut self.request, 2, stream_len + proto::bytes_len(self.message.len()));
        if stream != 0 {
            proto::put_uint(&mut self.request, 1, stream as u64);
        }
        proto::put_bytes(&mut self.request, field, &self.message);
        self.chunk_id = chunk_id;
        let total = self.request.len().div_ceil(CHUNK_PAYLOAD).max(1);
        if total > MOST_CHUNKS {
            return Err(Broken);
        }
        Ok(total)
    }

    fn chunk(&self, index: usize) -> &[u8] {
        let start = index * CHUNK_PAYLOAD;
        &self.request[start..self.request.len().min(start + CHUNK_PAYLOAD)]
    }

    fn total(&self) -> usize {
        self.request.len().div_ceil(CHUNK_PAYLOAD).max(1)
    }

    /// How long sealed message `index` of the staged frame is.
    pub(crate) fn sealed_len(&self, index: usize) -> usize {
        let field = |value: u64| if value != 0 { 1 + proto::varint_len(value) } else { 0 };
        let payload = self.chunk(index).len();
        field(self.chunk_id)
            + field(index as u64)
            + 1
            + proto::varint_len(self.total() as u64)
            + if payload > 0 { proto::bytes_len(payload) } else { 0 }
            + TAG
    }

    /// Appends sealed message `index` of the staged frame to `out`.
    pub(crate) fn seal(&mut self, index: usize, out: &mut Vec<u8>) -> Result<(), Broken> {
        let from = out.len();
        if self.chunk_id != 0 {
            proto::put_uint(out, 1, self.chunk_id);
        }
        if index != 0 {
            proto::put_uint(out, 2, index as u64);
        }
        proto::put_uint(out, 3, self.total() as u64);
        if !self.chunk(index).is_empty() {
            proto::put_bytes(out, 4, self.chunk(index));
        }
        self.cipher.seal(&[], out, from)
    }
}

struct Assembly {
    chunk_id: u64,
    pieces: Vec<Option<Vec<u8>>>,
    have: usize,
    bytes: usize,
    started_ms: u64,
}

pub(crate) struct Receiver {
    cipher: Cipher,
    pending: Vec<Assembly>,
}

impl Receiver {
    /// Decrypts one message where it lies. Gives back a ServiceResponse once
    /// one is whole, which for a large one takes several messages.
    pub(crate) fn open<'a>(&mut self, message: &'a mut [u8], now_ms: u64) -> Result<Option<Cow<'a, [u8]>>, Broken> {
        let plain_len = self.cipher.open(&[], message)?;
        let (mut chunk_id, mut index, mut total, mut payload) = (0u64, 0u64, 1u64, &[][..]);
        proto::fields(&message[..plain_len], |number, value| {
            match number {
                1 => chunk_id = value.varint()?,
                2 => index = value.varint()?,
                3 => total = value.varint()?,
                4 => payload = value.bytes()?,
                _ => {}
            }
            Ok(())
        })?;
        if total < 1 || total > MOST_CHUNKS as u64 || index >= total || payload.len() > CHUNK_PAYLOAD {
            return Err(Broken);
        }
        if total == 1 {
            return Ok(Some(Cow::Borrowed(payload)));
        }

        self.pending.retain(|assembly| now_ms.saturating_sub(assembly.started_ms) <= ASSEMBLY_LIFE_MS);
        let at = match self.pending.iter().position(|assembly| assembly.chunk_id == chunk_id) {
            Some(at) => at,
            None if self.pending.len() == MOST_ASSEMBLIES => return Err(Broken),
            None => {
                let mut pieces = Vec::new();
                pieces.resize_with(total as usize, || None);
                self.pending.push(Assembly { chunk_id, pieces, have: 0, bytes: 0, started_ms: now_ms });
                self.pending.len() - 1
            }
        };
        let assembly = &mut self.pending[at];
        let fits = assembly.pieces.len() == total as usize
            && assembly.pieces[index as usize].is_none()
            && assembly.bytes + payload.len() <= LARGEST_ASSEMBLY;
        if !fits {
            self.pending.swap_remove(at);
            return Err(Broken);
        }
        assembly.pieces[index as usize] = Some(payload.to_vec());
        assembly.bytes += payload.len();
        assembly.have += 1;
        if assembly.have < assembly.pieces.len() {
            return Ok(None);
        }
        let done = self.pending.swap_remove(at);
        let mut whole = Vec::with_capacity(done.bytes);
        for piece in done.pieces.iter().flatten() {
            whole.extend_from_slice(piece);
        }
        Ok(Some(Cow::Owned(whole)))
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Frame<'a> {
    pub stream: i64,
    pub kind: Kind<'a>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Kind<'a> {
    Response { status: i32, data: &'a [u8], end: bool },
    Body { data: &'a [u8], end: bool },
    Reset { reason: String },
}

/// Reads a ServiceResponse. `None` is one that holds nothing this client
/// acts on.
pub(crate) fn decode(envelope: &[u8]) -> Result<Option<Frame<'_>>, Broken> {
    let mut frame = &[][..];
    proto::fields(envelope, |number, value| {
        if number == 1 {
            frame = value.bytes()?;
        }
        Ok(())
    })?;
    if frame.is_empty() {
        return Err(Broken);
    }
    let (mut stream, mut request, mut response, mut body, mut reset) = (0u64, None, None, None, None);
    proto::fields(frame, |number, value| {
        match number {
            1 => stream = value.varint()?,
            2 => request = Some(value.bytes()?),
            3 => response = Some(value.bytes()?),
            4 => body = Some(value.bytes()?),
            5 => reset = Some(value.bytes()?),
            _ => {}
        }
        Ok(())
    })?;
    let stream = stream as i64;
    if request.is_some() {
        // Only this side opens streams.
        return Err(Broken);
    }
    let (mut data, mut end) = (&[][..], 0u64);
    let kind = if let Some(response) = response {
        let mut status = 0u64;
        proto::fields(response, |number, value| {
            match number {
                1 => status = value.varint()?,
                3 => data = value.bytes()?,
                4 => end = value.varint()?,
                _ => {}
            }
            Ok(())
        })?;
        let status = i32::try_from(status as i64).map_err(|_| Broken)?;
        Kind::Response { status, data, end: end != 0 }
    } else if let Some(body) = body {
        proto::fields(body, |number, value| {
            match number {
                1 => data = value.bytes()?,
                2 => end = value.varint()?,
                _ => {}
            }
            Ok(())
        })?;
        Kind::Body { data, end: end != 0 }
    } else if let Some(reset) = reset {
        proto::fields(reset, |number, value| {
            if number == 2 {
                data = value.bytes()?;
            }
            Ok(())
        })?;
        let printable = |&byte: &u8| if byte < 0x20 || byte == 0x7F { '?' } else { byte as char };
        Kind::Reset { reason: data.iter().take(127).map(printable).collect() }
    } else {
        return Ok(None);
    };
    Ok(Some(Frame { stream, kind }))
}

#[cfg(test)]
mod tests {
    use super::vectors as known;
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
    }

    fn session() -> (Sender, Receiver) {
        let key = |text| <[u8; KEY]>::try_from(hex(text)).unwrap();
        let (handshake, first) = Handshake::with_keys(key(known::INITIATOR_EPHEMERAL), key(known::INITIATOR_STATIC));
        assert_eq!(first.to_vec(), hex(known::MESSAGE_1));
        let (third, sender, receiver) = handshake.finish(&hex(known::MESSAGE_2)).unwrap();
        assert_eq!(third, hex(known::MESSAGE_3));
        (sender, receiver)
    }

    fn sealed(sender: &mut Sender, stream: i64, part: Part) -> Vec<u8> {
        assert_eq!(sender.stage_as(stream, &part, known::CHUNK_ID as u64), Ok(1));
        let mut out = Vec::new();
        sender.seal(0, &mut out).unwrap();
        assert_eq!(out.len(), sender.sealed_len(0));
        out
    }

    #[test]
    fn handshake_and_requests_match_the_sdk_byte_for_byte() {
        let (mut sender, _) = session();
        let stream = sender.new_stream();
        assert_eq!(stream, 1);
        let headers = [("x-request-id", "r-1"), ("x-app-id", "musegadget")];
        let request = Part::Request { path: "/chat/stream", headers: &headers, body: br#"{"message":"hi"}"#, end: false };
        assert_eq!(sealed(&mut sender, stream, request), hex(known::SEALED_REQUEST));
        assert_eq!(sealed(&mut sender, stream, Part::Body { data: b"tail", end: true }), hex(known::SEALED_BODY));
    }

    #[test]
    fn reads_what_the_sdk_server_seals() {
        let (_, mut receiver) = session();
        let mut read = |text: &str| {
            let mut message = hex(text);
            let envelope = receiver.open(&mut message, 0).unwrap().map(Cow::into_owned);
            envelope.map(|envelope| match decode(&envelope).unwrap().unwrap() {
                Frame { stream, kind: Kind::Response { status, data, end } } => (stream, status, data.to_vec(), end),
                Frame { stream, kind: Kind::Body { data, end } } => (stream, 0, data.to_vec(), end),
                Frame { stream, kind: Kind::Reset { reason } } => (stream, -1, reason.into_bytes(), false),
            })
        };
        assert_eq!(read(known::FROM_SERVER_RESPONSE), Some((7, 200, br#"{"ok":true}"#.to_vec(), true)));
        assert_eq!(read(known::FROM_SERVER_BODY), Some((7, 0, b"line\n".to_vec(), false)));
        assert_eq!(read(known::FROM_SERVER_RESET), Some((9, -1, b"gone".to_vec(), false)));
        assert_eq!(read(known::FROM_SERVER_BIG[0]), None);
        assert_eq!(read(known::FROM_SERVER_BIG[1]), None);
        let big: Vec<u8> = (0..150000).map(|i| (i % 251) as u8).collect();
        assert_eq!(read(known::FROM_SERVER_BIG[2]), Some((3, 0, big, true)));
    }

    #[test]
    fn a_large_frame_goes_out_in_chunks_the_receiver_rejoins() {
        let (mut sender, _) = session();
        let body = alloc::vec![7u8; 2 * CHUNK_PAYLOAD];
        let total = sender.stage(5, &Part::Body { data: &body, end: false }).unwrap();
        assert_eq!(total, 3);
        let mut lengths = Vec::new();
        for index in 0..total {
            let mut out = Vec::new();
            sender.seal(index, &mut out).unwrap();
            assert_eq!(out.len(), sender.sealed_len(index));
            lengths.push(out.len());
        }
        assert_eq!(lengths[1], lengths[0] + 2, "only the later chunks say where they go");
        let too_big = alloc::vec![0u8; MOST_CHUNKS * CHUNK_PAYLOAD];
        assert_eq!(sender.stage(5, &Part::Body { data: &too_big, end: false }), Err(Broken));
    }

    #[test]
    fn a_changed_byte_ends_the_session() {
        let (_, mut receiver) = session();
        let mut message = hex(known::FROM_SERVER_RESPONSE);
        message[3] ^= 1;
        assert_eq!(receiver.open(&mut message, 0), Err(Broken));
    }

    #[test]
    fn a_low_order_key_from_the_server_is_refused() {
        let key = |text| <[u8; KEY]>::try_from(hex(text)).unwrap();
        let (handshake, _) = Handshake::with_keys(key(known::INITIATOR_EPHEMERAL), key(known::INITIATOR_STATIC));
        let mut message = hex(known::MESSAGE_2);
        message[..KEY].fill(0);
        assert!(handshake.finish(&message).is_err());
        let (handshake, _) = Handshake::with_keys(key(known::INITIATOR_EPHEMERAL), key(known::INITIATOR_STATIC));
        assert!(handshake.finish(&hex(known::MESSAGE_2)[..SMALLEST_MESSAGE_2 - 1]).is_err());
    }

    #[test]
    fn rejects_envelopes_a_server_never_sends() {
        let mut request = Vec::new();
        proto::put_bytes(&mut request, 2, b"x");
        let mut envelope = Vec::new();
        proto::put_bytes(&mut envelope, 1, &request);
        assert_eq!(decode(&envelope), Err(Broken), "a request frame");
        assert_eq!(decode(&[]), Err(Broken), "an empty envelope");
        let mut unknown = Vec::new();
        proto::put_bytes(&mut unknown, 1, &[0x08, 0x03]);
        assert_eq!(decode(&unknown), Ok(None), "a frame of no known kind");
    }
}
