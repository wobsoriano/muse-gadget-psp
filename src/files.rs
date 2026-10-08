//! Whole files on the Memory Stick.

use alloc::vec::Vec;
use core::ffi::c_void;
use psp::sys::{self, IoOpenFlags};

fn c_path(path: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(path.len() + 1);
    bytes.extend_from_slice(path.as_bytes());
    bytes.push(0);
    bytes
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
    unsafe {
        let flags = IoOpenFlags::WR_ONLY | IoOpenFlags::CREAT | IoOpenFlags::TRUNC;
        let file = sys::sceIoOpen(c_path(path).as_ptr(), flags, 0o666);
        if file.0 < 0 {
            return false;
        }
        let wrote = sys::sceIoWrite(file, bytes.as_ptr() as *const c_void, bytes.len());
        sys::sceIoClose(file) >= 0 && wrote == bytes.len() as i32
    }
}

pub fn remove(path: &str) {
    unsafe { sys::sceIoRemove(c_path(path).as_ptr()) };
}

pub fn rename(from: &str, to: &str) -> bool {
    unsafe { sys::sceIoRename(c_path(from).as_ptr(), c_path(to).as_ptr()) >= 0 }
}
