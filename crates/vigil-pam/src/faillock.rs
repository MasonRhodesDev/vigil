//! Reading the user's own `pam_faillock` tally (issue #92).
//!
//! When faillock has the account locked, `pam_authenticate` returns the same
//! `PAM_AUTH_ERR` it returns for a typo, and the locker showed the same
//! "authentication failed" for both. The user least able to work out why
//! their correct password stopped working is precisely the one it happened
//! to — so vigil reads the tally and says so, with the time left.
//!
//! No privilege is required and none is taken: `/run/faillock/<user>` is
//! `rw-rw---- <user> root`, so the session user reads its own record. This
//! module only ever reads. Resetting a tally is `faillock --reset`, a
//! deliberate act, not something a lockscreen does on the user's behalf.
//!
//! Everything here is advisory. A tally that is missing, short, oversized,
//! unreadable or written by a future version means "not locked, unknown" —
//! never an error that could get between the user and their session.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// linux-pam `modules/pam_faillock/faillock.h`:
///
/// ```c
/// struct tally {
///     char source[52];
///     uint16_t reserved;
///     uint16_t status;
///     uint64_t time;
/// };
/// ```
///
/// Written straight out of memory: 64 bytes per record, host byte order,
/// no header and no version field. Confirmed byte-for-byte against a live
/// `/run/faillock/mason` (see the golden test below).
pub const RECORD_LEN: usize = 64;
const STATUS_OFFSET: usize = 54;
const TIME_OFFSET: usize = 56;
const STATUS_VALID: u16 = 0x1;

/// A tally is a handful of 64-byte records. Anything past this is not one,
/// and vigil will not allocate for it.
const MAX_TALLY_BYTES: u64 = 64 * 1024;

/// `faillock.conf` defaults, as documented in the shipped file: every key is
/// commented out by default, so these are what a stock system runs.
pub const DEFAULT_DENY: u32 = 3;
pub const DEFAULT_UNLOCK_TIME: u64 = 600;
pub const DEFAULT_FAIL_INTERVAL: u64 = 900;
const DEFAULT_DIR: &str = "/var/run/faillock";
const CONF_PATH: &str = "/etc/security/faillock.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Unix seconds of the attempt.
    pub time: u64,
    pub status: u16,
}

impl Record {
    /// `TALLY_STATUS_VALID`. An invalid record is a slot faillock has
    /// retired; it stays in the file and must not be counted.
    pub fn is_valid(&self) -> bool {
        self.status & STATUS_VALID != 0
    }
}

/// The subset of `/etc/security/faillock.conf` that decides a lockout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub deny: u32,
    pub unlock_time: u64,
    pub fail_interval: u64,
    pub dir: PathBuf,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            deny: DEFAULT_DENY,
            unlock_time: DEFAULT_UNLOCK_TIME,
            fail_interval: DEFAULT_FAIL_INTERVAL,
            dir: PathBuf::from(DEFAULT_DIR),
        }
    }
}

/// A lockout in force right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lockout {
    /// Clears on its own after this long.
    Until(Duration),
    /// `unlock_time = 0`: nothing but `faillock --reset` clears it.
    Indefinite,
}

/// Bytes -> records. Trailing bytes that do not complete a record are
/// ignored rather than rejected: the file is appended to in place, and a
/// reader has no lock against a writer mid-write.
pub fn parse_tally(bytes: &[u8]) -> Vec<Record> {
    bytes
        .chunks_exact(RECORD_LEN)
        .map(|record| Record {
            status: u16::from_le_bytes([record[STATUS_OFFSET], record[STATUS_OFFSET + 1]]),
            time: u64::from_le_bytes(
                record[TIME_OFFSET..TIME_OFFSET + 8]
                    .try_into()
                    .expect("chunks_exact yields RECORD_LEN bytes"),
            ),
        })
        .collect()
}

