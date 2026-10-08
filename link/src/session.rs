//! One session with a Muse VM: the WebSocket, the Noise handshake inside it,
//! registration, commands, and one chat turn at a time. Ported from the Muse
//! Gadget SDK's link_client.py onto one thread: each step reads for a short
//! while and does the keepalive and the chat turn in between.

use crate::channel::{Channel, Fault, Security};
use crate::client::{App, Event};
use crate::noise::{self, Handshake, Kind, Part};
use crate::turn::{text, truthy, Invoke, Line, Turn};
use crate::url::Endpoint;
use crate::{http, ws, Clock, Net, Wire};
use alloc::{format, string::String, string::ToString, vec, vec::Vec};
use core::fmt::Write;
use serde::de::IgnoredAny;
use serde_json::{json, Value};
use zeroize::Zeroizing;

const APP_ID: &str = "musegadget";
const READ_SIZE: usize = 4096;
/// The most frames one step handles before giving the caller its turn.
const FRAMES_PER_STEP: usize = 32;
const BODY_PIECE: usize = 16 * 1024;
const PIECES_PER_STEP: usize = 4;
pub(crate) const LARGEST_WAV: usize = 600 * 1024;
/// Parsed JSON takes many times its text in memory, and the PSP has 20 MB.
/// The reference allows 4 MB here. Commands and their parameters are small.
const LARGEST_CONTROL_MESSAGE: usize = 256 * 1024;
const LARGEST_ACK: usize = 64 * 1024;
const LARGEST_EVENT_LINE: usize = 256 * 1024;

const OPEN_PATIENCE_MS: u64 = 20_000;
const ACK_PATIENCE_MS: u64 = 60_000;
const FIRST_REPLY_PATIENCE_MS: u64 = 300_000;
const LONGEST_TURN_MS: u64 = 900_000;
/// A whole reply followed by this much silence is almost always the answer.
const SETTLED_AFTER_MS: u64 = 350;
const DONE_AFTER_MS: u64 = 3_000;
const PING_EVERY_MS: u64 = 15_000;
const PONG_PATIENCE_MS: u64 = 10_000;

macro_rules! log {
    ($app:expr, $($format:tt)*) => {
        $app.event(Event::Log(format_args!($($format)*)))
    };
}
pub(crate) use log;

