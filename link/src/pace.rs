//! When to try again: the wait between rounds, and how a VM's front door is
//! asked again after it answers 403.

const FIRST_WAIT_MS: u64 = 2_000;
const LONGEST_WAIT_MS: u64 = 60_000;
/// The least wait after a VM refuses the bearer.
pub(crate) const REFUSED_WAIT_MS: u64 = 15_000;

const FASTEST_KNOCK_MS: i64 = 100;
const SLOWEST_KNOCK_MS: i64 = 5_000;
const MOST_KNOCKS: i64 = 80;
/// A door that stayed shut through a whole spell of knocking is shut for a
/// reason knocking does not change, so the next spell waits this long.
const KNOCK_REST_MS: u64 = 10 * 60_000;

/// The wait between rounds: doubling from two seconds to a minute, and never
/// under the floor.
#[derive(Default)]
pub(crate) struct Backoff {
    failures: u32,
    floor_ms: u64,
}

impl Backoff {
    pub(crate) fn next_ms(&mut self) -> u64 {
        let wait = if self.failures < 16 { (FIRST_WAIT_MS << self.failures).min(LONGEST_WAIT_MS) } else { LONGEST_WAIT_MS };
        self.failures = self.failures.saturating_add(1);
        wait.max(self.floor_ms)
    }

    pub(crate) fn reset(&mut self) {
        *self = Backoff::default();
    }

    pub(crate) fn at_least(&mut self, floor_ms: u64) {
        self.floor_ms = floor_ms;
    }
}

/// How the API says to ask a refusing door again.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Knock {
    pub every_ms: u32,
    pub times: u32,
}

impl Knock {
    /// Takes the API's word only so far: eighty times at most, and never
    /// faster than ten times a second.
    pub(crate) fn from_hints(retry_after_ms: i64, max_retry_count: i64) -> Knock {
        let times = max_retry_count.clamp(0, MOST_KNOCKS) as u32;
        let every_ms = if times > 0 { retry_after_ms.clamp(FASTEST_KNOCK_MS, SLOWEST_KNOCK_MS) as u32 } else { 0 };
        Knock { every_ms, times }
    }
}

/// Remembers the last spell of knocking, so a door refusing for a real reason
/// is not asked eighty times every round.
#[derive(Default)]
pub(crate) struct Door {
    knocked_at_ms: Option<u64>,
}

impl Door {
    /// How many times this round may knock.
    pub(crate) fn allowance(&self, knock: Knock, now_ms: u64) -> u32 {
        match self.knocked_at_ms {
            Some(at) if now_ms.saturating_sub(at) < KNOCK_REST_MS => 0,
            _ => knock.times,
        }
    }

    pub(crate) fn knocked(&mut self, now_ms: u64) {
        self.knocked_at_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_double_to_a_minute() {
        let mut backoff = Backoff::default();
        let waits: Vec<u64> = (0..7).map(|_| backoff.next_ms()).collect();
        assert_eq!(waits, [2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000]);
        for _ in 0..100 {
            assert_eq!(backoff.next_ms(), 60_000);
        }
        backoff.reset();
        assert_eq!(backoff.next_ms(), 2_000);
    }

    #[test]
    fn a_refusal_holds_the_wait_up_until_a_healthy_session() {
        let mut backoff = Backoff::default();
        backoff.at_least(REFUSED_WAIT_MS);
        assert_eq!([backoff.next_ms(), backoff.next_ms(), backoff.next_ms(), backoff.next_ms()], [15_000, 15_000, 15_000, 16_000]);
        backoff.reset();
        assert_eq!(backoff.next_ms(), 2_000);
    }

    #[test]
    fn knocks_follow_the_api_within_limits() {
        assert_eq!(Knock::from_hints(500, 20), Knock { every_ms: 500, times: 20 });
        assert_eq!(Knock::from_hints(1, 1000), Knock { every_ms: 100, times: 80 });
        assert_eq!(Knock::from_hints(60_000, 3), Knock { every_ms: 5_000, times: 3 });
        assert_eq!(Knock::from_hints(500, 0), Knock { every_ms: 0, times: 0 });
        assert_eq!(Knock::from_hints(-5, -1), Knock { every_ms: 0, times: 0 });
    }

    #[test]
    fn one_spell_of_knocking_then_a_rest() {
        let knock = Knock::from_hints(500, 20);
        let mut door = Door::default();
        assert_eq!(door.allowance(knock, 1_000), 20);
        door.knocked(1_000);
        assert_eq!(door.allowance(knock, 1_000 + KNOCK_REST_MS - 1), 0);
        assert_eq!(door.allowance(knock, 1_000 + KNOCK_REST_MS), 20);
    }
}
