//! A realtime voice call with Muse, on a second Noise connection to the same
//! VM that is opened with `app_id=hatch-web` and never registers. One media
//! stream, `POST /voice/realtime`, carries one JSON record per line each way:
//! the microphone up in 40 ms records, and Muse's voice and the call's
//! events down. `POST /voice/end-call` ends it.

mod gate;
mod record;
mod stage;
mod uplink;

use self::gate::{Gate, Turns, Verdict};
use self::record::{Down, Id};
use self::stage::{Act, Input, Limits, Stage};
use self::uplink::{Edge, Uplink};
use crate::channel::{Fault, Security};
use crate::client::{App, Event};
use crate::connection::{Answer, Connection, Hello, Next};
use crate::noise::{self, Kind, Part};
use crate::session::{log, uuid4};
use crate::url::Endpoint;
use crate::{Clock, Net, Wire};
use alloc::{boxed::Box, format, string::String, vec::Vec};
use zeroize::Zeroizing;

/// Samples a second on the wire both ways: 16-bit, mono, little endian.
pub const CALL_RATE: u32 = 24_000;

/// The app's audio, as a call sees it. Called only during `Client::poll`, on
/// the thread that polls.
pub trait CallAudio {
    /// The microphone's rate, above zero.
    fn mic_rate(&self) -> u32;
    /// The oldest microphone capture not yet taken, copied into `into`,
    /// which has room for 1024 samples. `None` when nothing new has been
    /// heard.
    fn heard(&mut self, into: &mut [i16]) -> Option<Heard>;
    /// Queues Muse's voice, mono at [`CALL_RATE`], and says how many samples
    /// fit. What did not fit is dropped.
    fn play(&mut self, pcm: &[i16]) -> usize;
    /// Drops everything queued and not yet played.
    fn flush(&mut self);
    /// Samples queued and not yet played.
    fn queued(&self) -> usize;
}

/// One capture from the microphone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heard {
    pub samples: usize,
    /// Whether the talk button was down while it was captured. The one place
    /// a call learns about the button.
    pub talking: bool,
}

/// Where a call stands, for the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CallState {
    /// Connecting and being admitted. Nothing is sent or played yet.
    Dialing,
    /// Muse is on the line. `working`: a turn of the user's is in hand.
    /// `weak`: the uplink fell behind and skipped audio in the last 5 s.
    Live { working: bool, weak: bool },
    /// Saying goodbye: the last words play, then end-call, then the media
    /// closes.
    Ending,
    /// Over. Said once.
    Ended(Ended),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ended {
    /// The app hung up.
    HungUp,
    /// Muse said goodbye.
    Goodbye,
    /// Nothing said, played or worked on for 45 s.
    Idle,
    /// The 15 minute limit.
    TooLong,
    /// The media stream ended without being asked to.
    Dropped,
    /// The VM answered the call with this status.
    Refused(u16),
    /// No answer, or no readiness, in time.
    NoAnswer,
    /// An error from the server, a reset, or a table that would overflow.
    Failed,
    /// The connection went. The server ends the call itself.
    Lost,
}

const FRAMES_PER_STEP: usize = 32;
/// The most samples one capture holds.
const LARGEST_CAPTURE: usize = 1024;
/// One 4 s audio event in base64, with room for its JSON.
const LARGEST_LINE: usize = 260 * 1024;
/// Audio that waits for its turn to be confirmed: 5 s.
const MOST_HELD: usize = CALL_RATE as usize * 5;
const WEAK_FOR_MS: u64 = 5_000;
/// How often the uplink says how it is doing, in the log.
const REPORT_EVERY_MS: u64 = 1_000;
/// Zeros kept past this much queued would only add delay.
const MOST_QUEUED_SILENCE: usize = CALL_RATE as usize;
const AUDIBLE_PEAK: u16 = 256;
const LARGEST_ANSWER: usize = 64 * 1024;

/// Where to dial. Kept until the first step uses it.
struct Dial {
    to: Endpoint,
    bearer: Zeroizing<String>,
    hello: Option<Hello>,
}

/// The media stream, `POST /voice/realtime`.
struct Media {
    stream: i64,
    /// Our upload has not ended.
    up: bool,
    /// Its response has not ended.
    down: bool,
}

/// What the downlink keeps outside the gate.
struct Downlink {
    /// The start of a line whose end has not arrived.
    line: Vec<u8>,
    /// The line arriving is too long, and is dropped up to its end.
    skipping_line: bool,
    /// Audio waiting for the turn that confirms its reply.
    waiting: Vec<i16>,
    /// Real audio has played since the last flush, so zeros now keep pauses.
    heard_real: bool,
    mid_speech: bool,
    /// The server asked for no microphone audio.
    input_held: bool,
}

