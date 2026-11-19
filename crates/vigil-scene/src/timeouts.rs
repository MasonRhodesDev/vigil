use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rustix::time::{ClockId, clock_gettime};

use crate::helper::MAX_DEADLINE;
use crate::{Error, Format, Request};

/// How many timeouts at a striking deadline one logo and spec may cost at
/// one size class and layer before the library answers `Error::TooSlow` for them,
/// without running the helper. A timeout of a call that began less than
/// `STRIKES_APART` after the last strike ended is the same slow moment, such
/// as a stall on resume that times out one output after another, and does
/// not count again.
pub const TIMEOUTS_BEFORE_TOO_SLOW: u32 = 2;
/// The shortest deadline whose timeout counts as a strike. A timeout at a
/// shorter one says only that the caller chose a short deadline.
pub const STRIKING_DEADLINE: Duration =
    Duration::from_millis(MAX_DEADLINE.as_millis() as u64 * 9 / 10);
/// How long after a strike ended the next timeout must start to count.
pub const STRIKES_APART: Duration = Duration::from_secs(60);
/// A strike this old no longer counts.
pub const STRIKES_LAST: Duration = Duration::from_secs(10 * 60);
/// How long a logo found too slow is skipped before the helper tries it again.
pub const TOO_SLOW_LASTS: Duration = Duration::from_secs(60 * 60);

const REMEMBERED: usize = 64;

/// A logo and spec at one size class: the log2 of the area drawn, so a logo
/// too slow for an 8K panel is still tried on a 1080p one. A drawn layer
/// keys on its layer too, so a glow that times out costs the logo nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    request: u64,
    size_class: u32,
}

static SEED: LazyLock<RandomState> = LazyLock::new(RandomState::new);

impl Key {
    pub(crate) fn layer(req: &Request<'_>) -> Key {
        Key::new(
            &[
                Format::Svg.name().as_bytes(),
                req.layer.name().as_bytes(),
                req.svg.unwrap_or_default(),
                req.spec.unwrap_or_default(),
            ],
            u64::from(req.mode.0) * u64::from(req.mode.1),
        )
    }

