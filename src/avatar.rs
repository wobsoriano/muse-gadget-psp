//! The Muse Gadget SDK's avatar, played from the clips tools/avatar.sh
//! renders with the SDK's own code. The file layout is described in
//! tools/render_avatar.c.

use crate::screen::{self, Screen};
use alloc::vec::Vec;

/// What the avatar is doing. The order is the order of clips in the file.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mood {
    Boot,
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
    /// Not shown by this app. It is here because the file's clips are in this order.
    #[allow(dead_code)]
    Off,
    Happy,
}

/// How many clips the file must hold.
const MOODS: usize = Mood::Happy as usize + 1;

struct Clip {
    start: usize,
    frames: usize,
    loops: bool,
}

pub struct Avatar {
    file: Vec<u8>,
    side: usize,
    colours: usize,
    frames_per_second: usize,
    clips: Vec<Clip>,
}

/// Screen pixels per avatar pixel. The last row and column of each block are
/// drawn in the edge colour, which is the SDK's grid texture.
const BLOCK: usize = 4;

impl Avatar {
    /// Reads the clips, or `None` when the file is not one this app wrote.
    pub fn parse(file: Vec<u8>) -> Option<Avatar> {
        let word = |at: usize| Some(u16::from_le_bytes([*file.get(at)?, *file.get(at + 1)?]) as usize);
        if file.get(..8)? != b"MUSEAV1\0" {
            return None;
        }
        let (side, height, colours, frames_per_second, count) = (word(8)?, word(10)?, word(12)?, word(14)?, word(16)?);
        if side != height || side * BLOCK > screen::HEIGHT || frames_per_second == 0 || count != MOODS {
            return None;
        }
        let frame = 4 * colours + side * side;
        let mut start = 18 + 4 * count;
        let mut clips = Vec::with_capacity(count);
        for clip in 0..count {
            let frames = word(18 + 4 * clip)?;
            if frames == 0 {
                return None;
            }
            clips.push(Clip { start, frames, loops: word(20 + 4 * clip)? != 0 });
            start += frames * frame;
        }
        let pixels = file.get(18 + 4 * count + 4 * colours..start)?;
        if pixels.chunks(frame).any(|frame| frame[..side * side].iter().any(|&colour| colour as usize >= colours)) {
            return None;
        }
        Some(Avatar { file, side, colours, frames_per_second, clips })
    }

    /// Draws `mood` as it looks `since_ms` after it began, centred on the screen.
    pub fn draw(&self, screen: &mut Screen, mood: Mood, since_ms: u32) {
        let clip = &self.clips[mood as usize];
        let played = since_ms as usize * self.frames_per_second / 1000;
        let frame = if clip.loops { played % clip.frames } else { played.min(clip.frames - 1) };
        let at = clip.start + frame * (4 * self.colours + self.side * self.side);
        let colour = |table: usize, index: u8| {
            let at = at + 2 * (table * self.colours + index as usize);
            u16::from_le_bytes([self.file[at], self.file[at + 1]])
        };
        let pixels = &self.file[at + 4 * self.colours..][..self.side * self.side];
        let left = (screen::WIDTH - self.side * BLOCK) / 2;
        let top = (screen::HEIGHT - self.side * BLOCK) / 2;
        for (y, cells) in pixels.chunks(self.side).enumerate() {
            for line in 0..BLOCK {
                let row = &mut screen.row(top + y * BLOCK + line)[left..left + self.side * BLOCK];
                let edge_line = line == BLOCK - 1;
                for (block, &cell) in row.chunks_mut(BLOCK).zip(cells) {
                    let (plain, edge) = (colour(0, cell), colour(1, cell));
                    block[..BLOCK - 1].fill(if edge_line { edge } else { plain });
                    block[BLOCK - 1] = edge;
                }
            }
        }
    }
}
