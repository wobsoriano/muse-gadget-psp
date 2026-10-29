//! Which of Muse's reply audio may play. After a press the reply that was
//! playing never comes back, and only a reply to speech after it plays. A
//! port of the SDK's voice_gate.py with fixed bounds: a table that would
//! overflow fails the call with `Full` instead of evicting, since an evicted
//! id would look new and could play again.

use super::record::{Down, Id};
use alloc::vec::Vec;

const MOST_IDS: usize = 128;
const MOST_TURNS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Reply {
    Stale,
    Waiting,
    Playing,
    /// Plays without a turn_started of its own.
    Continuing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Verdict {
    Drop,
    /// Keep it until the wait ends. Then it plays if `playing()`, else it is
    /// dropped.
    Hold,
    Play,
}

/// What to throw away now.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct Flush {
    /// Queued playback and held audio.
    pub playing: bool,
    /// Held audio only.
    pub waiting: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Full;

pub(crate) struct Gate {
    /// Every provider response id of the call, with what its audio does.
    ids: Vec<(Id, Reply)>,
    /// Every caller input item speech started on.
    items: Vec<Id>,
    fenced: bool,
    /// The current input item.
    item: Option<Id>,
    /// A turn_started named the current item.
    confirmed: bool,
    /// A later turn_started named another item.
    blocked: bool,
    /// The next new id is the confirming turn's own reply.
    turn_fresh: bool,
    flush: Flush,
    /// The response.created with no done yet, which a press cancels.
    open_response: Option<Id>,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate {
            ids: Vec::new(),
            items: Vec::new(),
            fenced: false,
            item: None,
            confirmed: false,
            blocked: false,
            turn_fresh: false,
            flush: Flush::default(),
            open_response: None,
        }
    }

    /// The talk button went down: every id so far is stale, and no item is
    /// current. Gives back the open response, to cancel.
    pub(crate) fn press(&mut self) -> Option<Id> {
        let open = self.open_response.take();
        self.interrupt(None);
        open
    }

    pub(crate) fn event(&mut self, down: &Down) -> Result<(), Full> {
        match down {
            // A late or repeated start cannot make an older caller current.
            Down::SpeechStarted(Some(item)) if self.items.contains(item) => {}
            Down::SpeechStarted(item) => {
                if let Some(item) = item {
                    if self.items.len() == MOST_IDS {
                        return Err(Full);
                    }
                    self.items.push(item.clone());
                }
                self.interrupt(item.clone());
            }
            Down::TurnStarted { item, .. } => match self.item.as_ref().map(|current| current == item) {
                None => {}
                Some(false) => {
                    self.flush.waiting |= self.waiting();
                    self.flush.playing |= self.has(&[Reply::Continuing]);
                    self.replace(&[Reply::Waiting, Reply::Continuing], Reply::Stale);
                    self.blocked = true;
                    self.turn_fresh = false;
                }
                Some(true) => {
                    let had_reply = self.waiting() || self.playing();
                    self.replace(&[Reply::Waiting, Reply::Continuing], Reply::Playing);
                    self.confirmed = true;
                    self.blocked = false;
                    self.turn_fresh = !had_reply;
                }
            },
            Down::InputSuppressed(Some(item)) if self.item.as_ref() == Some(item) => self.interrupt(None),
            Down::ResponseCreated(Some(id)) => self.open_response = Some(id.clone()),
            Down::ResponseDone(Some(id)) if self.open_response.as_ref() == Some(id) => self.open_response = None,
            _ => {}
        }
        Ok(())
    }

    /// What to do with an audio event's samples.
    pub(crate) fn audio(&mut self, response_id: Option<&Id>) -> Result<Verdict, Full> {
        let Some(id) = response_id else {
            return Ok(if self.fenced { Verdict::Drop } else { Verdict::Play });
        };
        if let Some((_, reply)) = self.ids.iter().find(|(known, _)| known == id) {
            return Ok(verdict(*reply));
        }
        if self.ids.len() == MOST_IDS {
            return Err(Full);
        }
        let reply = if !self.fenced {
            Reply::Playing
        } else if self.item.is_none() || self.blocked {
            Reply::Stale
        } else if self.confirmed {
            // A newer reply takes over: two never interleave.
            self.flush.playing |= self.playing();
            self.replace(&[Reply::Playing, Reply::Continuing], Reply::Stale);
            let reply = if self.turn_fresh { Reply::Playing } else { Reply::Continuing };
            self.turn_fresh = false;
            reply
        } else {
            self.flush.waiting |= self.waiting();
            self.replace(&[Reply::Waiting], Reply::Stale);
            Reply::Waiting
        };
        self.ids.push((id.clone(), reply));
        Ok(verdict(reply))
    }

    /// Some id's audio waits for the current item's turn.
    pub(crate) fn waiting(&self) -> bool {
        self.has(&[Reply::Waiting])
    }

    /// Some id's audio plays.
    pub(crate) fn playing(&self) -> bool {
        self.has(&[Reply::Playing, Reply::Continuing])
    }

    /// The flush raised since the last call, cleared.
    pub(crate) fn take_flush(&mut self) -> Flush {
        core::mem::take(&mut self.flush)
    }

    /// The held audio outgrew its room: what waits is stale.
    pub(crate) fn overflow(&mut self) {
        self.replace(&[Reply::Waiting], Reply::Stale);
    }

    fn interrupt(&mut self, item: Option<Id>) {
        self.fenced = true;
        self.replace(&[Reply::Waiting, Reply::Playing, Reply::Continuing], Reply::Stale);
        self.item = item;
        self.confirmed = false;
        self.blocked = false;
        self.turn_fresh = false;
        self.open_response = None;
        self.flush = Flush { playing: true, waiting: true };
    }

    fn has(&self, replies: &[Reply]) -> bool {
        self.ids.iter().any(|(_, reply)| replies.contains(reply))
    }

    fn replace(&mut self, replies: &[Reply], with: Reply) {
        for (_, reply) in &mut self.ids {
            if replies.contains(reply) {
                *reply = with;
            }
        }
    }
}

