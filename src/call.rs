//! A voice call's audio, and what the screen shows of the call. The
//! microphone's chunks reach the connection thread through one ring, and
//! Muse's voice comes back through another to a speaker of its own, on the
//! PSP's 24 kHz hardware channel. Each atomic and each end of each ring has
//! one writer, named on it.

use crate::{audio, files, report};
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use muse_link::ring::Ring;
use muse_link::{CallAudio, CallState, Ended, Heard, CALL_RATE};
use psp::sys::{self, AudioOutputFrequency, ThreadAttributes};

/// Holds the word `call` while the app is in call mode. Voice notes are the
/// default, so deleting it goes back to them.
const MODE: &str = "state/mode.txt";
/// About 3 s of the microphone. A call starts skipping audio once it is 1.5 s
/// behind, so only a connection thread held up for longer fills it.
const MIC_CHUNKS: usize = 128;
/// 10.9 s of Muse's voice.
const SPEAKER_SAMPLES: usize = 1 << 18;
/// Frames the speaker plays at a time, 43 ms.
const BLOCK: usize = 1024;
const BLOCK_MICROS: u32 = (BLOCK as u64 * 1_000_000 / CALL_RATE as u64) as u32;
/// After running dry the speaker waits for this much of Muse's voice, so it
/// does not stutter, or for this long, so a short last word still plays.
const PREROLL_MS: u32 = 160;
const PREROLL: usize = (CALL_RATE * PREROLL_MS / 1000) as usize;
const AUDIBLE_PEAK: u16 = 256;
const SOUNDING_HOLD_MS: u32 = 300;
const ENDED_SHOWN_MS: u32 = 3000;
const FULL_VOLUME: i32 = 0x8000;
const IDLE_WAIT_MICROS: u32 = 20_000;
/// Level with the other speaker, above the screen loop, so sound never waits
/// on drawing.
const PRIORITY: i32 = 30;

#[derive(Clone, Copy)]
struct Chunk {
    /// Whether R was down when it was heard.
    talking: bool,
    pcm: [i16; audio::CHUNK],
}

/// The mic thread puts, the muse thread takes.
static MIC: Ring<Chunk> = Ring::new();
/// The muse thread puts and flushes, the call speaker takes.
static SPEAKER: Ring<i16> = Ring::new();

/// Main: R is down on a live call.
pub static TALK: AtomicBool = AtomicBool::new(false);

pub const NOTHING: u8 = 0;
pub const START: u8 = 1;
pub const HANG_UP: u8 = 2;
/// Main leaves one request here, and the muse thread takes it.
static ASK: AtomicU8 = AtomicU8::new(NOTHING);

/// Muse: a call's `Pipes` exist, from `Pipes::open` until the client drops them.
static LISTENING: AtomicBool = AtomicBool::new(false);
/// Call speaker: Muse's voice was audible until then, on `report::now_ms`'s clock.
static SOUNDING_UNTIL_MS: AtomicU32 = AtomicU32::new(0);

const IDLE: u8 = 0;
const DIALING: u8 = 1;
const LIVE: u8 = 2;
const ENDING: u8 = 3;
const ENDED: u8 = 4;
const NO_ANSWER: u8 = 5;
const REFUSED: u8 = 6;
const LOST: u8 = 7;
const FAILED: u8 = 8;
/// The call as the screen shows it. The muse thread writes it from the
/// call's events. Main writes it too, but only while no call is up and the
/// muse thread has not taken main's START: the dial holds the muse thread
/// for seconds before it can say it is dialing.
static SHOWN: AtomicU8 = AtomicU8::new(IDLE);
/// Muse: each written before `SHOWN` moves to the one state that reads it,
/// so the screen never pairs a state with another state's time.
static LIVE_SINCE_MS: AtomicU32 = AtomicU32::new(0);
static ENDED_AT_MS: AtomicU32 = AtomicU32::new(0);
static WORKING: AtomicBool = AtomicBool::new(false);
static WEAK: AtomicBool = AtomicBool::new(false);