    pub(crate) fn new(parts: &[&[u8]], area: u64) -> Key {
        Key {
            request: SEED.hash_one(parts),
            size_class: area.checked_ilog2().unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    strikes: u32,
    last: Duration,
    too_slow_since: Option<Duration>,
}

impl Entry {
    fn too_slow(&self, now: Duration) -> bool {
        self.too_slow_since
            .is_some_and(|since| now.saturating_sub(since) < TOO_SLOW_LASTS)
    }

    fn live(&self, now: Duration) -> bool {
        self.too_slow(now)
            || (self.too_slow_since.is_none() && now.saturating_sub(self.last) < STRIKES_LAST)
    }
}

/// The strikes against each key. Times are on one clock that never goes
/// back, passed in so the rule is testable without waiting an hour.
#[derive(Default)]
pub(crate) struct Table {
    entries: HashMap<Key, Entry>,
}

impl Table {
    pub(crate) fn check(&mut self, key: Key, now: Duration) -> Result<(), Error> {
        match self.entries.get(&key) {
            Some(entry) if entry.too_slow(now) => Err(too_slow()),
            Some(entry) if !entry.live(now) => {
                self.entries.remove(&key);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn settle(
        &mut self,
        key: Key,
        deadline: Duration,
        started: Duration,
        now: Duration,
        got: Result<(), Error>,
    ) -> Result<(), Error> {
        match got {
            Ok(()) => {
                self.entries.remove(&key);
                Ok(())
            }
            Err(Error::Timeout(why)) if deadline >= STRIKING_DEADLINE => {
                if self.strike(key, started, now) {
                    Err(too_slow())
                } else {
                    Err(Error::Timeout(why))
                }
            }
            Err(e) => Err(e),
        }
    }

    // Says whether the key is now too slow.
    fn strike(&mut self, key: Key, started: Duration, now: Duration) -> bool {
        if !self.entries.contains_key(&key) && self.entries.len() >= REMEMBERED {
            self.make_room(now);
        }
        let fresh = Entry {
            strikes: 0,
            last: started,
            too_slow_since: None,
        };
        let entry = self.entries.entry(key).or_insert(fresh);
        if !entry.live(now) {
            *entry = fresh;
        }
        if !entry.too_slow(now)
            && (entry.strikes == 0 || started >= entry.last.saturating_add(STRIKES_APART))
        {
            entry.strikes += 1;
            entry.last = now;
            if entry.strikes >= TIMEOUTS_BEFORE_TOO_SLOW {
                entry.too_slow_since = Some(now);
            }
        }
        entry.too_slow(now)
    }

    // Stale entries go first, then the oldest strikes, and a logo found too
    // slow only when every entry is one.
    fn make_room(&mut self, now: Duration) {
        self.entries.retain(|_, e| e.live(now));
        if self.entries.len() >= REMEMBERED
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| (e.too_slow(now), e.last))
                .map(|(k, _)| *k)
        {
            self.entries.remove(&oldest);
        }
    }
}

fn too_slow() -> Error {
    Error::TooSlow(format!(
        "the logo timed out {TIMEOUTS_BEFORE_TOO_SLOW} times at this size, so it is skipped for a while"
    ))
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(Default::default);

fn table() -> MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// CLOCK_BOOTTIME, which runs through suspend, so a strike before a suspend
/// is as old after resume as the wall clock says, and cannot pair with a
/// load spike on resume.
pub(crate) fn now() -> Duration {
    let t = clock_gettime(ClockId::Boottime);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

pub(crate) fn check(key: Key) -> Result<(), Error> {
    table().check(key, now())
}

pub(crate) fn settle(
    key: Key,
    deadline: Duration,
    started: Duration,
    got: Result<(), Error>,
) -> Result<(), Error> {
    table().settle(key, deadline, started, now(), got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Layer;

    const LONGEST: Duration = MAX_DEADLINE;
    const BRIEF: Duration = Duration::from_millis(500);

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn logo(n: u8) -> Key {
        Key::new(&[b"svg", &[n]], 1920 * 1080)
    }

    fn timeout() -> Result<(), Error> {
        Err(Error::Timeout("stopped".into()))
    }

    // A call that starts at `at` and times out at its deadline.
    fn time_out(t: &mut Table, key: Key, deadline: Duration, at: Duration) -> Result<(), Error> {
        t.check(key, at)?;
        t.settle(key, deadline, at, at + deadline, timeout())
    }

    fn is_timeout(got: Result<(), Error>) -> bool {
        matches!(got, Err(Error::Timeout(_)))
    }

    fn is_too_slow(got: Result<(), Error>) -> bool {
        matches!(got, Err(Error::TooSlow(_)))
    }

    // The second strike of a logo whose first began at 0.
    const AGAIN: Duration = LONGEST.saturating_add(STRIKES_APART);

    #[test]
    fn two_timeouts_at_the_longest_deadline_a_minute_apart_make_a_logo_too_slow() {
        let mut t = Table::default();
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, secs(0))));
        assert!(is_too_slow(time_out(&mut t, logo(1), LONGEST, AGAIN)));
        assert!(
            is_too_slow(t.check(logo(1), AGAIN + secs(10))),
            "skipped without a run"
        );
        assert!(
            t.check(logo(2), AGAIN + secs(10)).is_ok(),
            "another logo is its own"
        );
    }

    #[test]
    fn a_stall_across_two_outputs_after_resume_is_one_strike() {
        let mut t = Table::default();
        let brief_then_longest = |t: &mut Table, at: Duration| {
            assert!(is_timeout(time_out(t, logo(1), BRIEF, at)));
            time_out(t, logo(1), LONGEST, at + BRIEF)
        };
        assert!(is_timeout(brief_then_longest(&mut t, secs(0))), "output A");
        let b = BRIEF + LONGEST + Duration::from_millis(10);
        assert!(is_timeout(brief_then_longest(&mut t, b)), "output B");
        assert!(t.check(logo(1), b + secs(6)).is_ok(), "still tried");
    }

    #[test]
    fn a_slow_logo_retried_within_the_gap_is_still_marked_after_it() {
        let mut t = Table::default();
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, secs(0))));
        for at in [10, 30, 50, 64] {
            assert!(
                is_timeout(time_out(&mut t, logo(1), LONGEST, secs(at))),
                "{at} s is within the gap"
            );
        }
        assert!(is_too_slow(time_out(&mut t, logo(1), LONGEST, AGAIN)));
    }

    #[test]
    fn a_timeout_at_a_brief_deadline_is_no_strike() {
        let mut t = Table::default();
        for i in 0..10 {
            assert!(is_timeout(time_out(&mut t, logo(1), BRIEF, secs(i))));
        }
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, secs(20))));
        assert!(t.entries.len() == 1, "a brief timeout keeps no entry");
        assert!(is_timeout(time_out(
            &mut t,
            logo(2),
            STRIKING_DEADLINE - Duration::from_millis(1),
            secs(30)
        )));
        assert!(!t.entries.contains_key(&logo(2)));
    }

    #[test]
    fn a_logo_and_its_glow_slow_in_one_moment_are_one_strike_each() {
        let mut t = Table::default();
        let layer = |layer| {
            Key::layer(&Request {
                svg: Some(b"<svg/>"),
                spec: None,
                mode: (1920, 1080),
                transform: 0,
                layer,
                value: 1.0,
            })
        };
        let (logo, glow) = (layer(Layer::Logo), layer(Layer::Glow));
        assert!(is_timeout(time_out(&mut t, logo, LONGEST, secs(0))));
        assert!(is_timeout(time_out(&mut t, glow, LONGEST, LONGEST)));
        assert!(t.check(logo, secs(20)).is_ok(), "the logo is still tried");
    }

    #[test]
    fn a_strike_counts_only_at_its_size_class() {
        let mut t = Table::default();
        let (small, huge) = (
            Key::new(&[b"svg", b"x"], 1920 * 1080),
            Key::new(&[b"svg", b"x"], 7680 * 4320),
        );
        assert!(is_timeout(time_out(&mut t, huge, LONGEST, secs(0))));
        assert!(is_too_slow(time_out(&mut t, huge, LONGEST, AGAIN)));
        assert!(
            t.check(small, AGAIN + secs(10)).is_ok(),
            "the 8K strikes spare 1080p"
        );
        assert!(is_timeout(time_out(
            &mut t,
            small,
            LONGEST,
            AGAIN + secs(10)
        )));
    }

    #[test]
    fn a_drawing_clears_only_its_own_key() {
        let mut t = Table::default();
        let (small, huge) = (
            Key::new(&[b"svg", b"x"], 1920 * 1080),
            Key::new(&[b"svg", b"x"], 7680 * 4320),
        );
        assert!(is_timeout(time_out(&mut t, huge, LONGEST, secs(0))));
        assert!(is_timeout(time_out(&mut t, small, LONGEST, secs(0))));
        t.settle(small, LONGEST, secs(10), secs(11), Ok(()))
            .unwrap();
        assert!(
            is_timeout(time_out(&mut t, small, LONGEST, AGAIN)),
            "cleared"
        );
        assert!(
            is_too_slow(time_out(&mut t, huge, LONGEST, AGAIN)),
            "not cleared"
        );
    }

    #[test]
    fn overlapping_timeouts_are_one_strike() {
        let mut t = Table::default();
        for _ in 0..4 {
            t.check(logo(1), secs(0)).unwrap();
        }
        for end in [5, 5, 6, 7] {
            assert!(is_timeout(t.settle(
                logo(1),
                LONGEST,
                secs(0),
                secs(end),
                timeout()
            )));
        }
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, secs(8))));
        let again = secs(5) + STRIKES_APART;
        assert!(is_too_slow(time_out(&mut t, logo(1), LONGEST, again)));
    }

    #[test]
    fn a_strike_older_than_ten_minutes_is_forgotten() {
        let mut t = Table::default();
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, secs(0))));
        let later = STRIKES_LAST + secs(10);
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, later)));
        assert!(is_too_slow(time_out(
            &mut t,
            logo(1),
            LONGEST,
            later + AGAIN
        )));
    }

    #[test]
    fn a_logo_found_too_slow_is_tried_again_after_an_hour() {
        let mut t = Table::default();
        assert!(is_timeout(time_out(&mut t, logo(1), LONGEST, secs(0))));
        assert!(is_too_slow(time_out(&mut t, logo(1), LONGEST, AGAIN)));
        let found = AGAIN + LONGEST;
        assert!(is_too_slow(
            t.check(logo(1), found + TOO_SLOW_LASTS - secs(1))
        ));
        let after = found + TOO_SLOW_LASTS;
        assert!(t.check(logo(1), after).is_ok(), "tried again");
        assert!(
            is_timeout(time_out(&mut t, logo(1), LONGEST, after)),
            "a fresh count"
        );
    }

    #[test]
    fn a_full_table_keeps_the_logos_found_too_slow() {
        let mut t = Table::default();
        for n in 0..8 {
            assert!(is_timeout(time_out(&mut t, logo(n), LONGEST, secs(0))));
            assert!(is_too_slow(time_out(&mut t, logo(n), LONGEST, AGAIN)));
        }
        for n in 8..=REMEMBERED as u8 + 8 {
            assert!(is_timeout(time_out(
                &mut t,
                logo(n),
                LONGEST,
                AGAIN + secs(10 + u64::from(n))
            )));
        }
        assert!(t.entries.len() <= REMEMBERED);
        for n in 0..8 {
            assert!(
                is_too_slow(t.check(logo(n), AGAIN + secs(200))),
                "logo {n} kept"
            );
        }
    }

    #[test]
    fn a_full_table_of_stale_strikes_makes_room() {
        let mut t = Table::default();
        for n in 0..REMEMBERED as u8 {
            assert!(is_timeout(time_out(&mut t, logo(n), LONGEST, secs(0))));
        }
        let later = STRIKES_LAST + secs(60);
        assert!(is_timeout(time_out(&mut t, logo(200), LONGEST, later)));
        assert_eq!(t.entries.len(), 1, "stale strikes dropped first");
    }
}
