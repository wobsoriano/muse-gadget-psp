//! One WebSocket with a Noise session inside it, the transport a session
//! with a VM runs on. Every sealed message on it goes out through `send`, in
//! the order it was sealed: the VM counts them.

use crate::channel::{Channel, Fault, Security};
use crate::noise::{self, Handshake, Kind, Part};
use crate::url::Endpoint;
use crate::{http, ws, Clock, Net, Wire};
use alloc::{borrow::Cow, format, string::String, vec, vec::Vec};
use zeroize::Zeroizing;

const READ_SIZE: usize = 4096;
pub(crate) const OPEN_PATIENCE_MS: u64 = 20_000;
const PING_EVERY_MS: u64 = 15_000;
// A real PSP's uplink has stalled for whole seconds at a time and then
// recovered, so a session is not given up on quickly.
const PONG_PATIENCE_MS: u64 = 30_000;

impl From<noise::Broken> for Fault {
    fn from(_: noise::Broken) -> Self {
        Fault::Protocol
    }
}

impl From<ws::Bad> for Fault {
    fn from(_: ws::Bad) -> Self {
        Fault::Protocol
    }
}

/// The first Noise message with the keys behind it. Made before connecting,
/// and kept across attempts until a connection uses it.
pub(crate) type Hello = (Handshake, [u8; 32]);

/// The WebSocket: frames in and out over the connection.
struct Socket<W> {
    channel: Channel<W>,
    reader: ws::Reader,
    rx: Vec<u8>,
    rx_at: usize,
    rx_end: usize,
    out: Vec<u8>,
    last_rx_ms: u64,
}

impl<W: Wire> Socket<W> {
    fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), Fault> {
        self.out.clear();
        ws::begin(&mut self.out, opcode, payload.len());
        self.out.extend_from_slice(payload);
        self.channel.write_all(&self.out)
    }

    /// The next frame, waiting up to `wait_ms` for bytes when none are at
    /// hand. Pings are answered here. A message is left in `reader.message`.
    fn next(&mut self, mut wait_ms: u32, clock: &impl Clock) -> Result<Option<ws::Got>, Fault> {
        loop {
            if self.rx_at < self.rx_end {
                let mut input = &self.rx[self.rx_at..self.rx_end];
                let got = self.reader.next(&mut input)?;
                self.rx_at = self.rx_end - input.len();
                match got {
                    Some(ws::Got::Ping(len)) => {
                        let payload = self.reader.control;
                        self.send(ws::PONG, &payload[..len])?;
                        continue;
                    }
                    Some(got) => return Ok(Some(got)),
                    None => {}
                }
            }
            match self.channel.read_soon(&mut self.rx, wait_ms)? {
                None => return Ok(None),
                Some(0) => return Err(Fault::Closed),
                Some(got) => {
                    (self.rx_at, self.rx_end) = (0, got);
                    self.last_rx_ms = clock.millis();
                    // A frame that trickles in is finished on a later call.
                    wait_ms = 0;
                }
            }
        }
    }
}

pub(crate) enum Next {
    /// Nothing arrived within the wait.
    Nothing,
    /// A binary message, taken out of the reader. `recycle` gives its
    /// buffer back.
    Message(Vec<u8>),
    /// The VM closed the WebSocket.
    Closed,
    /// A frame with nothing in it to act on, such as a pong.
    Other,
}

pub(crate) struct Connection<W: Wire> {
    socket: Socket<W>,
    sender: noise::Sender,
    receiver: noise::Receiver,
    /// When the VM accepted the upgrade.
    pub opened_ms: u64,
    ping_cycle_ms: u64,
    ping_sent_ms: Option<u64>,
}

