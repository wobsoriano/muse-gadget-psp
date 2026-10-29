//! A small light and a word in the corner of the screen, for whether Muse
//! can be reached, and in call mode a second pair under it for the call.

use crate::call::Line;
use crate::screen::Screen;
use alloc::format;

/// rust-psp's debug font: 256 characters, eight rows of eight pixels each,
/// the left pixel in the high bit.
const FONT: &[u8; 2048] = include_bytes!("msxfont.bin");

const LEFT: usize = 10;
const TOP: usize = 10;
const CALL_TOP: usize = 22;
const WIDEST: usize = 14 * 8;
/// The light and the call's longest words, "START to call" and "on call 14:59".
const CALL_WIDEST: usize = 12 + 13 * 8;
const WORD_COLOUR: u16 = colour(150, 150, 160);
const AMBER: u16 = colour(255, 180, 40);
const GREEN: u16 = colour(70, 220, 110);
const RED: u16 = colour(255, 80, 80);
const GREY: u16 = colour(110, 110, 120);

/// A colour as the screen holds it, with red in the low bits.
const fn colour(red: u16, green: u16, blue: u16) -> u16 {
    red >> 3 | (green >> 2) << 5 | (blue >> 3) << 11
}

#[derive(Clone, Copy, PartialEq)]
pub enum Link {
    Connecting,
    Online,
    Offline,
}

impl Link {
    fn word(self) -> &'static str {
        match self {
            Link::Connecting => "connecting",
            Link::Online => "online",
            Link::Offline => "offline",
        }
    }

    fn light(self) -> u16 {
        match self {
            Link::Connecting => AMBER,
            Link::Online => GREEN,
            Link::Offline => RED,
        }
    }
}

/// Draws the light and the word. `now_ms` makes the light blink while
/// connecting.
pub fn draw(screen: &mut Screen, link: Link, now_ms: u32) {
    let lit = link != Link::Connecting || blink(now_ms);
    draw_row(screen, TOP, WIDEST, lit.then(|| link.light()), link.word());
}

/// Draws the call's row. The light blinks while dialing.
pub fn draw_call(screen: &mut Screen, line: Line, now_ms: u32) {
    let clock;
    let (light, word) = match line {
        Line::Idle => (GREY, "START to call"),
        Line::Dialing => (AMBER, "calling"),
        Line::Live { since_ms, weak, .. } => {
            let seconds = now_ms.wrapping_sub(since_ms) / 1000;
            clock = format!("on call {}:{:02}", seconds / 60, seconds % 60);
            (if weak { AMBER } else { GREEN }, clock.as_str())
        }
        Line::Ending => (AMBER, "hanging up"),
        Line::Ended { word, failed } => (if failed { RED } else { GREY }, word),
    };
    let lit = line != Line::Dialing || blink(now_ms);
    draw_row(screen, CALL_TOP, CALL_WIDEST, lit.then_some(light), word);
}

/// Blanks the call's row, which nothing draws in voice-note mode.
pub fn clear_call(screen: &mut Screen) {
    draw_row(screen, CALL_TOP, CALL_WIDEST, None, "");
}

fn blink(now_ms: u32) -> bool {
    now_ms / 500 % 2 == 0
}

fn draw_row(screen: &mut Screen, top: usize, widest: usize, light: Option<u16>, word: &str) {
    for line in 0..8 {
        let row = &mut screen.row(top + line)[LEFT..LEFT + widest];
        row.fill(0);
        if let (Some(light), 1..=6) = (light, line) {
            let inset = usize::from(line == 1 || line == 6);
            row[1 + inset..7 - inset].fill(light);
        }
        for (place, character) in word.bytes().enumerate() {
            let bits = FONT[character as usize * 8 + line];
            for pixel in 0..8 {
                if bits & (0x80 >> pixel) != 0 {
                    row[12 + place * 8 + pixel] = WORD_COLOUR;
                }
            }
        }
    }
}