#[derive(Default)]
struct Counts {
    advisory_errors: u32,
    skipped_lines: u32,
    bad_audio: u32,
    /// Frames on a stream that is not this call's.
    stray_frames: u32,
    /// Samples of Muse's voice that found no room.
    overflowed: usize,
}

pub(crate) struct Call<W: Wire> {
    dial: Option<Dial>,
    conn: Option<Connection<W>>,
    audio: Box<dyn CallAudio>,
    mic_rate: u32,
    call_id: String,
    device_id: String,
    stage: Stage,
    limits: Limits,
    /// Lines for the app's log since it was last told: stages entered, the
    /// server's events, and how the uplink is doing.
    notes: Vec<String>,
    media: Media,
    end_call: Option<(i64, Answer)>,
    gate: Gate,
    turns: Turns,
    uplink: Uplink,
    down: Downlink,
    thread_id: Option<Id>,
    /// A response.cancel still to send. Unsealed, so dropping it once the
    /// call is no longer live uses no nonce.
    cancel: Option<Id>,
    started_ms: u64,
    started_unix_ms: u64,
    weak_until_ms: u64,
    reported_ms: u64,
    /// Frames the uplink had sent at the last report.
    reported_frames: usize,
    hang_up: bool,
    told: Option<CallState>,
    counts: Counts,
}

impl<W: Wire> Call<W> {
    pub(crate) fn new(to: Endpoint, bearer: Zeroizing<String>, device_id: &str, audio: Box<dyn CallAudio>, hello: Option<Hello>) -> Result<Self, Fault> {
        let mic_rate = audio.mic_rate();
        assert!(mic_rate > 0, "a microphone has a rate");
        Ok(Call {
            dial: Some(Dial { to, bearer, hello }),
            conn: None,
            audio,
            mic_rate,
            call_id: uuid4()?,
            device_id: device_id.into(),
            stage: Stage::Dialing,
            limits: Limits::default(),
            notes: Vec::new(),
            media: Media { stream: 0, up: true, down: true },
            end_call: None,
            gate: Gate::new(),
            turns: Turns::new(),
            uplink: Uplink::new(mic_rate, CALL_RATE),
            down: Downlink { line: Vec::new(), skipping_line: false, waiting: Vec::new(), heard_real: false, mid_speech: false, input_held: false },
            thread_id: None,
            cancel: None,
            started_ms: 0,
            started_unix_ms: 0,
            weak_until_ms: 0,
            reported_ms: 0,
            reported_frames: 0,
            hang_up: false,
            told: None,
            counts: Counts::default(),
        })
    }

    /// Asks for the call to end. Safe to repeat.
    pub(crate) fn hang_up(&mut self) {
        self.hang_up = true;
    }

    pub(crate) fn is_over(&self) -> bool {
        matches!(self.stage, Stage::Over(_))
    }

    /// One slice of the call. The first dials, which blocks for seconds.
    /// Each later one takes the mic, sends, reads up to 32 frames, and keeps
    /// the stage's time. The app hears of every change in `CallState`.
    pub(crate) fn step(&mut self, net: &mut impl Net<Wire = W>, clock: &impl Clock, security: &Security, app: &mut impl App, wait_ms: u32) {
        let now = clock.millis();
        if core::mem::take(&mut self.hang_up) {
            self.feed(Input::HangUp, now);
        }
        match self.stage {
            Stage::Dialing => {
                self.tell(app, now);
                self.dial(net, clock, security, app);
            }
            Stage::Over(_) => {}
            _ => {
                if let Err(fault) = self.exchange(clock, wait_ms) {
                    log!(app, "the call's connection failed: {fault:?}");
                    self.feed(Input::Lost, clock.millis());
                }
            }
        }
        self.report(clock.millis());
        self.tell(app, clock.millis());
    }

    /// Connects, then sends the media request and the two opening records,
    /// each its own frame.
    fn dial(&mut self, net: &mut impl Net<Wire = W>, clock: &impl Clock, security: &Security, app: &mut impl App) {
        let Some(Dial { to, bearer, mut hello }) = self.dial.take() else { return };
        // end-call has to say when the call started.
        let Some(unix_seconds) = clock.unix_seconds() else {
            log!(app, "no call: the clock was never set");
            return self.feed(Input::Fatal(Ended::Failed), clock.millis());
        };
        let mut conn = match Connection::open(net, clock, security, &to, &bearer, &mut hello) {
            Ok(conn) => conn,
            Err(fault) => {
                log!(app, "the call did not connect: {fault:?}");
                let why = match fault {
                    Fault::Refused(status) => Ended::Refused(status),
                    Fault::Timeout => Ended::NoAnswer,
                    _ => Ended::Lost,
                };
                return self.feed(Input::Fatal(why), clock.millis());
            }
        };
        (self.started_ms, self.started_unix_ms) = (clock.millis(), unix_seconds * 1000);
        let media = conn.new_stream();
        self.media.stream = media;
        let path = format!("/voice/realtime?call_id={}", self.call_id);
        let headers = [("Content-Type", "application/x-ndjson; charset=utf-8")];
        let sent = conn
            .send(media, Part::Request { path: &path, headers: &headers, body: &[], end: false })
            .and_then(|()| conn.send(media, Part::Body { data: &record::admission(&self.call_id, &self.device_id), end: false }))
            .and_then(|()| conn.send(media, Part::Body { data: record::CAPABILITY_OFFER, end: false }));
        self.conn = Some(conn);
        match sent {
            Ok(()) => {
                log!(app, "call connected, waiting for Muse to answer");
                self.feed(Input::Dialed, clock.millis());
            }
            Err(fault) => {
                log!(app, "the call's admission did not go out: {fault:?}");
                self.feed(Input::Lost, clock.millis());
            }
        }
    }

