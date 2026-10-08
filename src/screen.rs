//! The PSP's display as a plain 480 by 272 grid of 16-bit colours.

use psp::sys::{self, DisplayMode, DisplayPixelFormat, DisplaySetBufSync};

pub const WIDTH: usize = 480;
pub const HEIGHT: usize = 272;
/// Pixels from the start of one row to the start of the next.
const STRIDE: usize = 512;
/// Reaches video memory without the CPU cache in between, so what is
/// written is what the display reads.
const UNCACHED: usize = 0x4000_0000;

pub struct Screen(*mut u16);

impl Screen {
    pub fn open() -> Screen {
        unsafe {
            let pixels = (sys::sceGeEdramGetAddr() as usize | UNCACHED) as *mut u16;
            sys::sceDisplaySetMode(DisplayMode::Lcd, WIDTH, HEIGHT);
            sys::sceDisplaySetFrameBuf(pixels as *const u8, STRIDE, DisplayPixelFormat::Psm5650, DisplaySetBufSync::NextFrame);
            let mut screen = Screen(pixels);
            for y in 0..HEIGHT {
                screen.row(y).fill(0);
            }
            screen
        }
    }

    pub fn row(&mut self, y: usize) -> &mut [u16] {
        assert!(y < HEIGHT);
        unsafe { core::slice::from_raw_parts_mut(self.0.add(y * STRIDE), WIDTH) }
    }

    /// Waits for the display to finish the frame it is showing.
    pub fn next_frame(&self) {
        unsafe { sys::sceDisplayWaitVblankStart() };
    }
}
