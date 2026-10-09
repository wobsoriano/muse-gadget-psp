//! The parts of the Muse connection that do not care which machine they run
//! on. The PSP app and the check that runs on a computer both build on this.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[cfg(all(feature = "plaintext-for-tests", target_os = "psp"))]
compile_error!("plaintext-for-tests is for the fake server on a computer. The PSP always uses TLS.");

pub mod dns;
pub mod https;
pub mod secure;
pub mod speech;
pub mod wav;

mod api;
mod channel;
mod client;
mod http;
mod noise;
mod pace;
mod proto;
mod session;
mod turn;
mod url;
mod ws;

pub use channel::Security;
pub use client::{App, BadConfig, Client, Config, Event, NotSaved, Pairing, Poll, Refused, State, Trouble, Vault};

/// A connected byte stream, such as a TCP socket. Each call waits a bounded
/// time and fails with [`Lost`] past it.
pub trait Stream {
    /// Reads up to `into.len()` bytes. Zero means the other side closed.
    fn read(&mut self, into: &mut [u8]) -> Result<usize, Lost>;
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Lost>;
}

/// The stream failed or stayed silent past its limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lost;

/// A stream that can say whether a read would find anything. The Muse session
/// reads in short slices and does its other work in between, so it must be
/// able to tell "nothing yet" from [`Lost`].
pub trait Wire: Stream {
    /// Waits up to `wait_ms` for bytes to read, or for the other side to
    /// close. False means neither happened.
    fn readable(&mut self, wait_ms: u32) -> Result<bool, Lost>;
}

/// How the client reaches a server: name lookup and connecting.
pub trait Net {
    type Wire: Wire;
    fn open(&mut self, host: &str, port: u16) -> Result<Self::Wire, Lost>;
}

pub trait Clock {
    /// Milliseconds from any fixed start. Never goes back.
    fn millis(&self) -> u64;
    /// Seconds since 1970, or `None` when the clock was never set.
    fn unix_seconds(&self) -> Option<u64>;
}
