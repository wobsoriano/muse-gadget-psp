//! Random bytes for the encryption.
//!
//! The PSP has no hardware random source. This pool hashes together what the
//! app observes that an outsider could not know: noise from the microphone,
//! the clock, the Wi-Fi chip's address, and when each network event arrived.

use core::sync::atomic::{AtomicU32, Ordering};
use sha2::{Digest, Sha256};
use spin::Mutex;

struct Pool {
    state: [u8; 32],
    served: u64,
}

/// Used by the connection thread, and by the main thread before any other
/// exists. The audio thread outranks both and would spin on this lock for
/// ever if it met it held, so it goes through `offer`.
static POOL: Mutex<Pool> = Mutex::new(Pool { state: [0; 32], served: 0 });

/// Noise waiting to be mixed in, left here by a thread that must not touch
/// the pool's lock.
static OFFERED: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

fn micros() -> [u8; 8] {
    unsafe { psp::sys::sceKernelGetSystemTimeWide() }.to_le_bytes()
}

fn digest(observed: &[u8]) -> [u8; 32] {
    Sha256::new().chain_update(micros()).chain_update(observed).finalize().into()
}

fn collect() -> [u8; 32] {
    let mut offered = [0u8; 32];
    for (bytes, word) in offered.chunks_mut(4).zip(&OFFERED) {
        bytes.copy_from_slice(&word.swap(0, Ordering::Relaxed).to_le_bytes());
    }
    offered
}

/// Leaves something observed for the pool to mix in later. Safe from any thread.
pub fn offer(observed: &[u8]) {
    for (bytes, word) in digest(observed).chunks(4).zip(&OFFERED) {
        word.fetch_xor(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]), Ordering::Relaxed);
    }
}

/// Mixes in something observed, along with the moment it was observed.
pub fn stir(observed: &[u8]) {
    let observed = digest(observed);
    let mut pool = POOL.lock();
    pool.state = Sha256::new().chain_update(pool.state).chain_update(observed).chain_update(collect()).finalize().into();
}

pub fn seed() {
    let mut tick = 0u64;
    let mut address = [0u8; 8];
    unsafe {
        psp::sys::sceRtcGetCurrentTick(&mut tick);
        psp::sys::sceWlanGetEtherAddr(address.as_mut_ptr());
    }
    stir(&tick.to_le_bytes());
    stir(&address);
}

fn fill(out: &mut [u8]) -> Result<(), getrandom::Error> {
    stir(&[]);
    let mut pool = POOL.lock();
    for block in out.chunks_mut(32) {
        pool.served += 1;
        let bytes = Sha256::new().chain_update(b"out").chain_update(pool.state).chain_update(pool.served.to_le_bytes()).finalize();
        block.copy_from_slice(&bytes[..block.len()]);
    }
    // Moves the pool on, so a later state cannot rebuild bytes already served.
    pool.state = Sha256::new().chain_update(b"next").chain_update(pool.state).finalize().into();
    Ok(())
}

getrandom::register_custom_getrandom!(fill);
