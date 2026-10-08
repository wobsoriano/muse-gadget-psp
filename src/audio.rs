//! The microphone, on a thread of its own because a capture blocks for as
//! long as the sound lasts.

use crate::{entropy, muse};
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use psp::sys::{self, AudioInputFrequency, ThreadAttributes};

const RATE: usize = 22_050;
const CHUNK: usize = 512;
const LONGEST_TAKE: usize = RATE * 12;
/// A shorter take is a slip of the finger.
const SHORTEST_TAKE: usize = RATE * 2 / 5;
const MIC_GAIN: i32 = 0x1000;
/// Above the screen loop, so sound never waits on drawing.
const PRIORITY: i32 = 30;

/// Set by the screen loop while the talk button is down.
pub static TALKING: AtomicBool = AtomicBool::new(false);
/// Whether a take is being recorded, for the avatar.
pub static RECORDING: AtomicBool = AtomicBool::new(false);

/// The microphone writes here and nowhere else. On a real PSP its first
/// capture into a large buffer came back as zeros, and a small one aligned
/// to 64 bytes does not.
#[repr(align(64))]
struct Room([i16; CHUNK]);

static mut ROOM: Room = Room([0; CHUNK]);

fn hear() -> &'static [i16; CHUNK] {
    unsafe {
        let room = core::ptr::addr_of_mut!(ROOM.0);
        sys::sceAudioInputBlocking(CHUNK as i32, AudioInputFrequency::Khz22_05, room as *mut c_void);
        &*room
    }
}

/// The take as a WAV file at half the recording rate, which is plenty for
/// speech and halves what goes over the air.
fn wav(take: &[i16]) -> Vec<u8> {
    // The microphone's level varies a great deal from one take to the next
    // on a real PSP, so a quiet take is brought up before it is sent.
    let peak = take.iter().map(|sample| sample.unsigned_abs() as i32).max().unwrap_or(0).max(600);
    let rate = (RATE / 2) as u32;
    let bytes = (take.len() / 2 * 2) as u32;
    let mut file = Vec::with_capacity(44 + bytes as usize);
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + bytes).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    for field in [16, 0x0001_0001, rate, rate * 2, 0x0010_0002] {
        file.extend_from_slice(&field.to_le_bytes());
    }
    file.extend_from_slice(b"data");
    file.extend_from_slice(&bytes.to_le_bytes());
    for pair in take.chunks_exact(2) {
        let level = (pair[0] as i32 + pair[1] as i32) / 2 * 24_000 / peak;
        file.extend_from_slice(&(level.clamp(-32_768, 32_767) as i16).to_le_bytes());
    }
    file
}

fn noise(samples: &[i16]) {
    let mut bytes = [0u8; CHUNK * 2];
    for (pair, sample) in bytes.chunks_mut(2).zip(samples) {
        pair.copy_from_slice(&sample.to_le_bytes());
    }
    entropy::offer(&bytes);
}

unsafe extern "C" fn run(_: usize, _: *mut c_void) -> i32 {
    crate::say!("audio: mic init {:#x}", sys::sceAudioInputInit(0, MIC_GAIN, 0));
    let mut take: Vec<i16> = Vec::with_capacity(LONGEST_TAKE);
    let mut heard = 0u32;
    loop {
        // The microphone keeps running between takes, so a take starts warm.
        let room = hear();
        heard = heard.wrapping_add(1);
        if heard % 8 == 0 {
            noise(room);
        }
        if !TALKING.load(Ordering::Relaxed) {
            continue;
        }
        RECORDING.store(true, Ordering::Relaxed);
        take.clear();
        take.extend_from_slice(room);
        while TALKING.load(Ordering::Relaxed) && take.len() + CHUNK <= LONGEST_TAKE {
            take.extend_from_slice(hear());
        }
        let loudest = take.iter().map(|sample| sample.unsigned_abs()).max().unwrap_or(0);
        crate::say!("audio: took {} ms, loudest {} of 32768", take.len() * 1000 / RATE, loudest);
        if take.len() >= SHORTEST_TAKE {
            muse::ask(wav(&take));
        }
        RECORDING.store(false, Ordering::Relaxed);
    }
}

pub fn start() {
    unsafe {
        let thread = sys::sceKernelCreateThread(
            b"audio\0".as_ptr(),
            run,
            PRIORITY,
            64 * 1024,
            ThreadAttributes::USER,
            core::ptr::null_mut(),
        );
        sys::sceKernelStartThread(thread, 0, core::ptr::null_mut());
    }
}