/// A question on its way to Muse.
pub(crate) enum Ask {
    Text(String),
    Voice(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum End {
    /// Muse removed this device.
    Unpaired,
    Failed(Fault),
}

impl From<Fault> for End {
    fn from(fault: Fault) -> Self {
        End::Failed(fault)
    }
}

impl From<noise::Broken> for Fault {
    fn from(_: noise::Broken) -> Self {
        Fault::Protocol
    }
}

impl From<noise::Broken> for End {
    fn from(_: noise::Broken) -> Self {
        End::Failed(Fault::Protocol)
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

fn uuid4() -> Result<String, Fault> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| Fault::Protocol)?;
    bytes[6] = bytes[6] & 0x0F | 0x40;
    bytes[8] = bytes[8] & 0x3F | 0x80;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if [4, 6, 8, 10].contains(&index) {
            text.push('-');
        }
        let _ = write!(text, "{byte:02x}");
    }
    Ok(text)
}

/// The stream chat events arrive on. Opened with the first question and kept
/// for the session.
struct Subscription {
    stream: i64,
    /// The start of a line whose end has not arrived.
    line: Vec<u8>,
    closed: Option<String>,
}

/// A voice note going out as the JSON body of a chat request, the recording
/// base64 encoded inside it the way Meta's voice gadgets send it. The body is
/// made a piece at a time so that it never exists whole.
struct Upload {
    wav: Vec<u8>,
    encoded: usize,
    body: Vec<u8>,
    tail_added: bool,
}

impl Upload {
    fn new(wav: Vec<u8>, device_id: &str) -> Upload {
        let mut body = Vec::with_capacity(2 * BODY_PIECE);
        body.extend_from_slice(br#"{"message":"","output_modality":"text","device_id":"#);
        body.extend_from_slice(Value::from(device_id).to_string().as_bytes());
        body.extend_from_slice(br#","items":[{"type":"file","mime_type":"audio/wav","filename":"voice_note.wav","data_base64":""#);
        Upload { wav, encoded: 0, body, tail_added: false }
    }

    /// The next piece of the body and whether it is the last.
    fn piece(&mut self) -> (&[u8], bool) {
        while self.body.len() < BODY_PIECE && !self.tail_added {
            // A multiple of three, so only the recording's end is padded.
            let take = (self.wav.len() - self.encoded).min(BODY_PIECE / 4 * 3);
            if take == 0 {
                self.body.extend_from_slice(br#""}]}"#);
                self.tail_added = true;
            } else {
                ws::base64(&self.wav[self.encoded..self.encoded + take], &mut self.body);
                self.encoded += take;
            }
        }
        let len = self.body.len().min(BODY_PIECE);
        (&self.body[..len], self.tail_added && len == self.body.len())
    }

    fn sent(&mut self, len: usize) {
        self.body.drain(..len);
    }
}

enum Phase {
    /// A voice note's body is still going out.
    Sending(Upload),
    /// Sent, and waiting for Muse to take it and name the message.
    AwaitingAck { sent_ms: u64 },
    /// Taken. The reply is arriving on the subscription.
    Replying { started_ms: u64 },
}

/// The one chat turn in flight.
struct Chat {
    phase: Phase,
    voice: bool,
    stream: i64,
    turn: Turn,
    ack_status: i32,
    ack_body: Vec<u8>,
    /// Set once the answer to the chat request is whole, or has failed.
    ack: Option<Result<(), String>>,
    /// The reply as last shown to the app.
    shown: String,
    draft: String,
    settled: bool,
}

impl Chat {
    fn on_ack(&mut self, kind: Kind) {
        if self.ack.is_some() {
            return;
        }
        let (data, end) = match kind {
            Kind::Reset { reason } => return self.ack = Some(Err(format!("stream reset: {reason}"))),
            Kind::Response { status, data, end } => {
                self.ack_status = status;
                (data, end)
            }
            Kind::Body { data, end } => (data, end),
        };
        if self.ack_body.len() + data.len() > LARGEST_ACK {
            self.ack = Some(Err("response too large".into()));
            return;
        }
        self.ack_body.extend_from_slice(data);
        if end {
            self.ack = Some(Ok(()));
        }
    }
}

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

pub(crate) struct Session<W: Wire> {
    socket: Socket<W>,
    sender: noise::Sender,
    receiver: noise::Receiver,
    device_id: String,
    control: i64,
    control_rx: Vec<u8>,
    register_id: String,
    pub registered_at_ms: Option<u64>,
    opened_ms: u64,
    subscription: Option<Subscription>,
    chat: Option<Chat>,
    ping_cycle_ms: u64,
    ping_sent_ms: Option<u64>,
}

impl<W: Wire> Session<W> {
    /// Connects, runs the handshake and asks to register. The VM's answer to
    /// that arrives in a later `step`, which sets `registered_at_ms`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        net: &mut impl Net<Wire = W>,
        clock: &impl Clock,
        security: &Security,
        to: &Endpoint,
        bearer: &str,
        device_id: &str,
        register: &Value,
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

        let mut session = Session {
            socket,
            sender,
            receiver,
            device_id: device_id.into(),
            control: 0,
            control_rx: Vec::new(),
            register_id: uuid4()?,
            registered_at_ms: None,
            opened_ms: now,
            subscription: None,
            chat: None,
            ping_cycle_ms: now,
            ping_sent_ms: None,
        };
        session.control = session.sender.new_stream();
        session.send(session.control, Part::Request { path: "/link-control", headers: &[], body: &[], end: false })?;
        let register = json!({"type": "req", "id": session.register_id, "method": "link.register", "params": register});
        session.send_control(register.to_string().as_bytes())?;
        Ok(session)
    }

    pub(crate) fn asking(&self) -> bool {
        self.chat.is_some()
    }

    /// Every sealed message goes through here, in the order it was sealed:
    /// the VM counts them.
    fn send(&mut self, stream: i64, part: Part) -> Result<(), Fault> {
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

    /// A control message is JSON behind its length as four bytes, low first.
    fn send_control(&mut self, json: &[u8]) -> Result<(), Fault> {
        let mut framed = Vec::with_capacity(4 + json.len());
        framed.extend_from_slice(&(json.len() as u32).to_le_bytes());
        framed.extend_from_slice(json);
        self.send(self.control, Part::Body { data: &framed, end: false })
    }

    /// One slice of the session: reads for up to `wait_ms`, then the
    /// keepalive and the chat turn. `ask` is taken once it can be sent.
    pub(crate) fn step(&mut self, clock: &impl Clock, app: &mut impl App, ask: &mut Option<Ask>, wait_ms: u32) -> Result<(), End> {
        let was_registered = self.registered_at_ms.is_some();
        self.read_control(clock.millis(), app)?;
        let uploading = matches!(self.chat, Some(Chat { phase: Phase::Sending(_), .. }));
        let mut wait_ms = if uploading { 0 } else { wait_ms };
        for _ in 0..FRAMES_PER_STEP {
            // The caller hears of the registration before anything that
            // follows it, such as a command, is acted on.
            if self.registered_at_ms.is_some() != was_registered {
                return Ok(());
            }
            match self.socket.next(wait_ms, clock)? {
                None => break,
                Some(ws::Got::Message(ws::BINARY)) => {
                    let mut message = core::mem::take(&mut self.socket.reader.message);
                    let handled = self.on_message(&mut message, clock.millis(), app);
                    self.socket.reader.message = message;
                    handled?;
                }
                Some(ws::Got::Close) => {
                    log!(app, "the VM closed the connection");
                    return Err(Fault::Closed.into());
                }
                Some(_) => {}
            }
            wait_ms = 0;
        }
        let now = clock.millis();
        if self.registered_at_ms.is_none() && now.saturating_sub(self.opened_ms) > OPEN_PATIENCE_MS {
            log!(app, "the VM did not answer link.register");
            return Err(Fault::Timeout.into());
        }
        self.keepalive(now, app)?;
        self.chat_step(now, app, ask)
    }

    /// Fails the turn in flight and says goodbye, as far as that still works.
    pub(crate) fn close(mut self, app: &mut impl App) {
        if let Some(chat) = self.chat.take() {
            app.event(Event::Failed { text: &chat.shown, why: "session ended" });
        }
        let _ = self.socket.send(ws::CLOSE, &[]);
    }

    fn on_message(&mut self, message: &mut [u8], now: u64, app: &mut impl App) -> Result<(), End> {
        let Some(envelope) = self.receiver.open(message, now)? else { return Ok(()) };
        let Some(frame) = noise::decode(&envelope)? else { return Ok(()) };
        if frame.stream == self.control {
            return self.on_control(frame.kind, now, app);
        }
        if self.subscription.as_ref().is_some_and(|subscription| subscription.stream == frame.stream) {
            return self.on_subscription(frame.kind, now, app);
        }
        if let Some(chat) = self.chat.as_mut().filter(|chat| chat.stream == frame.stream) {
            chat.on_ack(frame.kind);
        }
        Ok(())
    }

    fn on_control(&mut self, kind: Kind, now: u64, app: &mut impl App) -> Result<(), End> {
        let (data, end) = match kind {
            Kind::Reset { reason } => {
                log!(app, "control stream reset: {reason}");
                return Err(Fault::Closed.into());
            }
            Kind::Response { status, .. } if status >= 400 => {
                log!(app, "/link-control refused: HTTP {status}");
                return Err(Fault::Refused(u16::try_from(status).unwrap_or(u16::MAX)).into());
            }
            Kind::Response { data, end, .. } | Kind::Body { data, end } => (data, end),
        };
        if self.control_rx.len() + data.len() > 4 + LARGEST_CONTROL_MESSAGE {
            return Err(Fault::TooBig.into());
        }
        self.control_rx.extend_from_slice(data);
        self.read_control(now, app)?;
        if end {
            log!(app, "control stream ended by the VM");
            return Err(Fault::Closed.into());
        }
        Ok(())
    }

    /// Acts on the whole control messages received so far. Stops after the
    /// one that registers the session, and `step` picks up from there.
    fn read_control(&mut self, now: u64, app: &mut impl App) -> Result<(), End> {
        let was_registered = self.registered_at_ms.is_some();
        while let Some(length) = self.control_rx.first_chunk::<4>().map(|length| u32::from_le_bytes(*length) as usize) {
            if length > LARGEST_CONTROL_MESSAGE {
                return Err(Fault::TooBig.into());
            }
            let Some(json) = self.control_rx.get(4..4 + length) else { break };
            // An empty message is a keepalive and parses as nothing.
            let message = serde_json::from_slice::<Value>(json).ok();
            self.control_rx.drain(..4 + length);
            if let Some(message) = message.filter(Value::is_object) {
                self.on_control_message(&message, now, app)?;
            }
            if self.registered_at_ms.is_some() != was_registered {
                break;
            }
        }
        Ok(())
    }

    fn on_control_message(&mut self, message: &Value, now: u64, app: &mut impl App) -> Result<(), End> {
        let method = message.get("method");
        if text(message, "id") == Some(self.register_id.as_str()) && !truthy(method) {
            if truthy(message.get("error")) {
                // The SDK logs this and stays connected, registered for
                // nothing. Ending the session lets the app see it and retry.
                log!(app, "link.register rejected: {}", message["error"]);
                return Err(Fault::Protocol.into());
            }
            if self.registered_at_ms.is_none() {
                self.registered_at_ms = Some(now);
                log!(app, "registered with Muse");
            }
            return Ok(());
        }
        if let Some("link.unpaired" | "node.unpaired") = text(message, "event") {
            log!(app, "Muse removed this device");
            return Err(End::Unpaired);
        }
        if method.and_then(Value::as_str) == Some("link.invoke") && truthy(message.get("id")) {
            let params = message.get("params").filter(|params| params.is_object());
            return self.run(
                Invoke {
                    id: message["id"].to_string(),
                    command: text(message, "command").unwrap_or_default().into(),
                    params: params.map_or_else(|| "{}".into(), Value::to_string),
                },
                app,
            );
        }
        Ok(())
    }

    /// Runs a command and sends its result, whichever stream asked for it.
    fn run(&mut self, invoke: Invoke, app: &mut impl App) -> Result<(), End> {
        log!(app, "invoke {}", invoke.command);
        let result = app.invoke(&invoke.command, &invoke.params);
        let mut json = String::with_capacity(96 + result.as_ref().map_or(0, String::len));
        let _ = write!(json, r#"{{"method":"link.result","id":{},"#, invoke.id);
        match result {
            Ok(payload) if serde_json::from_str::<IgnoredAny>(&payload).is_ok() => {
                json.push_str(r#""ok":true,"payload":"#);
                json.push_str(&payload);
            }
            Ok(_) => json.push_str(r#""ok":false,"error":"invalid result""#),
            Err(why) => {
                let _ = write!(json, r#""ok":false,"error":{}"#, Value::from(why));
            }
        }
        json.push('}');
        Ok(self.send_control(json.as_bytes())?)
    }

    fn on_subscription(&mut self, kind: Kind, now: u64, app: &mut impl App) -> Result<(), End> {
        let Some(subscription) = self.subscription.as_mut().filter(|subscription| subscription.closed.is_none()) else {
            return Ok(());
        };
        let (data, end) = match kind {
            Kind::Reset { reason } => return Ok(subscription.closed = Some(reason)),
            Kind::Response { status, .. } if status >= 400 => return Ok(subscription.closed = Some(format!("HTTP {status}"))),
            Kind::Response { data, end, .. } | Kind::Body { data, end } => (data, end),
        };
        let mut started = core::mem::take(&mut subscription.line);
        let mut closed = end.then(|| String::from("ended by Muse"));
        let mut rest = data;
        while let Some(at) = rest.iter().position(|&byte| byte == b'\n') {
            let (line, after) = rest.split_at(at);
            rest = &after[1..];
            if started.len() + line.len() > LARGEST_EVENT_LINE {
                closed = Some("event line too long".into());
                break;
            }
            if started.is_empty() {
                self.on_line(line, now, app)?;
            } else {
                started.extend_from_slice(line);
                self.on_line(&started, now, app)?;
                started.clear();
            }
        }
        if started.len() + rest.len() > LARGEST_EVENT_LINE {
            closed = Some("event line too long".into());
        } else {
            started.extend_from_slice(rest);
        }
        if let Some(subscription) = self.subscription.as_mut() {
            subscription.line = started;
            subscription.closed = closed;
        }
        Ok(())
    }

    fn on_line(&mut self, line: &[u8], now: u64, app: &mut impl App) -> Result<(), End> {
        match (Line::parse(line), self.chat.as_mut()) {
            // A command for a turn that started here arrives this way, where
            // one from a turn started in the Muse app is link.invoke on the
            // control stream.
            (Line::Invoke(invoke), _) => return self.run(invoke, app),
            (Line::Busy(busy), Some(chat)) => chat.turn.busy(busy, now),
            (Line::Message(message), Some(chat)) => chat.turn.add(message, now),
            (Line::Unread, Some(chat)) => chat.turn.last_event_ms = now,
            _ => {}
        }
        Ok(())
    }

    /// A read on a dead connection only ever times out, so a ping that draws
    /// no frame of any kind is what ends the session. Silence alone never
    /// does: the app may simply not have polled for a while, and whatever
    /// arrived meanwhile is read before this runs.
    fn keepalive(&mut self, now: u64, app: &mut impl App) -> Result<(), End> {
        match self.ping_sent_ms {
            Some(sent) if self.socket.last_rx_ms >= sent => {
                self.ping_sent_ms = None;
                self.ping_cycle_ms = now;
                return Ok(());
            }
            Some(sent) if now.saturating_sub(sent) <= PONG_PATIENCE_MS => return Ok(()),
            Some(_) => {}
            None if now.saturating_sub(self.ping_cycle_ms) < PING_EVERY_MS => return Ok(()),
            None => {
                self.ping_sent_ms = Some(now);
                return Ok(self.socket.send(ws::PING, &[])?);
            }
        }
        log!(app, "the connection to Muse went quiet");
        Err(Fault::Quiet.into())
    }

    fn chat_step(&mut self, now: u64, app: &mut impl App, ask: &mut Option<Ask>) -> Result<(), End> {
        let Some(mut chat) = self.chat.take() else {
            if self.registered_at_ms.is_none() {
                return Ok(());
            }
            let Some(ask) = ask.take() else { return Ok(()) };
            return match self.start_chat(ask, now) {
                Ok(chat) => Ok(self.chat = Some(chat)),
                Err(fault) => {
                    app.event(Event::Failed { text: "", why: "session ended" });
                    Err(fault.into())
                }
            };
        };
        let advanced = self.advance(&mut chat, now, app);
        match &advanced {
            Ok(Some(Ok(()))) => app.event(Event::Done(&chat.shown)),
            Ok(Some(Err(why))) => app.event(Event::Failed { text: &chat.shown, why }),
            // A turn cut off by the session ending is failed by `close`.
            Ok(None) | Err(_) => self.chat = Some(chat),
        }
        advanced.map(drop)
    }

    fn start_chat(&mut self, ask: Ask, now: u64) -> Result<Chat, Fault> {
        if self.subscription.as_ref().is_none_or(|subscription| subscription.closed.is_some()) {
            let (stream, request_id) = (self.sender.new_stream(), uuid4()?);
            let headers = [
                ("Content-Type", "application/json"),
                ("accept", "application/x-ndjson"),
                ("x-request-id", request_id.as_str()),
                ("x-app-id", APP_ID),
            ];
            self.send(stream, Part::Request { path: "/chat/subscribe", headers: &headers, body: b"{}", end: true })?;
            self.subscription = Some(Subscription { stream, line: Vec::new(), closed: None });
        }
        let (stream, request_id) = (self.sender.new_stream(), uuid4()?);
        let headers = [("x-request-id", request_id.as_str()), ("x-app-id", APP_ID), ("Content-Type", "application/json")];
        let path = "/chat/stream";
        // device_id is what has Muse take the turn as this device's, and
        // send the commands it leads to back here.
        let (phase, voice) = match ask {
            Ask::Text(message) => {
                let body = json!({"message": message, "output_modality": "text", "device_id": self.device_id}).to_string();
                self.send(stream, Part::Request { path, headers: &headers, body: body.as_bytes(), end: true })?;
                (Phase::AwaitingAck { sent_ms: now }, false)
            }
            Ask::Voice(wav) => {
                self.send(stream, Part::Request { path, headers: &headers, body: &[], end: false })?;
                (Phase::Sending(Upload::new(wav, &self.device_id)), true)
            }
        };
        Ok(Chat {
            phase,
            voice,
            stream,
            turn: Turn::new(now),
            ack_status: 0,
            ack_body: Vec::new(),
            ack: None,
            shown: String::new(),
            draft: String::new(),
            settled: false,
        })
    }

    /// Moves the turn along. `Some` is its end: done, or failed and why.
    fn advance(&mut self, chat: &mut Chat, now: u64, app: &mut impl App) -> Result<Option<Result<(), String>>, End> {
        let started_ms = match &mut chat.phase {
            Phase::Sending(upload) => {
                // The VM can refuse the stream while the body is still going out.
                if chat.ack.as_ref().is_some_and(Result::is_err) {
                    return Ok(chat.ack.take());
                }
                for _ in 0..PIECES_PER_STEP {
                    let (piece, last) = upload.piece();
                    let len = piece.len();
                    self.send(chat.stream, Part::Body { data: piece, end: last })?;
                    upload.sent(len);
                    if last {
                        chat.phase = Phase::AwaitingAck { sent_ms: now };
                        break;
                    }
                }
                return Ok(None);
            }
            Phase::AwaitingAck { sent_ms } => {
                let sent_ms = *sent_ms;
                return Ok(match chat.ack.take() {
                    Some(Err(why)) => Some(Err(why)),
                    Some(Ok(())) if !(200..300).contains(&chat.ack_status) => {
                        Some(Err(format!("Muse did not take the message: HTTP {}", chat.ack_status)))
                    }
                    Some(Ok(())) => {
                        let answer = serde_json::from_slice::<Value>(&chat.ack_body).unwrap_or_default();
                        let named = answer.get("result").filter(|result| result.is_object()).unwrap_or(&answer);
                        match text(named, "message_id") {
                            // Without it no reply can ever be matched.
                            None => Some(Err("Muse acknowledged the message without naming it".into())),
                            Some(message_id) => {
                                chat.turn.acknowledged(message_id);
                                chat.ack_body = Vec::new();
                                chat.phase = Phase::Replying { started_ms: now };
                                None
                            }
                        }
                    }
                    None if now.saturating_sub(sent_ms) > ACK_PATIENCE_MS => Some(Err("Muse did not acknowledge the message".into())),
                    None => None,
                });
            }
            Phase::Replying { started_ms } => *started_ms,
        };

        if let Some(heard) = chat.turn.take_heard().filter(|_| chat.voice) {
            app.event(Event::Heard(heard));
        }
        if chat.turn.take_change() {
            chat.turn.write_text(&mut chat.draft);
            if chat.draft != chat.shown {
                core::mem::swap(&mut chat.draft, &mut chat.shown);
                chat.settled = false;
                app.event(Event::Reply(&chat.shown));
            }
        }
        let has_text = !chat.shown.is_empty();
        let whole = has_text && chat.turn.whole();
        let quiet_ms = now.saturating_sub(chat.turn.last_event_ms);
        let elapsed_ms = now.saturating_sub(started_ms);
        if whole && !chat.settled && quiet_ms > SETTLED_AFTER_MS {
            chat.settled = true;
            app.event(Event::Settled(&chat.shown));
        }
        let closed = self.subscription.as_ref().and_then(|subscription| subscription.closed.as_deref());
        // Nothing marks the end of a turn, so it is over once every reply
        // message is done and the stream has been quiet for a moment.
        Ok(if whole {
            // A whole reply waits out the pause even if the stream has closed.
            (quiet_ms > DONE_AFTER_MS).then_some(Ok(()))
        } else if let Some(reason) = closed {
            Some(Err(format!("chat stream closed: {reason}")))
        } else if !has_text && elapsed_ms > FIRST_REPLY_PATIENCE_MS {
            Some(Err("Muse did not reply".into()))
        } else if elapsed_ms > LONGEST_TURN_MS {
            Some(if has_text { Ok(()) } else { Err("Muse did not finish replying".into()) })
        } else {
            None
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_note_body_comes_out_in_even_pieces_and_decodes_back() {
        let wav: Vec<u8> = (0..100_000u32).map(|i| (i * 7) as u8).collect();
        let mut upload = Upload::new(wav.clone(), "home\"link");
        let (mut body, mut sizes) = (Vec::new(), Vec::new());
        loop {
            let (piece, last) = upload.piece();
            let len = piece.len();
            body.extend_from_slice(piece);
            sizes.push(len);
            upload.sent(len);
            if last {
                break;
            }
        }
        assert!(sizes[..sizes.len() - 1].iter().all(|&size| size == BODY_PIECE), "{sizes:?}");
        assert_eq!(sizes.len(), body.len().div_ceil(BODY_PIECE));
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["device_id"], "home\"link");
        assert_eq!((&parsed["message"], &parsed["output_modality"]), (&json!(""), &json!("text")));
        let item = &parsed["items"][0];
        assert_eq!((&item["type"], &item["mime_type"], &item["filename"]), (&json!("file"), &json!("audio/wav"), &json!("voice_note.wav")));
        let mut expected = Vec::new();
        ws::base64(&wav, &mut expected);
        assert_eq!(item["data_base64"].as_str().unwrap().as_bytes(), expected);
    }

    #[test]
    fn uuids_are_version_four() {
        let id = uuid4().unwrap();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.iter().map(|part| part.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4') && "89ab".contains(&parts[3][..1]));
        assert_ne!(id, uuid4().unwrap());
    }
}