    fn exchange(&mut self, clock: &impl Clock, wait_ms: u32) -> Result<(), Fault> {
        self.mic(clock.millis());
        self.send_up(clock.millis())?;
        self.read_down(clock, wait_ms)?;
        let now = clock.millis();
        if let Some(conn) = self.conn.as_mut() {
            conn.keepalive(now)?;
        }
        let busy = self.uplink.talking() || self.down.mid_speech || self.turns.any() || self.down.input_held;
        self.feed(Input::Tick { busy, queued: self.audio.queued() }, now);
        Ok(())
    }

    /// Takes everything the mic heard, so the app's ring never fills. Only a
    /// live call keeps any of it.
    fn mic(&mut self, now: u64) {
        let mut capture = [0i16; LARGEST_CAPTURE];
        let live = matches!(self.stage, Stage::Live { .. });
        let mut talked = false;
        while let Some(heard) = self.audio.heard(&mut capture) {
            if !live {
                continue;
            }
            let samples = heard.samples.min(capture.len());
            let edge = self.uplink.hear(&capture[..samples], heard.talking);
            if self.uplink.skipping() {
                self.weak_until_ms = now + WEAK_FOR_MS;
            }
            if edge == Some(Edge::Pressed) {
                self.cancel = self.gate.press().or(self.cancel.take());
                self.apply_flush();
            }
            talked |= heard.talking;
        }
        if talked {
            self.feed(Input::Activity, now);
        }
    }

    /// The cancel first, then the frames the uplink lets go now, each built
    /// and sealed here.
    fn send_up(&mut self, now: u64) -> Result<(), Fault> {
        if !matches!(self.stage, Stage::Live { .. }) {
            self.cancel = None;
            self.uplink.discard();
            return Ok(());
        }
        let Some(conn) = self.conn.as_mut() else { return Ok(()) };
        if let Some(open) = self.cancel.take() {
            conn.send(self.media.stream, Part::Body { data: &record::cancel(&open), end: false })?;
        }
        while let Some(line) = self.uplink.next_line(now) {
            conn.send(self.media.stream, Part::Body { data: line, end: false })?;
        }
        Ok(())
    }

    fn read_down(&mut self, clock: &impl Clock, mut wait_ms: u32) -> Result<(), Fault> {
        for _ in 0..FRAMES_PER_STEP {
            let Some(conn) = self.conn.as_mut() else { break };
            match conn.next(wait_ms, clock)? {
                Next::Nothing => break,
                Next::Closed => return Err(Fault::Closed),
                Next::Other => {}
                Next::Message(mut message) => {
                    let handled = self.on_message(&mut message, clock.millis());
                    if let Some(conn) = self.conn.as_mut() {
                        conn.recycle(message);
                    }
                    handled?;
                }
            }
            wait_ms = 0;
        }
        Ok(())
    }

    /// Each frame to the stream it names. Frames for any other are dropped.
    fn on_message(&mut self, message: &mut [u8], now: u64) -> Result<(), Fault> {
        let Some(conn) = self.conn.as_mut() else { return Ok(()) };
        let Some(envelope) = conn.open_message(message, now)? else { return Ok(()) };
        let Some(frame) = noise::decode(&envelope)? else { return Ok(()) };
        if frame.stream == self.media.stream {
            self.on_media(frame.kind, now);
        } else if let Some((_, answer)) = self.end_call.as_mut().filter(|(stream, _)| *stream == frame.stream) {
            let was_done = answer.done.is_some();
            answer.feed(frame.kind);
            if !was_done && answer.done.is_some() {
                self.feed(Input::Answered, now);
            }
        } else {
            self.counts.stray_frames += 1;
        }
        Ok(())
    }

    /// The media response: its head, then lines, then its end.
    fn on_media(&mut self, kind: Kind, now: u64) {
        let (data, end) = match kind {
            Kind::Reset { .. } => {
                (self.media.up, self.media.down) = (false, false);
                return self.feed(Input::MediaEnded { reset: true }, now);
            }
            Kind::Response { status, data, end } => {
                self.feed(Input::Head(status), now);
                (if (200..300).contains(&status) { data } else { &[][..] }, end)
            }
            Kind::Body { data, end } => (data, end),
        };
        self.on_lines(data, now);
        if end {
            self.media.down = false;
            self.feed(Input::MediaEnded { reset: false }, now);
        }
    }

