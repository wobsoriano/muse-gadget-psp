//! The microphone on its way up. The mic runs all the time, so its captures
//! are the clock. What it hears goes up while the button is down, for a short
//! tail after, and then as a second of zeros, which is what the server's
//! speech detection needs to hear the turn end. After that nothing goes up
//! until the next press, since this radio cannot carry a quiet stream all call
//! long. What waits to go up is bounded, and leaves at no more than one and a
//! half times real time.

use super::record;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::iter;

/// 40 ms at 24 kHz, the size of every record.
pub(crate) const FRAME: usize = 960;
/// Frames that may leave at once after a pause, and the most one step sends.
const BURST: usize = 3;
/// Keeps the last syllable.
const TAIL_MS: usize = 120;
/// Zeros after the tail, for the server to hear the turn end.
const QUIET_MS: usize = 1000;
/// Behind this much, captures are thrown away...
const MOST_BEHIND_MS: u64 = 8_000;
/// ...until the backlog is down to this.
const RESUME_BEHIND_MS: u64 = 4_000;

/// Linear interpolation at an exact rational step, 147:160 for 22050 to
/// 24000, so a long call never drifts. Cutting the input anywhere gives the
/// same output.
struct Resampler {
    up: u32,
    down: u32,
    /// Where the next output falls after `prev`, in steps of 1/up.
    phase: u32,
    prev: Option<i16>,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Resampler {
        let (mut a, mut b) = (from, to);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Resampler { up: to / a, down: from / a, phase: 0, prev: None }
    }

    fn run(&mut self, input: impl IntoIterator<Item = i16>, out: &mut VecDeque<i16>) {
        for next in input {
            let Some(prev) = self.prev.replace(next) else { continue };
            while self.phase < self.up {
                let step = (i32::from(next) - i32::from(prev)) * self.phase as i32 / self.up as i32;
                out.push_back((i32::from(prev) + step) as i16);
                self.phase += self.down;
            }
            self.phase -= self.up;
        }
    }