fn verdict(reply: Reply) -> Verdict {
    match reply {
        Reply::Waiting => Verdict::Hold,
        Reply::Stale => Verdict::Drop,
        Reply::Playing | Reply::Continuing => Verdict::Play,
    }
}

/// The user's turns Muse is working on, for the idle clock and for
/// `CallState::Live { working }`.
pub(crate) struct Turns {
    /// Submission and caller input item, oldest first.
    active: Vec<(Id, Id)>,
    /// The latest settled turns, so a replayed turn_started is not new work.
    settled: Vec<(Id, Id)>,
    next_settled: usize,
}

impl Turns {
    pub(crate) fn new() -> Turns {
        Turns { active: Vec::new(), settled: Vec::new(), next_settled: 0 }
    }

    /// A turn Muse started. A replay with the same item is not new work. A
    /// submission seen before with another item, or a 17th active turn, is
    /// `Full`.
    pub(crate) fn started(&mut self, submission: &Id, item: &Id) -> Result<(), Full> {
        if let Some((_, seen)) = self.active.iter().chain(&self.settled).find(|(known, _)| known == submission) {
            return if seen == item { Ok(()) } else { Err(Full) };
        }
        if self.active.len() == MOST_TURNS {
            return Err(Full);
        }
        self.active.push((submission.clone(), item.clone()));
        Ok(())
    }

    /// Retires that submission and the earlier ones for its item, never
    /// another item's or a later alias. False when it names no active turn.
    pub(crate) fn settled(&mut self, response_message: &Id) -> bool {
        let Some(at) = self.active.iter().position(|(submission, _)| submission == response_message) else { return false };
        let item = self.active[at].1.clone();
        let (settled, next) = (&mut self.settled, &mut self.next_settled);
        let mut index = 0;
        self.active.retain(|turn| {
            let retired = index <= at && turn.1 == item;
            index += 1;
            if retired {
                if settled.len() < MOST_TURNS {
                    settled.push(turn.clone());
                } else {
                    settled[*next] = turn.clone();
                }
                *next = (*next + 1) % MOST_TURNS;
            }
            !retired
        });
        true
    }

