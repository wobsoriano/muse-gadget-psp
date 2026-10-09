//! Whole files on the Memory Stick.

use alloc::vec::Vec;
use core::ffi::c_void;
use psp::sys::{self, IoOpenFlags};

/// The folder the app was started from, with its closing slash.
pub fn home() -> &'static [u8] {
    let started_from = psp::STARTED_FROM.load(core::sync::atomic::Ordering::Relaxed);
    if started_from.is_null() {
        return b"";
    }
    let mut length = 0;
    let mut folder = 0;
    unsafe {
        while *started_from.add(length) != 0 {
            length += 1;
            if *started_from.add(length - 1) == b'/' {
                folder = length;
            }
        }
        core::slice::from_raw_parts(started_from, folder)
    }
}

/// `path` under the app's own folder, as the system calls want it. The
/// PSP's working folder is the main thread's alone, so a bare name would
/// only work there.
fn c_path(path: &str) -> Vec<u8> {
    let home = home();
    let mut bytes = Vec::with_capacity(home.len() + path.len() + 1);
    bytes.extend_from_slice(home);
    bytes.extend_from_slice(path.as_bytes());
    bytes.push(0);
    bytes
}

/// Adds to the end of the file, creating it if need be.
pub fn append(path: &str, bytes: &[u8]) {
    write_with(path, IoOpenFlags::APPEND, bytes);
}

fn write_with(path: &str, mode: IoOpenFlags, bytes: &[u8]) -> bool {
    unsafe {
        let file = sys::sceIoOpen(c_path(path).as_ptr(), IoOpenFlags::WR_ONLY | IoOpenFlags::CREAT | mode, 0o666);
        if file.0 < 0 {
            return false;
        }
        let wrote = sys::sceIoWrite(file, bytes.as_ptr() as *const c_void, bytes.len());
        sys::sceIoClose(file) >= 0 && wrote == bytes.len() as i32
    }
}

pub fn read(path: &str) -> Option<Vec<u8>> {
    unsafe {
        let file = sys::sceIoOpen(c_path(path).as_ptr(), IoOpenFlags::RD_ONLY, 0);
        if file.0 < 0 {
            return None;
        }
        let mut bytes = Vec::new();
        let mut chunk = alloc::vec![0u8; 64 * 1024];
        loop {
            let got = sys::sceIoRead(file, chunk.as_mut_ptr() as *mut c_void, chunk.len() as u32);
            if got <= 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..got as usize]);
        }
        sys::sceIoClose(file);
        Some(bytes)
    }
}

/// Replaces the file. True only when every byte was written and the file closed cleanly.
pub fn write(path: &str, bytes: &[u8]) -> bool {
    write_with(path, IoOpenFlags::TRUNC, bytes)
}

pub fn remove(path: &str) {
    unsafe { sys::sceIoRemove(c_path(path).as_ptr()) };
}

pub fn rename(from: &str, to: &str) -> bool {
    unsafe { sys::sceIoRename(c_path(from).as_ptr(), c_path(to).as_ptr()) >= 0 }
}
