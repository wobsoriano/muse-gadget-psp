//! The microphone on its way up. The mic runs all the time, so its captures
//! are the clock: what it hears goes up while the button is down and for a
//! short tail after, and zeros after that. The zeros are the quiet stream the
//! server's speech detection needs, paced in real time with no timer.

use super::record;
use alloc::vec::Vec;
use core::iter;

/// 40 ms at 24 kHz, the size of every record.
pub(crate) const FRAME: usize = 960;
/// Records sent in one step, at most. More of the mic waits in the app's ring.
pub(crate) const SEND_PER_STEP: usize = 3;
/// Keeps the last syllable.
const TAIL_MS: usize = 120;

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

    fn run(&mut self, input: impl IntoIterator<Item = i16>, out: &mut Vec<i16>) {
        for next in input {
            let Some(prev) = self.prev.replace(next) else { continue };
            while self.phase < self.up {
                let step = (i32::from(next) - i32::from(prev)) * self.phase as i32 / self.up as i32;
                out.push((i32::from(prev) + step) as i16);
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
    /// Mic samples in `TAIL_MS`.
    tail: usize,
    /// Mic samples of the tail still to go up since the button was let go.
    tail_left: usize,
    talking: bool,
    /// The server asked for no microphone audio.
    held: bool,
    /// Resampled and not yet sent, at the wire's rate.
    pending: Vec<i16>,
    /// The record being built, the same buffer every time.
    line: Vec<u8>,
}

impl Uplink {
    pub(crate) fn new(mic_rate: u32, wire_rate: u32) -> Uplink {
        Uplink {
            resampler: Resampler::new(mic_rate, wire_rate),
            tail: mic_rate as usize * TAIL_MS / 1000,
            tail_left: 0,
            talking: false,
            held: false,
            pending: Vec::new(),
            line: Vec::new(),
        }
    }

    /// Whether more mic audio is wanted this step. What is not taken stays
    /// in the app's ring, and shows up as `Heard::behind`.
    pub(crate) fn hungry(&self) -> bool {
        self.pending.len() < SEND_PER_STEP * FRAME
    }

    pub(crate) fn talking(&self) -> bool {
        self.talking
    }

    /// One capture, and the button's edge if it has one.
    pub(crate) fn hear(&mut self, pcm: &[i16], talking: bool) -> Option<Edge> {
        let edge = self.edge(talking);
        let real = self.real(pcm.len());
        if !self.held {
            self.resampler.run(pcm[..real].iter().copied(), &mut self.pending);
            self.resampler.run(iter::repeat_n(0, pcm.len() - real), &mut self.pending);
        }
        edge
    }

    /// A capture thrown away because the call fell behind. The button is
    /// still followed, so no press is missed.
    pub(crate) fn skip(&mut self, samples: usize, talking: bool) -> Option<Edge> {
        let edge = self.edge(talking);
        self.real(samples);
        self.resampler.reset();
        edge
    }

    /// The server's hold. Going on drops what is not sent yet.
    pub(crate) fn hold(&mut self, held: bool) {
        if held && !self.held {
            self.discard();
        }
        self.held = held;
    }

    /// The oldest whole frame as a record, ready to send.
    pub(crate) fn next_line(&mut self) -> Option<&[u8]> {
        if self.pending.len() < FRAME {
            return None;
        }
        record::append(&self.pending[..FRAME], &mut self.line);
        self.pending.drain(..FRAME);
        Some(&self.line)
    }

    /// Drops everything not yet sent.
    pub(crate) fn discard(&mut self) {
        self.pending.clear();
        self.resampler.reset();
    }

    fn edge(&mut self, talking: bool) -> Option<Edge> {
        let edge = match (self.talking, talking) {
            (false, true) => Some(Edge::Pressed),
            (true, false) => {
                self.tail_left = self.tail;
                Some(Edge::Released)
            }
            _ => None,
        };
        self.talking = talking;
        edge
    }

    /// How many of the next `samples` are the mic's own, the rest being zeros.
    fn real(&mut self, samples: usize) -> usize {
        if self.talking {
            return samples;
        }
        let real = self.tail_left.min(samples);
        self.tail_left -= real;
        real
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
        let mut out = Vec::new();
        let mut from = 0;
        for &cut in cuts.iter().chain([&input.len()]) {
            resampler.run(input[from..cut].iter().copied(), &mut out);
            from = cut;
        }
        out
    }

    fn sine(hz: f64, samples: usize, rate: u32) -> Vec<i16> {
        (0..samples).map(|i| (8000.0 * (2.0 * core::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as i16).collect()
    }

    /// Every record waiting, decoded back to samples.
    fn sent(uplink: &mut Uplink) -> Vec<i16> {
        let mut samples = Vec::new();
        while let Some(line) = uplink.next_line() {
            let record: Value = serde_json::from_slice(line).unwrap();
            let mut bytes = Vec::new();
            ws::unbase64(record["audio"].as_str().unwrap().as_bytes(), usize::MAX, &mut bytes).unwrap();
            assert_eq!(bytes.len(), 2 * FRAME, "only whole frames go up");
            samples.extend(bytes.chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]])));
        }
        samples
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
    fn after_a_release_exactly_the_tail_of_real_audio_goes_up_then_zeros() {
        let mut uplink = Uplink::new(MIC, WIRE);
        let loud = [1000; CHUNK];
        for _ in 0..3 {
            uplink.hear(&loud, false);
        }
        let mut out = sent(&mut uplink);
        assert!(out.iter().all(|&sample| sample == 0), "nothing but the quiet stream before a press");
        for _ in 0..20 {
            uplink.hear(&loud, true);
        }
        for _ in 0..20 {
            uplink.hear(&loud, false);
        }
        out.extend(sent(&mut uplink));
        let end = 3 * CHUNK + 20 * CHUNK + 22_050 * TAIL_MS / 1000;
        let real_outputs = (end * 160).div_ceil(147);
        let first_loud = (3 * CHUNK * 160).div_ceil(147);
        let loud_part = &out[first_loud..real_outputs - 1];
        assert!(loud_part.iter().all(|&sample| sample == 1000), "talk and tail are the mic's own");
        assert!(out[real_outputs - 1] != 0, "the step down to the zeros");
        assert!(out[real_outputs..].iter().all(|&sample| sample == 0), "exact zeros after the tail");
        assert!(out.len() > real_outputs + FRAME, "and they keep coming");
    }

    #[test]
    fn hold_discards_and_starts_again_from_nothing() {
        let tone = sine(300.0, 4 * CHUNK, MIC);
        let mut uplink = Uplink::new(MIC, WIRE);
        for chunk in tone.chunks(CHUNK) {
            uplink.hear(chunk, true);
        }
        uplink.hold(true);
        assert_eq!(uplink.next_line(), None, "what was waiting is dropped");
        uplink.hear(&tone[..CHUNK], true);
        assert_eq!(uplink.next_line(), None, "nothing goes up while held");
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
    fn a_skipped_capture_follows_the_button_and_sends_nothing() {
        let mut uplink = Uplink::new(MIC, WIRE);
        for _ in 0..10 {
            assert_eq!(uplink.skip(CHUNK, false), None);
        }
        assert_eq!(uplink.skip(CHUNK, true), Some(Edge::Pressed));
        assert_eq!(uplink.next_line(), None);
        assert_eq!(uplink.skip(CHUNK, false), Some(Edge::Released));
        assert_eq!(uplink.next_line(), None);
    }

    #[test]
    fn it_wants_the_mic_until_a_steps_worth_of_frames_waits() {
        let mut uplink = Uplink::new(MIC, WIRE);
        let mut chunks = 0;
        while uplink.hungry() {
            uplink.hear(&[7; CHUNK], true);
            chunks += 1;
        }
        assert_eq!(chunks, (SEND_PER_STEP * FRAME * 147).div_ceil(160 * CHUNK), "a little over three frames of mic");
        let lines = core::iter::from_fn(|| uplink.next_line().map(<[u8]>::len)).count();
        assert_eq!(lines, SEND_PER_STEP);
        assert!(uplink.hungry());
    }
}