impl<W: Wire> Connection<W> {
    /// Connects to `to`, upgrades to a WebSocket with the bearer and runs the
    /// Noise handshake, making `hello` first if there is none.
    pub(crate) fn open(
        net: &mut impl Net<Wire = W>,
        clock: &impl Clock,
        security: &Security,
        to: &Endpoint,
        bearer: &str,
        hello: &mut Option<Hello>,
    ) -> Result<Self, Fault> {
        if hello.is_none() {
            *hello = Some(Handshake::begin()?);
        }
        let deadline = clock.millis() + OPEN_PATIENCE_MS;
        let mut channel = Channel::open(net, security, to)?;

        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).map_err(|_| Fault::Protocol)?;
        let key = ws::key(random);
        let authorization = Zeroizing::new(format!("Bearer {bearer}"));
        let headers = [
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", key.as_str()),
            ("Sec-WebSocket-Version", "13"),
            ("Authorization", authorization.as_str()),
        ];
        let request = Zeroizing::new(http::head("GET", to, &headers));
        channel.write_all(request.as_bytes())?;

        let mut head = vec![0u8; http::LARGEST_HEAD + READ_SIZE];
        let mut filled = 0;
        let head_end = loop {
            if let Some(end) = http::head_end(&head[..filled]) {
                break end;
            }
            if filled >= http::LARGEST_HEAD {
                return Err(Fault::Protocol);
            }
            if clock.millis() >= deadline {
                return Err(Fault::Timeout);
            }
            match channel.read_soon(&mut head[filled..filled + READ_SIZE], 250)? {
                None => {}
                // Hanging up ends the head, as it does to the reference client.
                Some(0) => break filled,
                Some(got) => filled += got,
            }
        };
        let text = core::str::from_utf8(&head[..head_end]).map_err(|_| Fault::Protocol)?;
        match http::status(text).ok_or(Fault::Protocol)? {
            101 => {}
            status => return Err(Fault::Refused(status)),
        }
        if http::header(text, "sec-websocket-accept") != Some(ws::accept(&key).as_str()) {
            return Err(Fault::Protocol);
        }

        // Whatever came after the head in the same read belongs to the socket.
        let early = &head[head_end..filled];
        let mut rx = vec![0u8; READ_SIZE.max(early.len())];
        rx[..early.len()].copy_from_slice(early);
        let now = clock.millis();
        let mut socket = Socket {
            channel,
            reader: ws::Reader::new(),
            rx,
            rx_at: 0,
            rx_end: early.len(),
            out: Vec::with_capacity(READ_SIZE),
            last_rx_ms: now,
        };
        // Taken before its first message goes out, so the keys are never
        // offered twice. A refusal at the upgrade above leaves them unused.
        let (handshake, first) = hello.take().ok_or(Fault::Protocol)?;
        socket.send(ws::BINARY, &first)?;
        loop {
            match socket.next(250, clock)? {
                Some(ws::Got::Message(ws::BINARY)) => break,
                Some(ws::Got::Message(_)) => return Err(Fault::Protocol),
                Some(ws::Got::Close) => return Err(Fault::Closed),
                _ if clock.millis() >= deadline => return Err(Fault::Timeout),
                _ => {}
            }
        }
        let (third, sender, receiver) = handshake.finish(&socket.reader.message)?;
        socket.send(ws::BINARY, &third)?;
        Ok(Connection { socket, sender, receiver, opened_ms: now, ping_cycle_ms: now, ping_sent_ms: None })
    }

    pub(crate) fn new_stream(&mut self) -> i64 {
        self.sender.new_stream()
    }

    /// The only place anything is sealed, so the order things go out in is
    /// the order of their nonces.
    pub(crate) fn send(&mut self, stream: i64, part: Part) -> Result<(), Fault> {
        let total = self.sender.stage(stream, &part)?;
        for index in 0..total {
            let out = &mut self.socket.out;
            out.clear();
            ws::begin(out, ws::BINARY, self.sender.sealed_len(index));
            self.sender.seal(index, out)?;
            self.socket.channel.write_all(out)?;
        }
        Ok(())
    }

    /// The next frame, waiting up to `wait_ms` for bytes when none are at
    /// hand. Pings are answered inside.
    pub(crate) fn next(&mut self, wait_ms: u32, clock: &impl Clock) -> Result<Next, Fault> {
        Ok(match self.socket.next(wait_ms, clock)? {
            None => Next::Nothing,
            Some(ws::Got::Message(ws::BINARY)) => Next::Message(core::mem::take(&mut self.socket.reader.message)),
            Some(ws::Got::Close) => Next::Closed,
            Some(_) => Next::Other,
        })
    }

    /// Decrypts a message where it lies. `Some` once a whole envelope is in,
    /// which for a large one takes several messages.
    pub(crate) fn open_message<'a>(&mut self, message: &'a mut [u8], now_ms: u64) -> Result<Option<Cow<'a, [u8]>>, noise::Broken> {
        self.receiver.open(message, now_ms)
    }

    pub(crate) fn recycle(&mut self, mut message: Vec<u8>) {
        if self.socket.reader.message.is_empty() {
            message.clear();
            self.socket.reader.message = message;
        }
    }

    /// A read on a dead connection only ever times out, so a ping that draws
    /// no frame of any kind is what ends the connection. Silence alone never
    /// does: the app may simply not have polled for a while, and whatever
    /// arrived meanwhile is read before this runs.
    pub(crate) fn keepalive(&mut self, now: u64) -> Result<(), Fault> {
        match self.ping_sent_ms {
            Some(sent) if self.socket.last_rx_ms >= sent => {
                self.ping_sent_ms = None;
                self.ping_cycle_ms = now;
                Ok(())
            }
            Some(sent) if now.saturating_sub(sent) <= PONG_PATIENCE_MS => Ok(()),
            Some(_) => Err(Fault::Quiet),
            None if now.saturating_sub(self.ping_cycle_ms) < PING_EVERY_MS => Ok(()),
            None => {
                self.ping_sent_ms = Some(now);
                self.socket.send(ws::PING, &[])
            }
        }
    }

    /// Says goodbye, as far as that still works.
    pub(crate) fn close(mut self) {
        let _ = self.socket.send(ws::CLOSE, &[]);
    }
}

