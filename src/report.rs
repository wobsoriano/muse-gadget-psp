//! Progress, in a file next to the app. The screen belongs to the avatar, so
//! nothing is printed there. The file is written line by line so a stall
//! still leaves everything up to it.

use alloc::format;
use core::fmt;
use crate::files;
use psp::sys;

const PATH: &str = "log.txt";
const BEFORE: &str = "log-before.txt";

/// Milliseconds since the PSP started.
pub fn now_ms() -> u32 {
    (unsafe { sys::sceKernelGetSystemTimeWide() } / 1000) as u32
}

/// Starts a new log and keeps the last one. What went wrong is often in the
/// run before the one that shows it.
pub fn begin() {
    files::remove(BEFORE);
    files::rename(PATH, BEFORE);
    files::write(PATH, b"");
}

pub fn line(text: fmt::Arguments) {
    files::append(PATH, format!("{:>7} {}\n", now_ms(), text).as_bytes());
}

#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => { $crate::report::line(format_args!($($arg)*)) };
}

pub struct Timer(u32);

impl Timer {
    pub fn start() -> Self {
        Timer(unsafe { sys::sceKernelGetSystemTimeLow() })
    }

    /// Milliseconds since the last lap.
    pub fn lap(&mut self) -> u32 {
        let now = unsafe { sys::sceKernelGetSystemTimeLow() };
        let passed = now.wrapping_sub(self.0) / 1000;
        self.0 = now;
        passed
    }
}