    /// Nothing is carried across a gap.
    fn reset(&mut self) {
        self.phase = 0;
        self.prev = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Edge {
    Pressed,
    Released,
}

pub(crate) struct Uplink {
    resampler: Resampler,
    wire_rate: u32,
    /// Mic samples in `TAIL_MS` and in `QUIET_MS`.
    tail: usize,
    quiet: usize,
    /// Of each, still to go up since the button was let go.
    tail_left: usize,
    quiet_left: usize,
    talking: bool,
    /// The server asked for no microphone audio.
    held: bool,
    /// Resampled and not yet sent, at the wire's rate.
    pending: VecDeque<i16>,
    /// Throwing captures away until little is left behind.
    skipping: bool,
    /// Mic samples thrown away that would have gone up.
    skipped: usize,
    frames_sent: usize,
    /// Wire samples that may leave now. Time adds to it, up to a burst.
    credit: usize,
    credited_ms: u64,
    frame: [i16; FRAME],
    /// The record being built, the same buffer every time.
    line: Vec<u8>,
}

impl Uplink {
    pub(crate) fn new(mic_rate: u32, wire_rate: u32) -> Uplink {
        let most_behind = wire_rate as usize * MOST_BEHIND_MS as usize / 1000;
        Uplink {
            resampler: Resampler::new(mic_rate, wire_rate),
            wire_rate,
            tail: mic_rate as usize * TAIL_MS / 1000,
            quiet: mic_rate as usize * QUIET_MS / 1000,
            tail_left: 0,
            quiet_left: 0,
            talking: false,
            held: false,
            pending: VecDeque::with_capacity(most_behind + 2 * FRAME),
            skipping: false,
            skipped: 0,
            frames_sent: 0,
            credit: 0,
            credited_ms: 0,
            frame: [0; FRAME],
            line: Vec::new(),
        }
    }

    pub(crate) fn talking(&self) -> bool {
        self.talking
    }

    pub(crate) fn skipping(&self) -> bool {
        self.skipping
    }

    pub(crate) fn skipped(&self) -> usize {
        self.skipped
    }

    pub(crate) fn frames_sent(&self) -> usize {
        self.frames_sent
    }

    /// How late the next frame to leave is.
    pub(crate) fn behind_ms(&self) -> u64 {
        self.pending.len() as u64 * 1000 / u64::from(self.wire_rate)
    }

    /// One capture, and the button's edge if it has one.
    pub(crate) fn hear(&mut self, pcm: &[i16], talking: bool) -> Option<Edge> {
        let edge = self.edge(talking);
        let (real, zeros) = self.due(pcm.len());
        let behind_ms = self.behind_ms();
        self.skipping = behind_ms > MOST_BEHIND_MS || self.skipping && behind_ms > RESUME_BEHIND_MS;
        if self.skipping {
            self.skipped += real + zeros;
        }
        if self.held || self.skipping || real + zeros == 0 {
            self.resampler.reset();
            return edge;
        }
        self.resampler.run(pcm[..real].iter().copied(), &mut self.pending);
        self.resampler.run(iter::repeat_n(0, zeros), &mut self.pending);
        edge
    }

    /// The server's hold. Going on drops what is not sent yet.
    pub(crate) fn hold(&mut self, held: bool) {
        if held && !self.held {
            self.discard();
        }
        self.held = held;
    }

    /// The oldest whole frame as a record, if one may leave now.
    pub(crate) fn next_line(&mut self, now_ms: u64) -> Option<&[u8]> {
        let elapsed_ms = now_ms.saturating_sub(self.credited_ms).min(1000) as usize;
        self.credited_ms = now_ms;
        self.credit = (self.credit + elapsed_ms * self.catch_up_per_ms()).min(BURST * FRAME);
        if self.pending.len() < FRAME || self.credit < FRAME {
            return None;
        }
        self.credit -= FRAME;
        for (sample, sent) in self.frame.iter_mut().zip(self.pending.drain(..FRAME)) {
            *sample = sent;
        }
        record::append(&self.frame, &mut self.line);
        self.frames_sent += 1;
        Some(&self.line)
    }

    /// Drops everything not yet sent.
    pub(crate) fn discard(&mut self) {
        self.pending.clear();
        self.resampler.reset();
    }

    /// One and a half times real time.
    fn catch_up_per_ms(&self) -> usize {
        self.wire_rate as usize * 3 / 2 / 1000
    }

    fn edge(&mut self, talking: bool) -> Option<Edge> {
        let edge = match (self.talking, talking) {
            (false, true) => Some(Edge::Pressed),
            (true, false) => {
                self.tail_left = self.tail;
                self.quiet_left = self.quiet;
                Some(Edge::Released)
            }
            _ => None,
        };
        self.talking = talking;
        edge
    }

    /// Of the next `samples`, how many are the mic's own and how many are
    /// zeros. The rest go nowhere.
    fn due(&mut self, samples: usize) -> (usize, usize) {
        if self.talking {
            return (samples, 0);
        }
        let real = self.tail_left.min(samples);
        self.tail_left -= real;
        let zeros = self.quiet_left.min(samples - real);
        self.quiet_left -= zeros;
        (real, zeros)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Dice;
    use super::*;
    use crate::ws;
    use serde_json::Value;

    const MIC: u32 = 22_050;
    const WIRE: u32 = 24_000;
    const CHUNK: usize = 512;

    fn resampled(input: &[i16], cuts: &[usize]) -> Vec<i16> {
        let mut resampler = Resampler::new(MIC, WIRE);
        let mut out = VecDeque::new();
        let mut from = 0;
        for &cut in cuts.iter().chain([&input.len()]) {
            resampler.run(input[from..cut].iter().copied(), &mut out);
            from = cut;
        }
        out.into()
    }

    fn sine(hz: f64, samples: usize, rate: u32) -> Vec<i16> {
        (0..samples).map(|i| (8000.0 * (2.0 * core::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as i16).collect()
    }

    fn decoded(line: &[u8]) -> Vec<i16> {
        let record: Value = serde_json::from_slice(line).unwrap();
        let mut bytes = Vec::new();
        ws::unbase64(record["audio"].as_str().unwrap().as_bytes(), usize::MAX, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 2 * FRAME, "only whole frames go up");
        bytes.chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]])).collect()
    }

    /// Every record waiting, decoded back to samples, with all the time in
    /// the world between them.
    fn sent(uplink: &mut Uplink) -> Vec<i16> {
        let mut samples = Vec::new();
        let mut now_ms = uplink.credited_ms;
        loop {
            now_ms += 1000;
            let Some(line) = uplink.next_line(now_ms) else { break samples };
            samples.extend(decoded(line));
        }
    }

    fn talk(uplink: &mut Uplink, chunks: usize, talking: bool) {
        for _ in 0..chunks {
            uplink.hear(&[1000; CHUNK], talking);
        }
    }

    #[test]
    fn a_second_at_22050_is_a_second_at_24000() {
        let out = resampled(&[0; 22_050], &[]);
        assert!((23_999..=24_001).contains(&out.len()), "{}", out.len());
    }

    #[test]
    fn cutting_the_input_anywhere_gives_the_same_output() {
        let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
        let input: Vec<i16> = (0..10_000).map(|_| dice.below(65_536) as i16).collect();
        let whole = resampled(&input, &[]);
        for _ in 0..50 {
            let mut cuts: Vec<usize> = (0..dice.below(40)).map(|_| dice.below(input.len())).collect();
            cuts.sort();
            assert_eq!(resampled(&input, &cuts), whole, "cut at {cuts:?}");
        }
    }

    #[test]
    fn a_steady_level_stays_exactly_where_it_is() {
        assert!(resampled(&[1234; 5000], &[]).iter().all(|&sample| sample == 1234));
        assert!(resampled(&[-32_768; 5000], &[17]).iter().all(|&sample| sample == -32_768));
    }

    #[test]
    fn a_tone_keeps_its_pitch() {
        let out = resampled(&sine(440.0, MIC as usize, MIC), &[]);
        let crossings = out.windows(2).filter(|pair| (pair[0] < 0) != (pair[1] < 0)).count();
        assert!((878..=882).contains(&crossings), "{crossings} crossings in a second, 880 expected");
    }

    #[test]
    fn edges_come_from_the_button_as_each_capture_was_heard() {
        let mut uplink = Uplink::new(MIC, WIRE);
        let chunk = [100; CHUNK];
        assert_eq!(uplink.hear(&chunk, false), None);
        assert_eq!(uplink.hear(&chunk, true), Some(Edge::Pressed), "a press after quiet");
        assert!(uplink.talking());
        assert_eq!(uplink.hear(&chunk, true), None);
        assert_eq!(uplink.hear(&chunk, false), Some(Edge::Released));
        assert!(!uplink.talking());
    }

    #[test]
    fn after_a_release_the_tail_goes_up_then_a_second_of_zeros_then_nothing() {
        let mut uplink = Uplink::new(MIC, WIRE);
        talk(&mut uplink, 3, false);
        assert!(sent(&mut uplink).is_empty(), "nothing goes up before a press");
        talk(&mut uplink, 20, true);
        talk(&mut uplink, 60, false);
        let out = sent(&mut uplink);
        let real = 20 * CHUNK + MIC as usize * TAIL_MS / 1000;
        let real_outputs = (real * 160).div_ceil(147);
        let quiet_outputs = ((real + MIC as usize * QUIET_MS / 1000) * 160).div_ceil(147);
        assert!(out[..real_outputs - 1].iter().all(|&sample| sample == 1000), "talk and tail are the mic's own");
        assert!(out[real_outputs - 1] != 0, "the step down to the zeros");
        assert!(out[real_outputs..].iter().all(|&sample| sample == 0), "exact zeros after the tail");
        assert_eq!(out.len(), quiet_outputs / FRAME * FRAME, "a second of them, in whole frames");
        talk(&mut uplink, 60, false);
        assert!(sent(&mut uplink).is_empty(), "and nothing after");
        assert_eq!(uplink.skipped(), 0);
    }

    #[test]
    fn hold_discards_and_starts_again_from_nothing() {
        let tone = sine(300.0, 4 * CHUNK, MIC);
        let mut uplink = Uplink::new(MIC, WIRE);
        for chunk in tone.chunks(CHUNK) {
            uplink.hear(chunk, true);
        }
        uplink.hold(true);
        assert_eq!(uplink.next_line(1000), None, "what was waiting is dropped");
        uplink.hear(&tone[..CHUNK], true);
        assert_eq!(uplink.next_line(2000), None, "nothing goes up while held");
        uplink.hold(false);
        for chunk in tone.chunks(CHUNK) {
            uplink.hear(chunk, true);
        }
        let mut fresh = Uplink::new(MIC, WIRE);
        for chunk in tone.chunks(CHUNK) {
            fresh.hear(chunk, true);
        }
        assert_eq!(sent(&mut uplink), sent(&mut fresh), "the same as a new uplink");
    }

    #[test]
    fn frames_leave_in_a_burst_then_at_one_and_a_half_times_real_time() {
        let mut uplink = Uplink::new(MIC, WIRE);
        talk(&mut uplink, 100, true);
        let lines = |uplink: &mut Uplink, now_ms| core::iter::from_fn(|| uplink.next_line(now_ms).map(<[u8]>::len)).count();
        assert_eq!(lines(&mut uplink, 5000), BURST, "a burst to start");
        assert_eq!(lines(&mut uplink, 5020), 0, "20 ms is not a frame's worth yet");
        assert_eq!(lines(&mut uplink, 5027), 1, "27 ms at one and a half times real time is");
        assert_eq!(lines(&mut uplink, 5107), BURST, "80 ms is three");
        assert_eq!(lines(&mut uplink, 9000), BURST, "a long pause earns no more than a burst");
    }

    #[test]
    fn eight_seconds_behind_captures_are_thrown_away_until_four_remain() {
        let mut uplink = Uplink::new(MIC, WIRE);
        let chunks_in = |ms: usize| ms * MIC as usize / 1000 / CHUNK;
        talk(&mut uplink, chunks_in(8000) + 20, true);
        assert!(uplink.skipping());
        let skipped = uplink.skipped();
        assert!((CHUNK..=20 * CHUNK).contains(&skipped), "{skipped} skipped");
        talk(&mut uplink, 10, true);
        assert_eq!(uplink.skipped(), skipped + 10 * CHUNK, "still behind, still skipping");
        let mut now_ms = 0;
        while uplink.behind_ms() > 5000 {
            now_ms += 1000;
            uplink.next_line(now_ms).unwrap();
        }
        uplink.hear(&[1000; CHUNK], true);
        assert!(uplink.skipping(), "five seconds behind is not caught up enough");
        while uplink.behind_ms() > 3900 {
            now_ms += 1000;
            uplink.next_line(now_ms).unwrap();
        }
        let waiting = uplink.pending.len();
        uplink.hear(&[1000; CHUNK], true);
        assert!(!uplink.skipping());
        assert!(uplink.pending.len() > waiting, "the capture goes up again");
        assert_eq!(uplink.skipped(), skipped + 11 * CHUNK);
    }
}