/// The whole answer to a request on one stream: its status and its body, up
/// to a limit.
pub(crate) struct Answer {
    pub status: i32,
    pub body: Vec<u8>,
    /// Set once the answer is whole, or has failed.
    pub done: Option<Result<(), String>>,
    limit: usize,
}

impl Answer {
    pub(crate) fn new(limit: usize) -> Answer {
        Answer { status: 0, body: Vec::new(), done: None, limit }
    }

    pub(crate) fn feed(&mut self, kind: Kind) {
        if self.done.is_some() {
            return;
        }
        let (data, end) = match kind {
            Kind::Reset { reason } => return self.done = Some(Err(format!("stream reset: {reason}"))),
            Kind::Response { status, data, end } => {
                self.status = status;
                (data, end)
            }
            Kind::Body { data, end } => (data, end),
        };
        if self.body.len() + data.len() > self.limit {
            self.done = Some(Err("response too large".into()));
            return;
        }
        self.body.extend_from_slice(data);
        if end {
            self.done = Some(Ok(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_its_status_and_whole_body_within_its_limit() {
        let mut answer = Answer::new(8);
        answer.feed(Kind::Response { status: 200, data: b"{\"ok\"", end: false });
        assert_eq!(answer.done, None, "not whole yet");
        answer.feed(Kind::Body { data: b":1}", end: true });
        assert_eq!((answer.status, answer.body.as_slice(), &answer.done), (200, &b"{\"ok\":1}"[..], &Some(Ok(()))));
        answer.feed(Kind::Body { data: b"late", end: true });
        assert_eq!(answer.body, b"{\"ok\":1}", "nothing is added once it is whole");

        let mut answer = Answer::new(8);
        answer.feed(Kind::Response { status: 200, data: b"123456789", end: true });
        assert_eq!(answer.done, Some(Err("response too large".into())));
        let mut answer = Answer::new(8);
        answer.feed(Kind::Reset { reason: "gone".into() });
        assert_eq!(answer.done, Some(Err("stream reset: gone".into())));
    }
}