/// Parse the `key = value` lines of `faillock.conf`. Absent, unreadable or
/// commented-out keys keep their documented defaults.
pub fn parse_conf(text: &str) -> Policy {
    let mut policy = Policy::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            // Bare flags (`audit`, `silent`, `even_deny_root`) carry no value
            // and none of them change the arithmetic below. The user is not
            // root, so root_unlock_time cannot apply either.
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "deny" => {
                if let Ok(v) = value.parse() {
                    policy.deny = v;
                }
            }
            "unlock_time" => {
                policy.unlock_time = if value == "never" {
                    0
                } else {
                    value.parse().unwrap_or(policy.unlock_time)
                };
            }
            "fail_interval" => {
                if let Ok(v) = value.parse() {
                    policy.fail_interval = v;
                }
            }
            "dir" if !value.is_empty() => policy.dir = PathBuf::from(value),
            _ => {}
        }
    }
    policy
}

/// Is the account locked, and for how much longer?
///
/// Mirrors linux-pam's `check_tally`, deliberately and exactly:
/// `latest_time` is the newest record of any kind; a failure counts when it
/// is valid and within `fail_interval` of that newest record; `deny`
/// failures lock the account; and the lockout clears once
/// `latest_time + unlock_time < now`. The countdown therefore runs from the
/// *most recent* failure, not the first — every further wrong password while
/// locked pushes the deadline out again, which is the behaviour a user needs
/// to be told about rather than left to discover.
pub fn lockout(records: &[Record], policy: &Policy, now: u64) -> Option<Lockout> {
    if policy.deny == 0 {
        return None;
    }
    let latest = records.iter().map(|record| record.time).max()?;
    let failures = records
        .iter()
        .filter(|record| {
            record.is_valid() && latest.saturating_sub(record.time) < policy.fail_interval
        })
        .count();
    if failures < policy.deny as usize {
        return None;
    }
    if policy.unlock_time == 0 {
        return Some(Lockout::Indefinite);
    }
    let clears_at = latest.saturating_add(policy.unlock_time);
    // Upstream unlocks on `latest + unlock_time < now`, so the final second
    // is still locked.
    (clears_at >= now).then(|| Lockout::Until(Duration::from_secs(clears_at - now)))
}

fn policy_from_disk() -> Policy {
    std::fs::read_to_string(CONF_PATH)
        .map(|text| parse_conf(&text))
        .unwrap_or_default()
}