/// The phone line, as the screen shows it.
#[derive(Clone, Copy, PartialEq)]
pub enum Line {
    /// No call, and none ended in the last 3 s.
    Idle,
    Dialing,
    /// `working`: Muse is on a turn of the user's. `weak`: the upload fell
    /// behind and skipped audio in the last 5 s.
    Live { since_ms: u32, working: bool, weak: bool },
    Ending,
    /// For 3 s after a call ends: how, in the screen's words.
    Ended { word: &'static str, failed: bool },
}

impl Line {
    /// No call is up, so one may start.
    pub fn idle(self) -> bool {
        matches!(self, Line::Idle | Line::Ended { .. })
    }
}

/// Main thread.
pub fn line(now_ms: u32) -> Line {
    let ended = |word, failed| {
        if now_ms.wrapping_sub(ENDED_AT_MS.load(Ordering::Relaxed)) < ENDED_SHOWN_MS {
            Line::Ended { word, failed }
        } else {
            Line::Idle
        }
    };
    match SHOWN.load(Ordering::Acquire) {
        DIALING => Line::Dialing,
        LIVE => Line::Live {
            since_ms: LIVE_SINCE_MS.load(Ordering::Relaxed),
            working: WORKING.load(Ordering::Relaxed),
            weak: WEAK.load(Ordering::Relaxed),
        },
        ENDING => Line::Ending,
        ENDED => ended("call ended", false),
        NO_ANSWER => ended("no answer", true),
        REFUSED => ended("call refused", true),
        LOST => ended("call lost", true),
        FAILED => ended("call failed", true),
        _ => Line::Idle,
    }
}

/// Main thread: Muse's voice on the call is audible, or was a moment ago.
pub fn sounding(now_ms: u32) -> bool {
    (SOUNDING_UNTIL_MS.load(Ordering::Relaxed).wrapping_sub(now_ms) as i32) > 0
}

/// Main thread, while no call is up: asks for one and shows it dialing.
pub fn dial() {
    SHOWN.store(DIALING, Ordering::Relaxed);
    ASK.store(START, Ordering::Release);
}

/// Main thread, while dialing or live: hangs up, or takes back a call the
/// muse thread has not started yet.
pub fn hang_up() {
    if ASK.compare_exchange(START, NOTHING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        SHOWN.store(IDLE, Ordering::Release);
    } else {
        ASK.store(HANG_UP, Ordering::Release);
    }
}

/// Muse thread: main's request since the last take, or `NOTHING`.
pub fn take_ask() -> u8 {
    ASK.swap(NOTHING, Ordering::AcqRel)
}

/// Muse thread: the call moved on.
pub fn show(state: CallState) {
    let now_ms = report::now_ms();
    let shown = match state {
        CallState::Dialing => DIALING,
        CallState::Live { working, weak } => {
            WORKING.store(working, Ordering::Relaxed);
            WEAK.store(weak, Ordering::Relaxed);
            if SHOWN.load(Ordering::Relaxed) != LIVE {
                LIVE_SINCE_MS.store(now_ms, Ordering::Relaxed);
            }
            LIVE
        }
        CallState::Ending => ENDING,
        CallState::Ended(why) => {
            ENDED_AT_MS.store(now_ms, Ordering::Relaxed);
            match why {
                Ended::HungUp | Ended::Goodbye | Ended::Idle | Ended::TooLong => ENDED,
                Ended::NoAnswer => NO_ANSWER,
                Ended::Refused(_) => REFUSED,
                Ended::Lost | Ended::Dropped => LOST,
                Ended::Failed => FAILED,
            }
        }
    };
    SHOWN.store(shown, Ordering::Release);
}

/// A call is up, from its dial to its end.
pub fn on_call() -> bool {
    LISTENING.load(Ordering::Acquire)
}

/// Mic thread: hands the chunk to the call, tagged with whether R is down.
/// False when no call is up to take it.
pub fn hear(pcm: &[i16; audio::CHUNK]) -> bool {
    if !LISTENING.load(Ordering::Acquire) {
        return false;
    }
    // The mic thread is MIC's one putter. A full ring turns the chunk away.
    unsafe { MIC.put(&[Chunk { talking: TALK.load(Ordering::Relaxed), pcm: *pcm }]) };
    true
}

/// The call's ends of the two rings, for `Client::call`. The client drops
/// them when the call is over, which stops the mic's chunks and the speaker.
pub struct Pipes(());

impl Pipes {
    /// Muse thread, only while no call is up.
    pub fn open() -> Pipes {
        // The muse thread takes from MIC and puts into SPEAKER, so it may
        // drop what the last call left in both.
        unsafe {
            MIC.skip_all();
            SPEAKER.drop_queued();
        }
        LISTENING.store(true, Ordering::Release);
        Pipes(())
    }
}

impl Drop for Pipes {
    fn drop(&mut self) {
        LISTENING.store(false, Ordering::Release);
    }
}

/// Every method runs on the muse thread, inside `Client::poll`: MIC's one
/// taker and SPEAKER's one putter.
impl CallAudio for Pipes {
    fn mic_rate(&self) -> u32 {
        audio::RATE as u32
    }

    fn heard(&mut self, into: &mut [i16]) -> Option<Heard> {
        let mut chunk = [Chunk { talking: false, pcm: [0; audio::CHUNK] }];
        if unsafe { MIC.take(&mut chunk) } == 0 {
            return None;
        }
        into[..audio::CHUNK].copy_from_slice(&chunk[0].pcm);
        Some(Heard { samples: audio::CHUNK, talking: chunk[0].talking, behind: MIC.queued() * audio::CHUNK })
    }

    fn play(&mut self, pcm: &[i16]) -> usize {
        unsafe { SPEAKER.put(pcm) }
    }

