//! Progress, in a file next to the app. The screen belongs to the avatar, so
//! nothing is printed there. The file is written line by line so a stall
//! still leaves everything up to it.

use alloc::format;
use core::ffi::c_void;
use core::fmt;
use psp::sys::{self, IoOpenFlags};

const PATH: &[u8] = b"log.txt\0";

fn write(mode: IoOpenFlags, bytes: &[u8]) {
    unsafe {
        let file = sys::sceIoOpen(PATH.as_ptr(), IoOpenFlags::WR_ONLY | IoOpenFlags::CREAT | mode, 0o666);
        if file.0 >= 0 {
            sys::sceIoWrite(file, bytes.as_ptr() as *const c_void, bytes.len());
            sys::sceIoClose(file);
        }
    }
}

/// Milliseconds since the PSP started.
pub fn now_ms() -> u32 {
    (unsafe { sys::sceKernelGetSystemTimeWide() } / 1000) as u32
}

pub fn begin() {
    write(IoOpenFlags::TRUNC, b"");
}

pub fn line(text: fmt::Arguments) {
    write(IoOpenFlags::APPEND, format!("{:>7} {}\n", now_ms(), text).as_bytes());
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
