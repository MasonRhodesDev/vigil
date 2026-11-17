use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RECORD_LEN: usize = 64;
const STATUS_OFFSET: usize = 54;
const TIME_OFFSET: usize = 56;
const STATUS_VALID: u16 = 0x1;

const MAX_TALLY_BYTES: u64 = 64 * 1024;

pub const DEFAULT_DENY: u32 = 3;
pub const DEFAULT_UNLOCK_TIME: u64 = 600;
pub const DEFAULT_FAIL_INTERVAL: u64 = 900;
const DEFAULT_DIR: &str = "/var/run/faillock";
const CONF_PATH: &str = "/etc/security/faillock.conf";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub time: u64,
    pub status: u16,
}

impl Record {
    pub fn is_valid(&self) -> bool {
        self.status & STATUS_VALID != 0
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lockout {
    Until(Duration),
    Indefinite,
}

pub fn parse_tally(bytes: &[u8]) -> Vec<Record> {
    let (records, _partial) = bytes.as_chunks::<RECORD_LEN>();
    records
        .iter()
        .map(|record| Record {
            status: u16::from_le_bytes([record[STATUS_OFFSET], record[STATUS_OFFSET + 1]]),
            time: u64::from_le_bytes(
                record[TIME_OFFSET..TIME_OFFSET + 8]
                    .try_into()
                    .expect("record is RECORD_LEN bytes"),
            ),
        })
        .collect()
}

pub fn parse_conf(text: &str) -> Policy {
    let mut policy = Policy::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
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
    (clears_at >= now).then(|| Lockout::Until(Duration::from_secs(clears_at - now)))
}

fn policy_from_disk() -> Policy {
    std::fs::read_to_string(CONF_PATH)
        .map(|text| parse_conf(&text))
        .unwrap_or_default()
}

pub fn read(user: &str) -> Option<Lockout> {
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

    #[test]
    fn parses_a_record_written_by_pam_faillock() {
        let mut bytes = vec![0u8; RECORD_LEN];
        bytes[..10].copy_from_slice(b"vigil-lock");
        bytes[STATUS_OFFSET..STATUS_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());
        bytes[TIME_OFFSET..TIME_OFFSET + 8].copy_from_slice(&1_788_554_275u64.to_le_bytes());
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
        assert_eq!(parse_conf("unlock_time = never").unlock_time, 0);
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
