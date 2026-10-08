//! Muse's reply, spoken. One thread fetches the speech from OpenAI and a
//! second plays it, so playing starts while the rest is still arriving.

use crate::{clock, files, muse};
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use muse_link::{secure, speech};
use psp::sys::{self, AudioFormat, ThreadAttributes};

const KEY: &str = "state/openai_key.txt";
const VOICE: &str = "alloy";
/// The most of one answer that is kept, in samples.
const ROOM: usize = speech::RATE as usize * 33;
/// The speaker's own rate. The speech is stretched to it as it plays.
const SPEAKER_RATE: usize = 44_100;
const FRAMES: usize = 1024;
/// Enough arrived that playing will not catch up with the download.
const HEAD_START: usize = speech::RATE as usize / 4;
const FULL_VOLUME: i32 = 0x8000;
const NEXT_FREE_CHANNEL: i32 = -1;
/// The fetch sits with the connection, under the screen. The speaker sits
/// above it, so sound never waits on drawing.
const FETCH_PRIORITY: i32 = 41;
const SPEAKER_PRIORITY: i32 = 30;

pub const SILENT: u8 = 0;
pub const PREPARING: u8 = 1;
pub const SPEAKING: u8 = 2;
/// The reply could not be spoken. The screen loop clears this.
pub const FAILED: u8 = 3;
pub static STATE: AtomicU8 = AtomicU8::new(SILENT);

static TEXT: AtomicPtr<String> = AtomicPtr::new(core::ptr::null_mut());

/// The answer's samples. The fetch thread writes past `ARRIVED` and then
/// moves it, and the speaker reads only below it, so the two never touch
/// the same sample and need no lock.
static SAMPLES: AtomicPtr<i16> = AtomicPtr::new(core::ptr::null_mut());
static ARRIVED: AtomicUsize = AtomicUsize::new(0);
static COMPLETE: AtomicBool = AtomicBool::new(false);

/// Speaks `text`. Call only while silent, which the one-question-at-a-time
/// rule gives for free.
pub fn say(text: &str) {
    STATE.store(PREPARING, Ordering::Relaxed);
    let waiting = TEXT.swap(Box::into_raw(Box::new(String::from(text))), Ordering::AcqRel);
    if !waiting.is_null() {
        drop(unsafe { Box::from_raw(waiting) });
    }
}

fn fetch(text: &str) -> Result<(), String> {
    let key = files::read(KEY).and_then(|file| speech::key_from(&file)).ok_or("no OpenAI key on the Memory Stick")?;
    let settings = secure::settings(Arc::new(clock::Rtc)).map_err(|error| alloc::format!("{:?}", error))?;
    let samples = SAMPLES.load(Ordering::Relaxed);
    let mut odd: Option<u8> = None;
    let mut arrived = 0;
    let outcome = speech::speak(&mut muse::Psp, settings, &key, VOICE, text, &mut |piece| {
        for &byte in piece {
            match odd.take() {
                None => odd = Some(byte),
                Some(low) if arrived < ROOM => {
                    // A little louder than it comes, since the PSP's speaker is small.
                    let loud = i16::from_le_bytes([low, byte]) as i32 * 3 / 2;
                    unsafe { samples.add(arrived).write(loud.clamp(-32_768, 32_767) as i16) };
                    arrived += 1;
                }
                Some(_) => {}
            }
        }
        ARRIVED.store(arrived, Ordering::Release);
    });
    outcome.map_err(|error| alloc::format!("{:?} after {} samples", error, arrived))
}

unsafe extern "C" fn fetching(_: usize, _: *mut c_void) -> i32 {
    loop {
        let text = TEXT.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if text.is_null() {
            sys::sceKernelDelayThread(20_000);
            continue;
        }
        let text = Box::from_raw(text);
        let mut timer = crate::report::Timer::start();
        let outcome = fetch(&text);
        crate::say!("speech: {:?}, {} samples in {} ms", outcome, ARRIVED.load(Ordering::Relaxed), timer.lap());
        if ARRIVED.load(Ordering::Relaxed) == 0 {
            STATE.store(FAILED, Ordering::Relaxed);
        }
        COMPLETE.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn speaking(_: usize, _: *mut c_void) -> i32 {
    let channel = sys::sceAudioChReserve(NEXT_FREE_CHANNEL, FRAMES as i32, AudioFormat::Stereo);
    crate::say!("speech: speaker channel {}", channel);
    let samples = SAMPLES.load(Ordering::Relaxed);
    let mut frames = [0i16; FRAMES * 2];
    // How far into the answer the speaker is, in 1/SPEAKER_RATE of a sample.
    let mut position = 0u64;
    loop {
        // In this order, a `complete` that reads true comes with the final count.
        let complete = COMPLETE.load(Ordering::Acquire);
        let arrived = ARRIVED.load(Ordering::Acquire);
        let at = (position / SPEAKER_RATE as u64) as usize;
        let started = STATE.load(Ordering::Relaxed) == SPEAKING;
        if at + 1 >= arrived || (!started && !complete && arrived < HEAD_START) {
            if complete && STATE.load(Ordering::Relaxed) != FAILED {
                position = 0;
                ARRIVED.store(0, Ordering::Release);
                COMPLETE.store(false, Ordering::Release);
                STATE.store(SILENT, Ordering::Relaxed);
            }
            sys::sceKernelDelayThread(10_000);
            continue;
        }
        STATE.store(SPEAKING, Ordering::Relaxed);
        for frame in frames.chunks_mut(2) {
            let at = (position / SPEAKER_RATE as u64) as usize;
            let sample = if at + 1 < arrived {
                let (here, next) = (samples.add(at).read() as i32, samples.add(at + 1).read() as i32);
                let part = (position % SPEAKER_RATE as u64) as i32;
                (here + (next - here) * part / SPEAKER_RATE as i32) as i16
            } else {
                0
            };
            frame.fill(sample);
            if at + 1 < arrived {
                position += speech::RATE as u64;
            }
        }
        sys::sceAudioOutputPannedBlocking(channel, FULL_VOLUME, FULL_VOLUME, frames.as_mut_ptr() as *mut c_void);
    }
}

/// Call after `net::start`, like `muse::start`.
pub fn start() {
    let room: &'static mut [i16] = Vec::leak(alloc::vec![0i16; ROOM]);
    SAMPLES.store(room.as_mut_ptr(), Ordering::Relaxed);
    for (name, entry, priority, stack) in [
        (&b"speech\0"[..], fetching as sys::SceKernelThreadEntry, FETCH_PRIORITY, 256 * 1024),
        (&b"speaker\0"[..], speaking as sys::SceKernelThreadEntry, SPEAKER_PRIORITY, 64 * 1024),
    ] {
        unsafe {
            let attributes = ThreadAttributes::USER | ThreadAttributes::VFPU;
            let thread = sys::sceKernelCreateThread(name.as_ptr(), entry, priority, stack, attributes, core::ptr::null_mut());
            sys::sceKernelStartThread(thread, 0, core::ptr::null_mut());
        }
    }
}
