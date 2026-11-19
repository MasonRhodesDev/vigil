use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crate::Error;

/// How many timeouts one logo and spec may cost a process before the
/// library refuses them for the rest of its life without running the
/// helper. A timeout of a call that began before the last one counted is
/// the same slow moment and counts once, and a drawing clears the count.
pub const TIMEOUTS_BEFORE_REFUSAL: u32 = 3;

const REMEMBERED: usize = 64;

#[derive(Default)]
struct Strikes {
    count: u32,
    last: Option<Instant>,
}

static SEED: LazyLock<RandomState> = LazyLock::new(RandomState::new);
static SLOW: LazyLock<Mutex<HashMap<u64, Strikes>>> = LazyLock::new(Default::default);

fn slow() -> MutexGuard<'static, HashMap<u64, Strikes>> {
    SLOW.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refusal(count: u32) -> Error {
    Error::Refused(format!(
        "the logo timed out {count} times, so this process refuses it"
    ))
}

pub(crate) fn key(parts: &[&[u8]]) -> u64 {
    SEED.hash_one(parts)
}

pub(crate) fn refused(key: u64) -> Result<(), Error> {
    match slow().get(&key) {
        Some(s) if s.count >= TIMEOUTS_BEFORE_REFUSAL => Err(refusal(s.count)),
        _ => Ok(()),
    }
}

pub(crate) fn settle(key: u64, started: Instant, got: Result<(), Error>) -> Result<(), Error> {
    let mut slow = slow();
    match got {
        Ok(()) => {
            slow.remove(&key);
            Ok(())
        }
        Err(Error::Timeout(why)) => {
            if slow.len() >= REMEMBERED && !slow.contains_key(&key) {
                slow.clear();
            }
            let strikes = slow.entry(key).or_default();
            if strikes.last.is_none_or(|last| started >= last) {
                strikes.count += 1;
                strikes.last = Some(Instant::now());
            }
            if strikes.count >= TIMEOUTS_BEFORE_REFUSAL {
                return Err(refusal(strikes.count));
            }
            Err(Error::Timeout(why))
        }
        Err(e) => Err(e),
    }
}
