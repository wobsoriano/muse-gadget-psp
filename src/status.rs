//! A small light and a word in the corner of the screen, for whether Muse
//! can be reached.

use crate::screen::Screen;

/// rust-psp's debug font: 256 characters, eight rows of eight pixels each,
/// the left pixel in the high bit.
const FONT: &[u8; 2048] = include_bytes!("msxfont.bin");

const LEFT: usize = 10;
const TOP: usize = 10;
const WIDEST: usize = 14 * 8;
const WORD_COLOUR: u16 = colour(150, 150, 160);

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
            Link::Connecting => colour(255, 180, 40),
            Link::Online => colour(70, 220, 110),
            Link::Offline => colour(255, 80, 80),
        }
    }
}

/// Draws the light and the word. `now_ms` makes the light blink while
/// connecting.
pub fn draw(screen: &mut Screen, link: Link, now_ms: u32) {
    let lit = link != Link::Connecting || now_ms / 500 % 2 == 0;
    for line in 0..8 {
        let row = &mut screen.row(TOP + line)[LEFT..LEFT + WIDEST];
        row.fill(0);
        if lit && (1..7).contains(&line) {
            let inset = usize::from(line == 1 || line == 6);
            row[1 + inset..7 - inset].fill(link.light());
        }
        for (place, character) in link.word().bytes().enumerate() {
            let bits = FONT[character as usize * 8 + line];
            for pixel in 0..8 {
                if bits & (0x80 >> pixel) != 0 {
                    row[12 + place * 8 + pixel] = WORD_COLOUR;
                }
            }
        }
    }
}