    fn flush(&mut self) {
        unsafe { SPEAKER.drop_queued() }
    }

    fn queued(&self) -> usize {
        SPEAKER.queued()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Channel {
    Free,
    Ours,
    /// The PSP would not give it. Asked again for the next call.
    Refused,
}

#[derive(Clone, Copy, PartialEq)]
enum Flow {
    Playing,
    /// Nothing left to play.
    Dry,
    /// Muse's voice came back after a dry spell, at this time.
    Filling(u32),
}

impl Flow {
    fn next(self, queued: usize, now_ms: u32) -> Flow {
        match self {
            Flow::Dry if queued > 0 => Flow::Filling(now_ms).next(queued, now_ms),
            Flow::Filling(since_ms) if queued >= PREROLL || now_ms.wrapping_sub(since_ms) >= PREROLL_MS => Flow::Playing,
            flow => flow,
        }
    }
}

/// The call speaker, SPEAKER's one taker. It holds the 24 kHz channel only
/// while a call is up.
unsafe extern "C" fn speaking(_: usize, _: *mut c_void) -> i32 {
    let mut channel = Channel::Free;
    let mut flow = Flow::Dry;
    let mut mono = [0i16; BLOCK];
    let mut stereo = [0i16; BLOCK * 2];
    loop {
        let listening = LISTENING.load(Ordering::Acquire);
        channel = match (channel, listening) {
            (Channel::Free, true) => {
                let reserved = sys::sceAudioSRCChReserve(BLOCK as i32, AudioOutputFrequency::Khz24, 2);
                crate::say!("call: speaker channel {:#x}", reserved);
                flow = Flow::Dry;
                if reserved < 0 {
                    Channel::Refused
                } else {
                    Channel::Ours
                }
            }
            (Channel::Ours, false) => {
                // The channel will not let go while its last block still
                // plays, so this is tried again until it does.
                if sys::sceAudioSRCChRelease() < 0 {
                    Channel::Ours
                } else {
                    Channel::Free
                }
            }
            (Channel::Refused, false) => Channel::Free,
            (channel, _) => channel,
        };
        if !listening || channel != Channel::Ours {
            sys::sceKernelDelayThread(IDLE_WAIT_MICROS);
            continue;
        }
        let now_ms = report::now_ms();
        if TALK.load(Ordering::Relaxed) {
            // R silences Muse within a block, before the muse thread has
            // even seen the press.
            SPEAKER.take(&mut mono);
            mono.fill(0);
            flow = Flow::Dry;
        } else {
            flow = flow.next(SPEAKER.queued(), now_ms);
            let got = if flow == Flow::Playing { SPEAKER.take(&mut mono) } else { 0 };
            mono[got..].fill(0);
            if flow == Flow::Playing && got < BLOCK {
                flow = Flow::Dry;
            }
        }
        if mono.iter().any(|sample| sample.unsigned_abs() >= AUDIBLE_PEAK) {
            SOUNDING_UNTIL_MS.store(now_ms.wrapping_add(SOUNDING_HOLD_MS), Ordering::Relaxed);
        }
        for (frame, &sample) in stereo.chunks_mut(2).zip(&mono) {
            // As loud as the other speaker plays speech, since the PSP's speaker is small.
            frame.fill((sample as i32 * 3 / 2).clamp(-32_768, 32_767) as i16);
        }
        if sys::sceAudioSRCOutputBlocking(FULL_VOLUME, stereo.as_mut_ptr() as *mut c_void) < 0 {
            // A refused block returns at once, and a loop at this rank that
            // never waits would starve every thread below it.
            sys::sceKernelDelayThread(BLOCK_MICROS);
        }
    }
}

/// Call once the network is up, since without it there is never a call.
/// Leaks both rings, about 640 KiB, so no large allocation happens as a call
/// starts, and starts the call speaker.
pub fn start() {
    MIC.install(Vec::leak(alloc::vec![Chunk { talking: false, pcm: [0; audio::CHUNK] }; MIC_CHUNKS]));
    SPEAKER.install(Vec::leak(alloc::vec![0i16; SPEAKER_SAMPLES]));
    unsafe {
        let thread = sys::sceKernelCreateThread(
            b"call speaker\0".as_ptr(),
            speaking,
            PRIORITY,
            64 * 1024,
            ThreadAttributes::USER,
            core::ptr::null_mut(),
        );
        sys::sceKernelStartThread(thread, 0, core::ptr::null_mut());
    }
}

/// Main thread, at start: whether the app was left in call mode.
pub fn load_mode() -> bool {
    files::read(MODE).is_some_and(|file| file.trim_ascii() == b"call")
}

/// Main thread, on SELECT.
pub fn keep_mode(calls: bool) {
    if calls {
        files::write(MODE, b"call\n");
    } else {
        files::remove(MODE);
    }
}