    /// Splits the media body into lines. A line longer than any record is
    /// dropped up to its end and counted.
    fn on_lines(&mut self, mut data: &[u8], now: u64) {
        while !data.is_empty() {
            let newline = data.iter().position(|&byte| byte == b'\n');
            let piece = &data[..newline.unwrap_or(data.len())];
            data = newline.map_or(&[][..], |at| &data[at + 1..]);
            if self.down.skipping_line || self.down.line.len() + piece.len() > LARGEST_LINE {
                if !self.down.skipping_line {
                    self.counts.skipped_lines += 1;
                }
                self.down.line.clear();
                self.down.skipping_line = newline.is_none();
            } else if newline.is_none() {
                self.down.line.extend_from_slice(piece);
            } else if self.down.line.is_empty() {
                self.on_line(piece, now);
            } else {
                let mut line = core::mem::take(&mut self.down.line);
                line.extend_from_slice(piece);
                self.on_line(&line, now);
                line.clear();
                self.down.line = line;
            }
        }
    }

    fn on_line(&mut self, line: &[u8], now: u64) {
        let down = record::parse(line);
        if !matches!(down, Down::Audio { .. } | Down::BadAudio | Down::Other) {
            self.notes.push(format!("call: heard {down:?}"));
        }
        match (self.stage, down) {
            (Stage::Ringing { .. }, Down::Session { call_id, thread_id, .. }) if call_id.as_str() == self.call_id => {
                self.thread_id = thread_id;
                self.feed(Input::Ready, now);
            }
            (Stage::Ringing { .. } | Stage::Live { .. }, Down::Session { .. } | Down::BadSession) => self.feed(Input::Fatal(Ended::Failed), now),
            (Stage::Admitting { .. } | Stage::Ringing { .. }, Down::Error { .. }) | (Stage::Live { .. } | Stage::Goodbye { .. }, Down::Error { dead: true }) => {
                self.feed(Input::Fatal(Ended::Failed), now)
            }
            (_, Down::Error { .. }) => self.counts.advisory_errors += 1,
            (_, Down::Stop) => self.feed(Input::Stop, now),
            (_, Down::Hold(held)) => {
                self.down.input_held = held;
                self.uplink.hold(held);
            }
            (Stage::Live { .. }, Down::Audio { response_id, pcm }) => self.on_audio(response_id.as_ref(), &pcm, now),
            (Stage::Live { .. }, Down::BadAudio) => self.counts.bad_audio += 1,
            (Stage::Live { .. }, down) => self.on_event(&down, now),
            _ => {}
        }
    }

    /// A turn or reply event: the turn table, then the gate.
    fn on_event(&mut self, down: &Down, now: u64) {
        let (fatal, active) = match down {
            Down::SpeechStarted(_) => {
                self.down.mid_speech = true;
                (false, true)
            }
            Down::SpeechStopped(_) => {
                self.down.mid_speech = false;
                (false, false)
            }
            Down::TurnStarted { submission, item } => (self.turns.started(submission, item).is_err(), true),
            Down::TurnSettled(submission) => (false, self.turns.settled(submission)),
            Down::BadTurn => (true, false),
            Down::ResponseCreated(_) | Down::ResponseDone(_) => (false, true),
            _ => (false, false),
        };
        let was_waiting = self.gate.waiting();
        if fatal || self.gate.event(down).is_err() {
            return self.feed(Input::Fatal(Ended::Failed), now);
        }
        self.apply_flush();
        if was_waiting && !self.gate.waiting() {
            let mut waiting = core::mem::take(&mut self.down.waiting);
            if self.gate.playing() {
                self.play(&waiting, now);
            }
            waiting.clear();
            self.down.waiting = waiting;
        }
        if active {
            self.feed(Input::Activity, now);
        }
    }

    fn on_audio(&mut self, response_id: Option<&Id>, pcm: &[i16], now: u64) {
        // The server sends zeros all call long. Before real audio they only
        // keep the stream alive, and behind a second of speech they only add
        // delay, so they never reach the gate.
        let silent = pcm.iter().all(|&sample| sample == 0);
        if silent && (!self.down.heard_real || self.audio.queued() > MOST_QUEUED_SILENCE) {
            return;
        }
        let Ok(verdict) = self.gate.audio(response_id) else { return self.feed(Input::Fatal(Ended::Failed), now) };
        // What this audio makes stale goes before it is queued: it can be
        // the first of a newer reply.
        self.apply_flush();
        match verdict {
            Verdict::Drop => {}
            Verdict::Hold if self.down.waiting.len() + pcm.len() > MOST_HELD => {
                self.counts.overflowed += self.down.waiting.len() + pcm.len();
                self.down.waiting.clear();
                self.gate.overflow();
            }
            Verdict::Hold => self.down.waiting.extend_from_slice(pcm),
            Verdict::Play => self.play(pcm, now),
        }
    }