    pub(crate) fn any(&self) -> bool {
        !self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::id;
    use super::*;
    use alloc::format;
    use Verdict::{Drop, Hold, Play};

    fn audio(gate: &mut Gate, response: &str) -> Verdict {
        gate.audio(Some(&id(response))).unwrap()
    }

    fn speech(gate: &mut Gate, item: &str) {
        gate.event(&Down::SpeechStarted(Some(id(item)))).unwrap();
    }

    fn turn(gate: &mut Gate, item: &str, submission: &str) {
        gate.event(&Down::TurnStarted { submission: id(submission), item: id(item) }).unwrap();
    }

    #[test]
    fn interrupted_ids_never_reopen_and_a_held_prefix_plays_once_confirmed() {
        let mut gate = Gate::new();
        assert_eq!(audio(&mut gate, "old"), Play, "unfenced audio plays");
        assert_eq!(gate.press(), None);
        assert_eq!(audio(&mut gate, "old"), Drop);
        assert_eq!(audio(&mut gate, "before-speech"), Drop, "no item after a press");
        speech(&mut gate, "current");
        assert_eq!(audio(&mut gate, "pending"), Hold, "a reply before its turn waits");
        assert!(gate.waiting() && !gate.playing());
        turn(&mut gate, "current", "turn");
        assert!(!gate.waiting() && gate.playing(), "the held audio is released to play");
        assert_eq!(audio(&mut gate, "pending"), Play);
        assert_eq!(audio(&mut gate, "old"), Drop);
        assert_eq!(audio(&mut gate, "before-speech"), Drop);
    }

    #[test]
    fn a_continuation_plays_and_another_items_turn_stales_and_blocks() {
        let mut gate = Gate::new();
        speech(&mut gate, "current");
        turn(&mut gate, "current", "turn");
        assert_eq!(audio(&mut gate, "own"), Play);
        gate.take_flush();
        assert_eq!(audio(&mut gate, "continuation"), Play, "no alias turn is needed");
        assert_eq!(gate.take_flush(), Flush { playing: true, waiting: false }, "the newer reply takes over");
        assert_eq!(audio(&mut gate, "own"), Drop);
        turn(&mut gate, "older", "late");
        assert!(gate.take_flush().playing, "the continuation was the older work's");
        assert_eq!(audio(&mut gate, "continuation"), Drop);
        assert_eq!(audio(&mut gate, "foreign"), Drop, "blocked");
        turn(&mut gate, "current", "alias");
        assert_eq!(audio(&mut gate, "fresh"), Play);
        assert_eq!(audio(&mut gate, "foreign"), Drop);
    }

    #[test]
    fn a_foreign_turn_drops_an_unconfirmed_prefix_but_not_a_confirmed_reply() {
        let mut gate = Gate::new();
        speech(&mut gate, "current");
        assert_eq!(audio(&mut gate, "pending"), Hold);
        turn(&mut gate, "older", "late");
        assert!(!gate.waiting());
        assert_eq!(audio(&mut gate, "pending"), Drop);
        turn(&mut gate, "current", "turn");
        assert_eq!(audio(&mut gate, "current"), Play);
        turn(&mut gate, "older", "another-late");
        assert_eq!(audio(&mut gate, "current"), Play, "its own turn named it");
        assert_eq!(audio(&mut gate, "new-unattributed"), Drop);
    }

    #[test]
    fn missing_lineage_cannot_unmute() {
        let mut gate = Gate::new();
        gate.press();
        gate.event(&Down::SpeechStarted(None)).unwrap();
        turn(&mut gate, "current", "turn");
        assert_eq!(audio(&mut gate, "unattributed"), Drop, "speech on no item confirms nothing");
        speech(&mut gate, "current");
        turn(&mut gate, "current", "turn");
        assert_eq!(gate.audio(None), Ok(Drop), "audio naming no response, once fenced");
    }

    #[test]
    fn an_old_speech_start_or_suppression_cannot_reselect_an_old_caller() {
        let mut gate = Gate::new();
        speech(&mut gate, "old");
        speech(&mut gate, "current");
        speech(&mut gate, "old");
        turn(&mut gate, "current", "turn");
        assert_eq!(audio(&mut gate, "own"), Play, "current is still the current item");
        gate.event(&Down::InputSuppressed(Some(id("old")))).unwrap();
        assert_eq!(audio(&mut gate, "own"), Play, "suppressing another item changes nothing");
        gate.event(&Down::InputSuppressed(Some(id("current")))).unwrap();
        assert_eq!(audio(&mut gate, "own"), Drop);
        assert_eq!(audio(&mut gate, "next"), Drop, "no item is current");
    }

    #[test]
    fn ids_and_items_are_bounded_for_the_whole_call_without_eviction() {
        let mut gate = Gate::new();
        for index in 0..MOST_IDS {
            audio(&mut gate, &format!("{index}"));
            gate.press();
        }
        assert_eq!(audio(&mut gate, "0"), Drop);
        assert_eq!(gate.audio(Some(&id("overflow"))), Err(Full));
        for index in 0..MOST_IDS {
            speech(&mut gate, &format!("{index}"));
        }
        assert_eq!(gate.event(&Down::SpeechStarted(Some(id("overflow")))), Err(Full));
    }

    #[test]
    fn a_press_cancels_only_the_open_response() {
        let mut gate = Gate::new();
        audio(&mut gate, "provider");
        gate.event(&Down::ResponseCreated(Some(id("mapper")))).unwrap();
        gate.event(&Down::ResponseDone(Some(id("older-mapper")))).unwrap();
        assert_eq!(audio(&mut gate, "provider"), Play);
        assert_eq!(gate.press(), Some(id("mapper")), "still open");
        assert_eq!(gate.take_flush(), Flush { playing: true, waiting: true });
        assert_eq!(gate.press(), None, "a cancel is asked for once");

        gate.event(&Down::ResponseCreated(Some(id("next")))).unwrap();
        gate.event(&Down::ResponseDone(Some(id("next")))).unwrap();
        assert_eq!(gate.press(), None, "a finished response is not cancelled");
    }

    #[test]
    fn speech_on_a_new_item_flushes_and_overflow_stales_what_waits() {
        let mut gate = Gate::new();
        assert_eq!(audio(&mut gate, "playing"), Play);
        gate.take_flush();
        speech(&mut gate, "current");
        assert_eq!(gate.take_flush(), Flush { playing: true, waiting: true }, "the server's own barge-in");
        assert_eq!(audio(&mut gate, "pending"), Hold);
        gate.overflow();
        assert!(!gate.waiting());
        assert_eq!(audio(&mut gate, "pending"), Drop);
        turn(&mut gate, "current", "turn");
        assert_eq!(audio(&mut gate, "after"), Play, "a later reply still plays");
    }

    #[test]
    fn turns_settle_earlier_aliases_of_one_item_and_fail_closed() {
        let mut turns = Turns::new();
        assert!(!turns.any());
        turns.started(&id("m1"), &id("i1")).unwrap();
        turns.started(&id("m2"), &id("i2")).unwrap();
        turns.started(&id("m3"), &id("i1")).unwrap();
        assert_eq!(turns.started(&id("m1"), &id("i1")), Ok(()), "a replay");
        assert_eq!(turns.started(&id("m1"), &id("i2")), Err(Full), "the same submission for another item");
        assert!(!turns.settled(&id("unknown")));
        assert!(turns.settled(&id("m1")));
        assert!(turns.any(), "the later alias m3 and the other item's m2 are still worked on");
        assert!(turns.settled(&id("m3")));
        assert!(turns.any(), "m2 belongs to another item");
        assert!(turns.settled(&id("m2")));
        assert!(!turns.any());
        assert_eq!(turns.started(&id("m3"), &id("i1")), Ok(()), "a replay of a settled turn");
        assert!(!turns.any(), "is not new work");
        assert_eq!(turns.started(&id("m3"), &id("i9")), Err(Full));

        let mut turns = Turns::new();
        for index in 0..MOST_TURNS {
            turns.started(&id(&format!("m{index}")), &id("i")).unwrap();
        }
        assert_eq!(turns.started(&id("m-extra"), &id("i")), Err(Full), "a 17th active turn");
        assert!(turns.settled(&id("m5")), "settles m0 to m5");
        turns.started(&id("m-extra"), &id("i")).unwrap();
    }
}
