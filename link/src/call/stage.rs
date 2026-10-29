//! A call's life, from the dial to the media's end. Pure: an input comes in,
//! and out come the next stage and the one thing to do, so every path is a
//! table test. A call that was admitted ends through end-call, its answer,
//! the media's half-close and its drain, in that order, unless the
//! connection is gone. Half-closing only on the way out of `Ending` is what
//! keeps that order.

use super::Ended;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Limits {
    /// The admission to the media response's head.
    pub head_ms: u64,
    /// The head to hatch.voice.session.
    pub ready_ms: u64,
    /// Live with nothing said, played or worked on.
    pub idle_ms: u64,
    pub longest_ms: u64,
    /// end-call to its answer.
    pub end_call_ms: u64,
    /// The half-close to the media's end.
    pub drain_ms: u64,
    /// Muse's goodbye to the end of its last words.
    pub goodbye_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            head_ms: 30_000,
            ready_ms: 40_000,
            idle_ms: 45_000,
            longest_ms: 900_000,
            end_call_ms: 10_000,
            drain_ms: 5_000,
            goodbye_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Stage {
    Dialing,
    /// The admission went out. Waiting for the media response's head.
    Admitting { since_ms: u64 },
    /// A 2xx head. Waiting for hatch.voice.session.
    Ringing { since_ms: u64 },
    Live { since_ms: u64, quiet_since_ms: u64 },
    /// Muse said goodbye. What is queued plays out first.
    Goodbye { until_ms: u64 },
    /// end-call is out, waiting for its answer. `ended`: the media response
    /// has ended already, so nothing is left to drain.
    Ending { why: Ended, sent_ms: u64, ended: bool },
    /// The media upload is half-closed, and its response is read to the end.
    Closing { why: Ended, since_ms: u64 },
    Over(Ended),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Input {
    /// Connected, and the admission went out.
    Dialed,
    /// The media response's head.
    Head(i32),
    /// hatch.voice.session for this call.
    Ready,
    /// Muse said goodbye.
    Stop,
    HangUp,
    /// Something that ends the call, and why.
    Fatal(Ended),
    /// The media response ended. `reset`: the server reset the stream.
    MediaEnded { reset: bool },
    /// end-call's answer arrived, whatever it says.
    Answered,
    /// Talking, speech heard, audible playback, or Muse starting or settling
    /// a turn: the idle clock starts again.
    Activity,
    /// Time passed. While `busy` the idle clock does not run. `queued`:
    /// Muse's voice not yet played.
    Tick { busy: bool, queued: usize },
    /// The connection is gone.
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Act {
    Nothing,
    /// POST /voice/end-call on a new stream.
    EndCall,
    /// The media upload's empty, final body.
    HalfClose,
    /// Resets whatever stream is still open, then closes the connection.
    Reset,
    /// Closes the connection.
    Close,
}

impl Stage {
    pub(crate) fn on(self, input: Input, now_ms: u64, limits: &Limits) -> (Stage, Act) {
        use Input::*;
        let end = |why| (Stage::Ending { why, sent_ms: now_ms, ended: false }, Act::EndCall);
        let end_ended = |why| (Stage::Ending { why, sent_ms: now_ms, ended: true }, Act::EndCall);
        let after_end_call = |why, ended| match ended {
            true => (Stage::Over(why), Act::Close),
            false => (Stage::Closing { why, since_ms: now_ms }, Act::HalfClose),
        };
        let past = |since: u64, limit: u64| now_ms.saturating_sub(since) >= limit;
        match (self, input) {
            (Stage::Over(why), _) => (Stage::Over(why), Act::Nothing),
            (Stage::Ending { why, .. } | Stage::Closing { why, .. }, Lost) => (Stage::Over(why), Act::Nothing),
            (_, Lost) => (Stage::Over(Ended::Lost), Act::Nothing),

            (Stage::Dialing, Dialed) => (Stage::Admitting { since_ms: now_ms }, Act::Nothing),
            (Stage::Dialing, HangUp) => (Stage::Over(Ended::HungUp), Act::Close),
            (Stage::Dialing, Fatal(why)) => (Stage::Over(why), Act::Close),

            (Stage::Admitting { .. }, Head(status)) if (200..300).contains(&status) => (Stage::Ringing { since_ms: now_ms }, Act::Nothing),
            // Refused, but it may have been admitted on the way.
            (Stage::Admitting { .. }, Head(status)) => end(Ended::Refused(u16::try_from(status).unwrap_or(0))),
            (Stage::Admitting { since_ms }, Tick { .. }) if past(since_ms, limits.head_ms) => end(Ended::NoAnswer),
            (Stage::Ringing { .. }, Ready) => (Stage::Live { since_ms: now_ms, quiet_since_ms: now_ms }, Act::Nothing),
            (Stage::Ringing { since_ms }, Tick { .. }) if past(since_ms, limits.ready_ms) => end(Ended::NoAnswer),
            (Stage::Admitting { .. } | Stage::Ringing { .. }, Stop) => end(Ended::Goodbye),
            (Stage::Admitting { .. } | Stage::Ringing { .. }, MediaEnded { .. }) => end_ended(Ended::Failed),
            (Stage::Admitting { .. } | Stage::Ringing { .. } | Stage::Live { .. }, HangUp) => end(Ended::HungUp),
            (Stage::Admitting { .. } | Stage::Ringing { .. } | Stage::Live { .. }, Fatal(why)) => end(why),

            (Stage::Live { .. }, Ready) => end(Ended::Failed),
            (Stage::Live { .. }, Stop) => (Stage::Goodbye { until_ms: now_ms + limits.goodbye_ms }, Act::Nothing),
            (Stage::Live { .. }, MediaEnded { reset }) => end_ended(if reset { Ended::Failed } else { Ended::Dropped }),
            (Stage::Live { since_ms, .. }, Tick { .. }) if past(since_ms, limits.longest_ms) => end(Ended::TooLong),
            (Stage::Live { since_ms, .. }, Activity | Tick { busy: true, .. }) => (Stage::Live { since_ms, quiet_since_ms: now_ms }, Act::Nothing),
            (Stage::Live { quiet_since_ms, .. }, Tick { .. }) if past(quiet_since_ms, limits.idle_ms) => end(Ended::Idle),

            (Stage::Goodbye { until_ms }, Tick { queued, .. }) if queued == 0 || now_ms >= until_ms => end(Ended::Goodbye),
            (Stage::Goodbye { .. }, HangUp | Fatal(_)) => end(Ended::Goodbye),
            (Stage::Goodbye { .. }, MediaEnded { .. }) => end_ended(Ended::Goodbye),

            (Stage::Ending { why, sent_ms, .. }, MediaEnded { .. }) => (Stage::Ending { why, sent_ms, ended: true }, Act::Nothing),
            (Stage::Ending { why, ended, .. }, Answered) => after_end_call(why, ended),
            (Stage::Ending { why, sent_ms, ended }, Tick { .. }) if past(sent_ms, limits.end_call_ms) => after_end_call(why, ended),
            (Stage::Closing { why, .. }, MediaEnded { .. }) => (Stage::Over(why), Act::Close),
            (Stage::Closing { why, since_ms }, Tick { .. }) if past(since_ms, limits.drain_ms) => (Stage::Over(why), Act::Reset),

            (stage, _) => (stage, Act::Nothing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Dice;
    use super::*;
    use alloc::vec::Vec;
    use Input::*;

    const QUIET: Input = Tick { busy: false, queued: 0 };
    const BUSY: Input = Tick { busy: true, queued: 0 };

    fn ending(why: Ended, sent_ms: u64) -> Stage {
        Stage::Ending { why, sent_ms, ended: false }
    }

    #[test]
    fn every_transition_in_the_table() {
        let live = Stage::Live { since_ms: 0, quiet_since_ms: 0 };
        let rows = [
            (Stage::Dialing, Dialed, 100, Stage::Admitting { since_ms: 100 }, Act::Nothing),
            (Stage::Dialing, HangUp, 100, Stage::Over(Ended::HungUp), Act::Close),
            (Stage::Dialing, Fatal(Ended::NoAnswer), 100, Stage::Over(Ended::NoAnswer), Act::Close),
            (Stage::Admitting { since_ms: 0 }, Head(200), 50, Stage::Ringing { since_ms: 50 }, Act::Nothing),
            (Stage::Admitting { since_ms: 0 }, Head(204), 50, Stage::Ringing { since_ms: 50 }, Act::Nothing),
            (Stage::Admitting { since_ms: 0 }, Head(403), 50, ending(Ended::Refused(403), 50), Act::EndCall),
            (Stage::Admitting { since_ms: 0 }, QUIET, 29_999, Stage::Admitting { since_ms: 0 }, Act::Nothing),
            (Stage::Admitting { since_ms: 0 }, QUIET, 30_000, ending(Ended::NoAnswer, 30_000), Act::EndCall),
            (Stage::Admitting { since_ms: 0 }, Ready, 50, Stage::Admitting { since_ms: 0 }, Act::Nothing),
            (Stage::Ringing { since_ms: 1000 }, Ready, 2000, Stage::Live { since_ms: 2000, quiet_since_ms: 2000 }, Act::Nothing),
            (Stage::Ringing { since_ms: 1000 }, BUSY, 40_999, Stage::Ringing { since_ms: 1000 }, Act::Nothing),
            (Stage::Ringing { since_ms: 1000 }, QUIET, 41_000, ending(Ended::NoAnswer, 41_000), Act::EndCall),
            (Stage::Ringing { since_ms: 0 }, Fatal(Ended::Failed), 9, ending(Ended::Failed, 9), Act::EndCall),
            (Stage::Ringing { since_ms: 0 }, Stop, 9, ending(Ended::Goodbye, 9), Act::EndCall),
            (Stage::Ringing { since_ms: 0 }, HangUp, 9, ending(Ended::HungUp, 9), Act::EndCall),
            (Stage::Ringing { since_ms: 0 }, MediaEnded { reset: false }, 9, Stage::Ending { why: Ended::Failed, sent_ms: 9, ended: true }, Act::EndCall),
            (live, Ready, 9, ending(Ended::Failed, 9), Act::EndCall),
            (live, Stop, 10, Stage::Goodbye { until_ms: 5010 }, Act::Nothing),
            (live, HangUp, 9, ending(Ended::HungUp, 9), Act::EndCall),
            (live, Fatal(Ended::Failed), 9, ending(Ended::Failed, 9), Act::EndCall),
            (live, MediaEnded { reset: false }, 9, Stage::Ending { why: Ended::Dropped, sent_ms: 9, ended: true }, Act::EndCall),
            (live, MediaEnded { reset: true }, 9, Stage::Ending { why: Ended::Failed, sent_ms: 9, ended: true }, Act::EndCall),
            (live, QUIET, 44_999, live, Act::Nothing),
            (live, QUIET, 45_000, ending(Ended::Idle, 45_000), Act::EndCall),
            (live, BUSY, 60_000, Stage::Live { since_ms: 0, quiet_since_ms: 60_000 }, Act::Nothing),
            (Stage::Live { since_ms: 0, quiet_since_ms: 10 }, Activity, 500, Stage::Live { since_ms: 0, quiet_since_ms: 500 }, Act::Nothing),
            (Stage::Live { since_ms: 0, quiet_since_ms: 899_000 }, BUSY, 900_000, ending(Ended::TooLong, 900_000), Act::EndCall),
            (Stage::Goodbye { until_ms: 5000 }, Tick { busy: false, queued: 100 }, 4999, Stage::Goodbye { until_ms: 5000 }, Act::Nothing),
            (Stage::Goodbye { until_ms: 5000 }, QUIET, 1000, ending(Ended::Goodbye, 1000), Act::EndCall),
            (Stage::Goodbye { until_ms: 5000 }, Tick { busy: true, queued: 100 }, 5000, ending(Ended::Goodbye, 5000), Act::EndCall),
            (Stage::Goodbye { until_ms: 5000 }, HangUp, 9, ending(Ended::Goodbye, 9), Act::EndCall),
            (Stage::Goodbye { until_ms: 5000 }, Stop, 9, Stage::Goodbye { until_ms: 5000 }, Act::Nothing),
            (Stage::Goodbye { until_ms: 5000 }, MediaEnded { reset: false }, 9, Stage::Ending { why: Ended::Goodbye, sent_ms: 9, ended: true }, Act::EndCall),
            (ending(Ended::HungUp, 0), Answered, 100, Stage::Closing { why: Ended::HungUp, since_ms: 100 }, Act::HalfClose),
            (ending(Ended::HungUp, 0), QUIET, 9_999, ending(Ended::HungUp, 0), Act::Nothing),
            (ending(Ended::HungUp, 0), QUIET, 10_000, Stage::Closing { why: Ended::HungUp, since_ms: 10_000 }, Act::HalfClose),
            (ending(Ended::HungUp, 0), MediaEnded { reset: false }, 100, Stage::Ending { why: Ended::HungUp, sent_ms: 0, ended: true }, Act::Nothing),
            (Stage::Ending { why: Ended::HungUp, sent_ms: 0, ended: true }, Answered, 200, Stage::Over(Ended::HungUp), Act::Close),
            (Stage::Ending { why: Ended::Idle, sent_ms: 0, ended: true }, QUIET, 10_000, Stage::Over(Ended::Idle), Act::Close),
            (ending(Ended::Idle, 0), HangUp, 9, ending(Ended::Idle, 0), Act::Nothing),
            (ending(Ended::Idle, 0), Fatal(Ended::Failed), 9, ending(Ended::Idle, 0), Act::Nothing),
            (Stage::Closing { why: Ended::HungUp, since_ms: 0 }, MediaEnded { reset: false }, 10, Stage::Over(Ended::HungUp), Act::Close),
            (Stage::Closing { why: Ended::HungUp, since_ms: 0 }, QUIET, 4_999, Stage::Closing { why: Ended::HungUp, since_ms: 0 }, Act::Nothing),
            (Stage::Closing { why: Ended::HungUp, since_ms: 0 }, QUIET, 5_000, Stage::Over(Ended::HungUp), Act::Reset),
            (Stage::Closing { why: Ended::HungUp, since_ms: 0 }, HangUp, 9, Stage::Closing { why: Ended::HungUp, since_ms: 0 }, Act::Nothing),
            (live, Lost, 9, Stage::Over(Ended::Lost), Act::Nothing),
            (Stage::Ringing { since_ms: 0 }, Lost, 9, Stage::Over(Ended::Lost), Act::Nothing),
            (ending(Ended::HungUp, 0), Lost, 9, Stage::Over(Ended::HungUp), Act::Nothing),
            (Stage::Over(Ended::Idle), HangUp, 9, Stage::Over(Ended::Idle), Act::Nothing),
        ];
        let limits = Limits::default();
        for (stage, input, now_ms, next, act) in rows {
            assert_eq!(stage.on(input, now_ms, &limits), (next, act), "{stage:?} on {input:?} at {now_ms} ms");
        }
    }

    fn any_input(dice: &mut Dice) -> Input {
        let why = [Ended::Failed, Ended::NoAnswer, Ended::HungUp][dice.below(3)];
        match dice.below(13) {
            0 => Dialed,
            1 => Head([200, 204, 403, 500][dice.below(4)]),
            2 => Ready,
            3 => Stop,
            4 => HangUp,
            5 => Fatal(why),
            6 => MediaEnded { reset: dice.below(2) == 0 },
            7 => Answered,
            8 => Activity,
            9 if dice.below(20) == 0 => Lost,
            _ => Tick { busy: dice.below(2) == 0, queued: dice.below(2) * 4800 },
        }
    }

    #[test]
    fn random_calls_keep_the_teardown_order_and_end_for_good() {
        let limits = Limits::default();
        let mut dice = Dice(0x2545_F491_4F6C_DD1D);
        let mut lengths = Vec::new();
        for _ in 0..3000 {
            let (mut stage, mut now_ms) = (Stage::Dialing, 0u64);
            let (mut end_call_ms, mut answered, mut was_up) = (None, false, false);
            for step in 0..80 {
                let input = any_input(&mut dice);
                now_ms += [0, 1, 400, 4_000, 20_000, 200_000][dice.below(6)];
                let (next, act) = stage.on(input, now_ms, &limits);
                if let Stage::Over(why) = stage {
                    assert_eq!((next, act), (Stage::Over(why), Act::Nothing), "Over absorbs {input:?}");
                }
                if matches!(stage, Stage::Ending { .. }) && input == Answered {
                    answered = true;
                }
                match act {
                    Act::EndCall => end_call_ms = Some(now_ms),
                    Act::HalfClose => {
                        let sent_ms = end_call_ms.expect("a half-close comes after end-call");
                        assert!(answered || now_ms - sent_ms >= limits.end_call_ms, "a half-close waits for end-call's answer or its time");
                    }
                    _ => {}
                }
                was_up |= matches!(stage, Stage::Ringing { .. } | Stage::Live { .. });
                if !matches!(stage, Stage::Over(_)) && matches!(next, Stage::Over(_)) {
                    assert!(input == Lost || matches!(act, Act::Close | Act::Reset), "{stage:?} to {next:?} closes the connection");
                    assert!(!was_up || end_call_ms.is_some() || input == Lost, "{stage:?} on {input:?}: an admitted call ends through end-call");
                    lengths.push(step);
                }
                stage = next;
            }
        }
        assert!(lengths.len() > 2000, "most random calls end: {}", lengths.len());
    }
}