    fn play(&mut self, pcm: &[i16], now: u64) {
        if pcm.is_empty() {
            return;
        }
        let fit = self.audio.play(pcm);
        self.counts.overflowed += pcm.len().saturating_sub(fit);
        self.down.heard_real |= pcm.iter().any(|&sample| sample != 0);
        if pcm.iter().any(|&sample| sample.unsigned_abs() >= AUDIBLE_PEAK) {
            self.feed(Input::Activity, now);
        }
    }

    /// Carries out what the gate asked to drop.
    fn apply_flush(&mut self) {
        let flush = self.gate.take_flush();
        if flush.playing {
            self.audio.flush();
            self.down.waiting.clear();
            self.down.heard_real = false;
        } else if flush.waiting {
            self.down.waiting.clear();
        }
    }

    /// Moves the stage on and does what it says. A send that fails here is
    /// the connection going.
    fn feed(&mut self, input: Input, now: u64) {
        let (next, act) = self.stage.on(input, now, &self.limits);
        self.enter(next);
        if self.act(act, now).is_err() {
            let (next, _) = self.stage.on(Input::Lost, now, &self.limits);
            self.enter(next);
            self.conn = None;
        }
    }

    fn enter(&mut self, next: Stage) {
        if core::mem::discriminant(&next) != core::mem::discriminant(&self.stage) {
            self.notes.push(format!("call: {next:?}"));
        }
        self.stage = next;
    }

    /// Once a second while live, how the uplink is doing, unless it has
    /// nothing to do.
    fn report(&mut self, now_ms: u64) {
        if !matches!(self.stage, Stage::Live { .. }) || now_ms.saturating_sub(self.reported_ms) < REPORT_EVERY_MS {
            return;
        }
        let frames = self.uplink.frames_sent();
        let behind_ms = self.uplink.behind_ms();
        if frames != self.reported_frames || behind_ms > 0 {
            self.notes.push(format!("call: {} frames up, {behind_ms} ms behind, {} ms skipped in all", frames - self.reported_frames, self.skipped_ms()));
        }
        (self.reported_ms, self.reported_frames) = (now_ms, frames);
    }

    fn skipped_ms(&self) -> u64 {
        self.uplink.skipped() as u64 * 1000 / u64::from(self.mic_rate)
    }

    fn act(&mut self, act: Act, now: u64) -> Result<(), Fault> {
        let Some(conn) = self.conn.as_mut() else { return Ok(()) };
        match act {
            Act::Nothing | Act::Close => {}
            Act::EndCall => {
                let lasted_ms = now.saturating_sub(self.started_ms);
                let body = record::end_call_body(&self.call_id, self.started_unix_ms, lasted_ms, self.thread_id.as_ref());
                let stream = conn.new_stream();
                self.end_call = Some((stream, Answer::new(LARGEST_ANSWER)));
                let headers = [("Content-Type", "application/json")];
                conn.send(stream, Part::Request { path: "/voice/end-call", headers: &headers, body: &body, end: true })?;
            }
            Act::HalfClose => {
                self.media.up = false;
                conn.send(self.media.stream, Part::Body { data: &[], end: true })?;
            }
            Act::Reset => {
                if self.media.up || self.media.down {
                    let _ = conn.send(self.media.stream, Part::Reset { reason: "call ended" });
                }
                if let Some((stream, _)) = self.end_call.as_ref().filter(|(_, answer)| answer.done.is_none()) {
                    let _ = conn.send(*stream, Part::Reset { reason: "call ended" });
                }
            }
        }
        if matches!(act, Act::Reset | Act::Close) {
            if let Some(conn) = self.conn.take() {
                conn.close();
            }
        }
        Ok(())
    }

    /// The app's view, derived from the stage.
    fn state(&self, now_ms: u64) -> CallState {
        match self.stage {
            Stage::Dialing | Stage::Admitting { .. } | Stage::Ringing { .. } => CallState::Dialing,
            Stage::Live { .. } => CallState::Live { working: self.turns.any() || self.down.mid_speech, weak: now_ms < self.weak_until_ms },
            Stage::Goodbye { .. } | Stage::Ending { .. } | Stage::Closing { .. } => CallState::Ending,
            Stage::Over(why) => CallState::Ended(why),
        }
    }

