//! A realtime voice call with Muse, on a second Noise connection to the same
//! VM that is opened with `app_id=hatch-web` and never registers. One media
//! stream, `POST /voice/realtime`, carries one JSON record per line each way:
//! the microphone up in 40 ms records, and Muse's voice and the call's
//! events down. `POST /voice/end-call` ends it.

mod gate;
mod record;
mod stage;
mod uplink;

/// Samples a second on the wire both ways: 16-bit, mono, little endian.
pub const CALL_RATE: u32 = 24_000;

/// The app's audio, as a call sees it. Called only during `Client::poll`, on
/// the thread that polls.
pub trait CallAudio {
    /// The microphone's rate.
    fn mic_rate(&self) -> u32;
    /// The oldest microphone capture not yet taken, copied into `into`.
    /// `None` when nothing new has been heard.
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
    /// Samples captured after it and still waiting: how far behind the call is.
    pub behind: usize,
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