/// The live answer for `user`, or `None` for "not locked, or unknowable".
pub fn read(user: &str) -> Option<Lockout> {
    // The tally is addressed by file name; a user string that could climb
    // out of the directory is not one this ever asks about.
    if user.is_empty() || user.contains('/') || Path::new(user).components().count() != 1 {
        return None;
    }
    let policy = policy_from_disk();
    let path = policy.dir.join(user);
    if std::fs::metadata(&path).ok()?.len() > MAX_TALLY_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    lockout(&parse_tally(&bytes), &policy, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(time: u64, valid: bool) -> Record {
        Record {
            time,
            status: if valid { STATUS_VALID } else { 0 },
        }
    }

    fn strict() -> Policy {
        Policy::default()
    }

    /// The record layout, verified against a real `/run/faillock/mason`
    /// written by pam_faillock on this machine. `faillock(8)` decoded the
    /// same record as `2026-09-04 13:37:55 SVC vigil-lock V`.
    #[test]
    fn parses_a_record_written_by_pam_faillock() {
        let mut bytes = vec![0u8; RECORD_LEN];
        bytes[..10].copy_from_slice(b"vigil-lock");
        bytes[STATUS_OFFSET..STATUS_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());
        bytes[TIME_OFFSET..TIME_OFFSET + 8].copy_from_slice(&1_788_554_275u64.to_le_bytes());
        // Byte-for-byte what the live file held.
        assert_eq!(
            &bytes[48..],
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x23, 0x2c, 0x9b, 0x6a, 0x00, 0x00,
                0x00, 0x00
            ]
        );
        let records = parse_tally(&bytes);
        assert_eq!(records, vec![record(1_788_554_275, true)]);
        assert!(records[0].is_valid());
    }

    #[test]
    fn a_short_or_empty_tally_is_not_a_lockout() {
        assert_eq!(parse_tally(&[]), Vec::new());
        // A torn write mid-record contributes nothing rather than erroring.
        assert_eq!(parse_tally(&[0u8; RECORD_LEN + 9]).len(), 1);
        assert_eq!(lockout(&[], &strict(), 1_000_000), None);
    }

    #[test]
    fn one_failure_short_of_deny_is_not_a_lockout() {
        let now = 1_000_000;
        let records: Vec<_> = (0..DEFAULT_DENY - 1)
            .map(|i| record(now - u64::from(i), true))
            .collect();
        assert_eq!(lockout(&records, &strict(), now), None);
    }

    #[test]
    fn deny_failures_inside_the_interval_lock_the_account() {
        let now = 1_000_000;
        let records = [
            record(now - 20, true),
            record(now - 10, true),
            record(now - 5, true),
        ];
        // Upstream counts from the newest failure, so the countdown is
        // unlock_time measured from `now - 5`.
        assert_eq!(
            lockout(&records, &strict(), now),
            Some(Lockout::Until(Duration::from_secs(DEFAULT_UNLOCK_TIME - 5)))
        );
    }

    #[test]
    fn failures_spread_wider_than_the_interval_do_not_add_up() {
        let now = 1_000_000;
        let records = [
            record(now - DEFAULT_FAIL_INTERVAL, true),
            record(now - DEFAULT_FAIL_INTERVAL - 500, true),
            record(now, true),
        ];
        // Only the newest is within fail_interval of the newest.
        assert_eq!(lockout(&records, &strict(), now), None);
    }

    #[test]
    fn retired_records_are_not_failures() {
        let now = 1_000_000;
        let records = [
            record(now - 20, true),
            record(now - 10, false),
            record(now - 5, true),
        ];
        assert_eq!(lockout(&records, &strict(), now), None);
    }

    #[test]
    fn a_lapsed_lockout_has_already_cleared() {
        let now = 1_000_000;
        let old = now - DEFAULT_UNLOCK_TIME - 1;
        let records = [
            record(old - 2, true),
            record(old - 1, true),
            record(old, true),
        ];
        assert_eq!(lockout(&records, &strict(), now), None);
        // ...and the last second of the window is still locked.
        let edge = now - DEFAULT_UNLOCK_TIME;
        let records = [
            record(edge - 2, true),
            record(edge - 1, true),
            record(edge, true),
        ];
        assert_eq!(
            lockout(&records, &strict(), now),
            Some(Lockout::Until(Duration::ZERO))
        );
    }

    #[test]
    fn unlock_time_zero_never_clears_on_its_own() {
        let now = 1_000_000;
        let policy = Policy {
            unlock_time: 0,
            ..Policy::default()
        };
        let records = [record(now, true), record(now, true), record(now, true)];
        assert_eq!(lockout(&records, &policy, now), Some(Lockout::Indefinite));
    }

    #[test]
    fn deny_zero_disables_lockout_entirely() {
        let now = 1_000_000;
        let policy = Policy {
            deny: 0,
            ..Policy::default()
        };
        let records = [record(now, true); 10];
        assert_eq!(lockout(&records, &policy, now), None);
    }

    #[test]
    fn the_shipped_conf_is_all_comments_so_the_defaults_apply() {
        let shipped = "\
# Deny access if the number of consecutive authentication failures
# deny = 3
# fail_interval = 900
# unlock_time = 600
";
        assert_eq!(parse_conf(shipped), Policy::default());
    }

    #[test]
    fn conf_values_override_the_defaults() {
        let policy = parse_conf(
            "audit\ndeny = 5\nunlock_time=1200\nfail_interval = 60\ndir = /run/faillock\n",
        );
        assert_eq!(
            policy,
            Policy {
                deny: 5,
                unlock_time: 1200,
                fail_interval: 60,
                dir: PathBuf::from("/run/faillock"),
            }
        );
        // `never` is documented as equivalent to 0.
        assert_eq!(parse_conf("unlock_time = never").unlock_time, 0);
        // Garbage keeps the default rather than erroring the unlock.
        assert_eq!(parse_conf("deny = lots").deny, DEFAULT_DENY);
    }

    #[test]
    fn a_user_name_can_never_leave_the_tally_directory() {
        assert_eq!(read(""), None);
        assert_eq!(read("../../etc/shadow"), None);
        assert_eq!(read("."), None);
        assert_eq!(read(".."), None);
    }
}