    fn tell(&mut self, app: &mut impl App, now_ms: u64) {
        for note in self.notes.drain(..) {
            log!(app, "{note}");
        }
        let state = self.state(now_ms);
        if self.told == Some(state) {
            return;
        }
        self.told = Some(state);
        if let CallState::Ended(_) = state {
            let end_call = match &self.end_call {
                None => "never sent",
                Some((_, answer)) if answer.done.is_none() => "not answered",
                Some((_, answer)) if acknowledged(answer) => "acknowledged",
                Some(_) => "not acknowledged",
            };
            let counts = &self.counts;
            log!(
                app,
                "call over, end-call {end_call}: {} advisory errors, {} lines too long, {} bad audio, {} stray frames, {} samples with no room, {} ms of mic skipped",
                counts.advisory_errors,
                counts.skipped_lines,
                counts.bad_audio,
                counts.stray_frames,
                counts.overflowed,
                self.skipped_ms()
            );
        }
        app.event(Event::Call(state));
    }
}

/// end-call's answer says the call is over. A refusal is final either way.
fn acknowledged(answer: &Answer) -> bool {
    answer.done == Some(Ok(())) && (200..300).contains(&answer.status) && record::ended_ok(&answer.body)
}

/// The same numbers every run, for tests over many random cases.
#[cfg(test)]
struct Dice(u64);

