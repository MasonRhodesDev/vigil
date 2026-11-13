//! Pause automatic login when sessions the greeter starts without any
//! user input keep dying right after they start. Otherwise a broken
//! desktop plus a preselected account that needs no credential restarts
//! the greeter forever, with nobody at the keyboard able to break in.

use std::time::{SystemTime, UNIX_EPOCH};

use vigil_config::State;

/// A session that is gone again within this long of its start counts as
/// short-lived.
pub const SHORT_SESSION_SECS: u64 = 30;
/// Consecutive short-lived automatic sessions before automatic login pauses.
pub const PAUSE_AFTER: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Automatic login may proceed; `streak` is what a new automatic start
    /// records.
    Allow { streak: u32 },
    /// Wait for the user. `last_secs` is how long the previous automatic
    /// session lasted when that is known.
    Pause { streak: u32, last_secs: Option<u64> },
}

pub fn assess(state: Option<&State>, now_secs: u64) -> Verdict {
    let Some(state) = state else {
        return Verdict::Allow { streak: 0 };
    };
    let last_secs = state
        .auto_started_at
        .and_then(|at| now_secs.checked_sub(at))
        .filter(|secs| *secs < SHORT_SESSION_SECS);
    if state.auto_streak >= PAUSE_AFTER {
        return Verdict::Pause {
            streak: state.auto_streak,
            last_secs,
        };
    }
    match last_secs {
        Some(_) if state.auto_streak + 1 >= PAUSE_AFTER => Verdict::Pause {
            streak: state.auto_streak + 1,
            last_secs,
        },
        Some(_) => Verdict::Allow {
            streak: state.auto_streak + 1,
        },
        None => Verdict::Allow { streak: 0 },
    }
}

/// State to store when the greeter starts a session without user input.
pub fn record_auto_start(prev: &State, now_secs: u64, streak: u32) -> State {
    State {
        auto_started_at: Some(now_secs),
        auto_streak: streak,
        ..prev.clone()
    }
}

/// State to store when a typed credential or an explicit user action
/// started the session.
pub fn record_user_start(prev: &State) -> State {
    State {
        auto_started_at: None,
        auto_streak: 0,
        ..prev.clone()
    }
}

/// State to store once automatic login pauses, so a later greeter restart
/// stays paused until a user signs in.
pub fn record_pause(prev: &State, streak: u32) -> State {
    State {
        auto_streak: streak,
        ..prev.clone()
    }
}

pub fn notice(last_secs: Option<u64>) -> String {
    match last_secs {
        Some(secs) => format!(
            "The last session ended after {secs} s, so automatic login is paused. Sign in to continue."
        ),
        None => "Recent sessions ended right after starting, so automatic login is paused. Sign in to continue.".to_owned(),
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(user: &str) -> State {
        State {
            user: user.into(),
            session: "DE".into(),
            ..State::default()
        }
    }

    #[test]
    fn no_state_allows() {
        assert_eq!(assess(None, 1_000), Verdict::Allow { streak: 0 });
        assert_eq!(
            assess(Some(&stored("games")), 1_000),
            Verdict::Allow { streak: 0 }
        );
    }

    #[test]
    fn three_short_automatic_sessions_pause_the_fourth_start() {
        let mut now = 1_000;
        let mut state = stored("games");
        for expected in 0..PAUSE_AFTER {
            let verdict = assess(Some(&state), now);
            assert_eq!(verdict, Verdict::Allow { streak: expected });
            let Verdict::Allow { streak } = verdict else {
                unreachable!()
            };
            state = record_auto_start(&state, now, streak);
            now += 7;
        }
        assert_eq!(
            assess(Some(&state), now),
            Verdict::Pause {
                streak: PAUSE_AFTER,
                last_secs: Some(7)
            }
        );
    }

    #[test]
    fn a_long_session_resets_the_streak() {
        let state = record_auto_start(&stored("games"), 1_000, 1);
        assert_eq!(
            assess(Some(&state), 1_000 + SHORT_SESSION_SECS),
            Verdict::Allow { streak: 0 }
        );
        assert_eq!(
            assess(Some(&state), 1_000 + SHORT_SESSION_SECS - 1),
            Verdict::Allow { streak: 2 }
        );
    }

    #[test]
    fn a_clock_that_went_backwards_is_not_short() {
        let state = record_auto_start(&stored("games"), 5_000, 2);
        assert_eq!(assess(Some(&state), 100), Verdict::Allow { streak: 0 });
    }

    #[test]
    fn pause_persists_across_restarts_until_a_user_start() {
        let state = record_auto_start(&stored("games"), 1_000, 2);
        let Verdict::Pause { streak, .. } = assess(Some(&state), 1_005) else {
            panic!("expected a pause");
        };
        let paused = record_pause(&state, streak);
        assert_eq!(
            assess(Some(&paused), 1_005 + 3_600),
            Verdict::Pause {
                streak: PAUSE_AFTER,
                last_secs: None
            }
        );
        let resumed = record_user_start(&paused);
        assert_eq!(resumed.user, "games");
        assert_eq!(
            assess(Some(&resumed), 1_005 + 3_601),
            Verdict::Allow { streak: 0 }
        );
    }

    #[test]
    fn user_start_keeps_the_remembered_choice() {
        let state = record_auto_start(&stored("mason"), 1_000, 1);
        let after = record_user_start(&state);
        assert_eq!(after.user, "mason");
        assert_eq!(after.session, "DE");
        assert_eq!(after.auto_started_at, None);
        assert_eq!(after.auto_streak, 0);
    }

    #[test]
    fn notice_names_the_duration_when_known() {
        assert!(notice(Some(1)).contains("after 1 s"));
        assert!(notice(None).contains("automatic login is paused"));
    }
}