#[cfg(test)]
impl Dice {
    fn below(&mut self, most: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % most as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{secure, ws, Lost, Stream};
    use alloc::{collections::VecDeque, rc::Rc, string::ToString, sync::Arc};
    use core::cell::{Cell, RefCell};
    use rustls::pki_types::UnixTime;
    use rustls::time_provider::TimeProvider;
    use serde_json::{json, Value};

    const LIVE: Stage = Stage::Live { since_ms: 0, quiet_since_ms: 0 };
    const QUIET: CallState = CallState::Live { working: false, weak: false };
    const MIC_RATE: u32 = 22_050;

    /// A network with nothing on it.
    struct Unplugged;

    impl Stream for Unplugged {
        fn read(&mut self, _: &mut [u8]) -> Result<usize, Lost> {
            Err(Lost)
        }
        fn write_all(&mut self, _: &[u8]) -> Result<(), Lost> {
            Err(Lost)
        }
    }

    impl Wire for Unplugged {
        fn readable(&mut self, _: u32) -> Result<bool, Lost> {
            Err(Lost)
        }
    }

    impl Net for Unplugged {
        type Wire = Unplugged;
        fn open(&mut self, _: &str, _: u16) -> Result<Unplugged, Lost> {
            Err(Lost)
        }
    }

    #[derive(Debug)]
    struct NoDate;

    impl TimeProvider for NoDate {
        fn current_time(&self) -> Option<UnixTime> {
            None
        }
    }

    struct Wall {
        ms: Cell<u64>,
        unix_seconds: Option<u64>,
    }

    impl Clock for Wall {
        fn millis(&self) -> u64 {
            self.ms.get()
        }
        fn unix_seconds(&self) -> Option<u64> {
            self.unix_seconds
        }
    }

    /// The app's side of the audio, as the call left it.
    #[derive(Default)]
    struct Room {
        mic: VecDeque<Heard>,
        played: Vec<i16>,
        /// How much had been played at each flush.
        flushes: Vec<usize>,
        queued: usize,
    }

    struct Pipes(Rc<RefCell<Room>>);

    impl CallAudio for Pipes {
        fn mic_rate(&self) -> u32 {
            MIC_RATE
        }
        fn heard(&mut self, into: &mut [i16]) -> Option<Heard> {
            let heard = self.0.borrow_mut().mic.pop_front()?;
            into[..heard.samples].fill(1000);
            Some(heard)
        }
        fn play(&mut self, pcm: &[i16]) -> usize {
            self.0.borrow_mut().played.extend_from_slice(pcm);
            pcm.len()
        }
        fn flush(&mut self) {
            let mut room = self.0.borrow_mut();
            let played = room.played.len();
            room.flushes.push(played);
        }
        fn queued(&self) -> usize {
            self.0.borrow().queued
        }
    }

    /// Every state the app was told, in order.
    #[derive(Default)]
    struct Told(Vec<CallState>);

    impl App for Told {
        fn event(&mut self, event: Event<'_>) {
            if let Event::Call(state) = event {
                self.0.push(state);
            }
        }
        fn invoke(&mut self, _: &str, _: &str) -> Result<String, String> {
            Err("no commands".into())
        }
    }

    struct Rig {
        call: Call<Unplugged>,
        room: Rc<RefCell<Room>>,
        clock: Wall,
        told: Told,
    }

    impl Rig {
        /// A call that has already dialed and stands at `stage`.
        fn at(stage: Stage) -> Rig {
            let mut rig = Rig::dialing(Some(1_793_293_200));
            rig.call.dial = None;
            rig.call.stage = stage;
            rig
        }

        fn dialing(unix_seconds: Option<u64>) -> Rig {
            let room = Rc::<RefCell<Room>>::default();
            let to = Endpoint::parse("wss://vm.example/v1/noise?vm_id=vm&app_id=hatch-web").unwrap();
            let call = Call::new(to, Zeroizing::new("bearer".into()), "homelink-1", Box::new(Pipes(room.clone())), None).unwrap();
            Rig { call, room, clock: Wall { ms: Cell::new(1000), unix_seconds }, told: Told::default() }
        }

        fn step(&mut self) {
            let security = Security::Tls(secure::settings(Arc::new(NoDate)).unwrap());
            self.call.step(&mut Unplugged, &self.clock, &security, &mut self.told, 0);
        }

        /// Lines from Muse, all in one frame of the media body.
        fn hear(&mut self, events: &[Value]) {
            let mut body = Vec::new();
            for event in events {
                body.extend_from_slice(event.to_string().as_bytes());
                body.push(b'\n');
            }
            self.call.on_media(Kind::Body { data: &body, end: false }, self.clock.ms.get());
        }

        fn played(&self) -> Vec<i16> {
            self.room.borrow().played.clone()
        }
    }

    fn audio(response_id: Option<&str>, sample: i16, samples: usize) -> Value {
        let mut frames = Vec::new();
        ws::base64(&sample.to_le_bytes().repeat(samples), &mut frames);
        let mut event = json!({"type": "continuous_audio_video.delta", "audio_delta": {"audio_frames": String::from_utf8(frames).unwrap()}});
        if let Some(id) = response_id {
            event["response_id"] = id.into();
        }
        event
    }

    fn speech(item: &str) -> Value {
        json!({"type": "input_audio_buffer.speech_started", "item_id": item})
    }

    fn turn(submission: &str, item: &str) -> Value {
        json!({"type": "hatch.voice.turn_started", "submission_message_id": submission, "item_id": item})
    }

    fn samples(sample: i16, count: usize) -> Vec<i16> {
        alloc::vec![sample; count]
    }

    #[test]
    fn zeros_play_only_after_real_audio_and_never_behind_a_second_of_it() {
        let mut rig = Rig::at(LIVE);
        rig.hear(&[audio(None, 0, 480)]);
        assert!(rig.played().is_empty(), "a keepalive before any real audio");
        rig.hear(&[audio(None, 500, 480), audio(None, 0, 480)]);
        assert_eq!(rig.played(), [samples(500, 480), samples(0, 480)].concat(), "zeros after real audio keep a pause");
        rig.room.borrow_mut().queued = 24_001;
        rig.hear(&[audio(None, 0, 480), audio(None, 500, 10)]);
        assert_eq!(rig.played().len(), 970, "zeros behind a second of speech are dropped, speech is not");

        rig.room.borrow_mut().queued = 0;
        rig.hear(&[speech("i1"), turn("m1", "i1"), audio(Some("r1"), 0, 480)]);
        assert_eq!(rig.room.borrow().flushes, [970]);
        assert_eq!(rig.played().len(), 970, "after a flush, zeros wait for real audio again, even a confirmed reply's");
        rig.hear(&[audio(Some("r1"), 700, 10), audio(Some("r1"), 0, 20)]);
        assert_eq!(rig.played()[970..], [samples(700, 10), samples(0, 20)].concat());
    }

    #[test]
    fn a_newer_reply_flushes_the_older_before_its_own_first_audio_plays() {
        let mut rig = Rig::at(LIVE);
        rig.hear(&[
            speech("i1"),
            json!({"type": "response.created", "response": {"id": "r1"}}),
            audio(Some("r1"), 1111, 100),
            turn("m1", "i1"),
            audio(Some("r1"), 1111, 100),
            audio(Some("r2"), 2222, 100),
            audio(Some("r1"), 1111, 100),
        ]);
        assert_eq!(rig.played(), [samples(1111, 200), samples(2222, 100)].concat(), "the held prefix plays once confirmed, and r1 stops for r2");
        assert_eq!(rig.room.borrow().flushes, [0, 200], "r1 is flushed before r2 is queued, so all of r2 is heard");
    }

    #[test]
    fn a_press_flushes_what_plays_and_cancels_the_open_reply() {
        let mut rig = Rig::at(LIVE);
        rig.hear(&[json!({"type": "response.created", "response": {"id": "r1"}}), audio(Some("r1"), 1111, 100)]);
        rig.room.borrow_mut().mic.extend([Heard { samples: 512, talking: true }]);
        rig.step();
        rig.hear(&[audio(Some("r1"), 1111, 100)]);
        assert_eq!(rig.played(), samples(1111, 100));
        assert_eq!(rig.room.borrow().flushes, [100]);
        assert_eq!(rig.call.cancel, Some(record::id("r1")), "the cancel waits for the next send");
    }

    #[test]
    fn the_app_hears_each_change_once_and_working_follows_the_turn() {
        let mut rig = Rig::at(LIVE);
        rig.step();
        rig.step();
        rig.hear(&[speech("i1")]);
        rig.step();
        rig.hear(&[json!({"type": "input_audio_buffer.speech_stopped", "item_id": "i1"}), turn("m1", "i1")]);
        rig.step();
        rig.hear(&[json!({"type": "hatch.voice.turn_settled", "response_message_id": "m1"})]);
        rig.step();
        rig.call.hang_up();
        rig.call.hang_up();
        rig.step();
        assert_eq!(rig.told.0, [QUIET, CallState::Live { working: true, weak: false }, QUIET, CallState::Ending]);
    }

    #[test]
    fn readiness_must_be_this_calls_and_come_once() {
        let mut rig = Rig::at(Stage::Ringing { since_ms: 0 });
        rig.hear(&[json!({"type": "hatch.voice.session", "call_id": "another", "session_id": "s"})]);
        assert!(matches!(rig.call.stage, Stage::Ending { why: Ended::Failed, .. }), "another call's readiness");

        let mut rig = Rig::at(Stage::Ringing { since_ms: 0 });
        let ready = json!({"type": "hatch.voice.session", "call_id": rig.call.call_id, "session_id": "s", "thread_id": "t"});
        rig.hear(&[json!({"type": "session.created"}), ready.clone()]);
        rig.step();
        assert_eq!(rig.told.0, [QUIET]);
        assert_eq!(rig.call.thread_id, Some(record::id("t")), "kept for end-call");
        rig.hear(&[ready]);
        rig.step();
        assert_eq!(rig.told.0, [QUIET, CallState::Ending], "a second readiness ends the call");
    }

    #[test]
    fn a_line_comes_together_across_frames_and_one_too_long_is_skipped() {
        let mut rig = Rig::at(LIVE);
        let line = audio(None, 300, 50).to_string();
        let (first, rest) = line.as_bytes().split_at(10);
        rig.call.on_media(Kind::Body { data: first, end: false }, 0);
        rig.call.on_media(Kind::Body { data: rest, end: false }, 0);
        assert!(rig.played().is_empty(), "not whole yet");
        rig.call.on_media(Kind::Body { data: b"\n", end: false }, 0);
        assert_eq!(rig.played(), samples(300, 50));

        let long = alloc::vec![b' '; LARGEST_LINE];
        rig.call.on_media(Kind::Body { data: &long, end: false }, 0);
        rig.call.on_media(Kind::Body { data: b"  \n", end: false }, 0);
        rig.hear(&[audio(None, 300, 50)]);
        assert_eq!(rig.played(), samples(300, 100), "the line after it is read");
        assert_eq!(rig.call.counts.skipped_lines, 1);
    }

    #[test]
    fn the_mic_goes_up_only_while_live_and_a_backlog_is_skipped_and_shown() {
        let mut rig = Rig::at(Stage::Ringing { since_ms: 0 });
        rig.room.borrow_mut().mic.extend([Heard { samples: 512, talking: true }; 8]);
        rig.step();
        assert!(rig.room.borrow().mic.is_empty(), "taken before readiness, so the ring never fills");
        assert_eq!(rig.call.uplink.next_line(u64::MAX), None, "and nothing goes up");

        rig.call.stage = LIVE;
        let chunks_in = |seconds: usize| seconds * MIC_RATE as usize / 512;
        rig.room.borrow_mut().mic.extend(core::iter::repeat_n(Heard { samples: 512, talking: true }, chunks_in(7)));
        rig.step();
        assert_eq!(rig.call.skipped_ms(), 0, "seven seconds of talk wait, with no network to take them");
        rig.room.borrow_mut().mic.extend(core::iter::repeat_n(Heard { samples: 512, talking: true }, chunks_in(2)));
        rig.step();
        assert!((500..=1200).contains(&rig.call.skipped_ms()), "{} ms skipped past eight seconds behind", rig.call.skipped_ms());
        rig.clock.ms.set(rig.clock.ms.get() + WEAK_FOR_MS);
        rig.step();
        assert_eq!(rig.told.0[1..], [QUIET, CallState::Live { working: false, weak: true }, QUIET]);
    }

    #[test]
    fn a_call_ends_once_without_dialing_when_it_cannot_say_when_it_started() {
        let mut rig = Rig::dialing(None);
        rig.step();
        rig.step();
        assert_eq!(rig.told.0, [CallState::Dialing, CallState::Ended(Ended::Failed)]);

        let mut rig = Rig::dialing(Some(1_793_293_200));
        rig.call.hang_up();
        rig.step();
        assert_eq!(rig.told.0, [CallState::Ended(Ended::HungUp)], "hung up before it dialed");

        let mut rig = Rig::dialing(Some(1_793_293_200));
        rig.step();
        assert_eq!(rig.told.0, [CallState::Dialing, CallState::Ended(Ended::Lost)], "no network");
    }
}
