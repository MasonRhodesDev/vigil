//! The root token vigil-lock requires before it obeys a logind Unlock. Each case builds a
//! token directory on disk, with the owner, kinds, modes and bytes drawn here, and checks
//! `admit` against a reference written apart from the parser.

use std::fs::Permissions;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use rustix::fs::{CWD, FileType, Mode};
use vigil_unlock_token::{Moment, WINDOW, admit};

const SESSION: &str = "c1";
const BOOT: &str = "6b1f0c4e-2d3a-4f5b-8c7d-9e0f1a2b3c4d";
const OTHER_BOOT: &str = "6b1f0c4e-2d3a-4f5b-8c7d-9e0f1a2b3c4e";
const NOW: Duration = Duration::from_secs(1_000);
const MAX_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Regular,
    HardLinked,
    Symlink,
    Fifo,
    Directory,
    Missing,
}

#[derive(Debug, Clone)]
pub struct Case {
    trusted: bool,
    dir_mode: u32,
    kind: Kind,
    file_mode: u32,
    content: Vec<u8>,
}

fn token(session: &str, boot: &str, issued: &str) -> Vec<u8> {
    format!("vigil-unlock 1\nsession {session}\nboot {boot}\nissued {issued}\n").into_bytes()
}

fn issued_at(age: Duration) -> String {
    (NOW - age).as_nanos().to_string()
}

fn admissible(content: &[u8]) -> bool {
    let head = token(SESSION, BOOT, "");
    let Some(digits) = content
        .strip_prefix(&head[..head.len() - 1])
        .and_then(|rest| rest.strip_suffix(b"\n"))
    else {
        return false;
    };
    let canonical = !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
        && (digits == b"0" || digits[0] != b'0');
    let issued = std::str::from_utf8(digits)
        .ok()
        .and_then(|d| d.parse::<u64>().ok());
    content.len() <= MAX_LEN
        && canonical
        && issued.is_some_and(|issued| {
            let now = NOW.as_nanos();
            u128::from(issued) <= now && now - u128::from(issued) <= WINDOW.as_nanos()
        })
}

fn expected(case: &Case) -> bool {
    case.trusted
        && case.kind == Kind::Regular
        && case.file_mode & !0o644 == 0
        && case.dir_mode & 0o022 == 0
        && admissible(&case.content)
}

struct Scene {
    base: PathBuf,
    dir: PathBuf,
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.dir, Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {mode:o} {}: {e}", path.display()))?;
    let set = std::fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .mode()
        & 0o7777;
    if set != mode {
        return Err(format!(
            "{} kept mode {set:o}, not {mode:o}",
            path.display()
        ));
    }
    Ok(())
}

fn build(case: &Case) -> Result<Scene, String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let base = std::env::temp_dir().join(format!(
        "vigil-attack-token-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let dir = base.join("unlock");
    let elsewhere = base.join("elsewhere");
    let scene = Scene {
        base: base.clone(),
        dir: dir.clone(),
    };
    let io = |e: std::io::Error| e.to_string();
    for path in [&base, &dir, &elsewhere] {
        std::fs::create_dir(path).map_err(io)?;
        chmod(path, 0o700)?;
    }
    let name = dir.join(SESSION);
    let plant = |path: &Path| -> Result<(), String> {
        std::fs::write(path, &case.content).map_err(io)?;
        chmod(path, case.file_mode)
    };
    match case.kind {
        Kind::Regular => plant(&name)?,
        Kind::HardLinked => {
            plant(&name)?;
            std::fs::hard_link(&name, elsewhere.join(SESSION)).map_err(io)?;
        }
        Kind::Symlink => {
            plant(&elsewhere.join(SESSION))?;
            std::os::unix::fs::symlink(elsewhere.join(SESSION), &name).map_err(io)?;
        }
        Kind::Fifo => {
            rustix::fs::mknodat(
                CWD,
                &name,
                FileType::Fifo,
                Mode::from_raw_mode(case.file_mode),
                0,
            )
            .map_err(|e| e.to_string())?;
            chmod(&name, case.file_mode)?;
        }
        Kind::Directory => {
            std::fs::create_dir(&name).map_err(io)?;
            chmod(&name, 0o700)?;
        }
        Kind::Missing => {}
    }
    chmod(&dir, case.dir_mode)?;
    Ok(scene)
}

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn judge(case: &Case) -> Result<(), String> {
    let scene = build(case)?;
    let owner = if case.trusted {
        me()
    } else {
        me().wrapping_add(1)
    };
    let at = Moment {
        boot: BOOT.into(),
        since_boot: NOW,
    };
    let verdict = admit(&scene.dir, owner, SESSION, &at);
    if verdict.is_ok() != expected(case) {
        return Err(format!(
            "admit said {verdict:?}, the reference says {}admit",
            if expected(case) { "" } else { "do not " }
        ));
    }
    Ok(())
}

fn age() -> impl Strategy<Value = String> {
    prop_oneof![
        (0..=WINDOW.as_millis() as u64 + 2_000).prop_map(|ms| issued_at(Duration::from_millis(ms))),
        (0..5_000u64).prop_map(|ms| (NOW + Duration::from_millis(ms)).as_nanos().to_string()),
        Just(issued_at(WINDOW)),
        Just(
            (NOW - WINDOW - Duration::from_nanos(1))
                .as_nanos()
                .to_string()
        ),
        Just(issued_at(Duration::ZERO)),
        "[0-9+ -]{0,22}",
    ]
}

fn issued_token() -> impl Strategy<Value = Vec<u8>> {
    (
        prop_oneof![4 => Just(SESSION.to_string()), 1 => "[a-zA-Z0-9 ./-]{0,8}"],
        prop_oneof![4 => Just(BOOT.to_string()), 1 => Just(OTHER_BOOT.to_string())],
        age(),
    )
        .prop_map(|(session, boot, issued)| token(&session, &boot, &issued))
}

fn edited(base: Vec<u8>, edits: Vec<(usize, u8, u8)>) -> Vec<u8> {
    let mut bytes = base;
    for (at, op, byte) in edits {
        let at = at % (bytes.len() + 1);
        match op % 3 {
            0 => bytes.insert(at, byte),
            1 if at < bytes.len() => {
                bytes.remove(at);
            }
            _ if at < bytes.len() => bytes[at] = byte,
            _ => bytes.push(byte),
        }
    }
    bytes
}

fn content() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => issued_token(),
        3 => (
            issued_token(),
            proptest::collection::vec((any::<usize>(), any::<u8>(), any::<u8>()), 1..4)
        )
            .prop_map(|(base, edits)| edited(base, edits)),
        1 => (issued_token(), proptest::collection::vec(any::<u8>(), 0..200))
            .prop_map(|(mut base, tail)| {
                base.extend(tail);
                base
            }),
        1 => proptest::collection::vec(any::<u8>(), 0..300),
    ]
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        8 => Just(Kind::Regular),
        1 => Just(Kind::HardLinked),
        1 => Just(Kind::Symlink),
        1 => Just(Kind::Fifo),
        1 => Just(Kind::Directory),
        1 => Just(Kind::Missing),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (
        proptest::bool::weighted(0.85),
        prop_oneof![3 => Just(0o700u32), 1 => (0..0o100u32).prop_map(|bits| 0o700 | bits)],
        kind(),
        prop_oneof![
            3 => Just(0o644u32),
            1 => prop::sample::select(vec![0o400u32, 0o600, 0o640, 0o444]),
            2 => any::<u32>().prop_map(|bits| 0o400 | (bits & 0o5377)),
        ],
        content(),
    )
        .prop_map(|(trusted, dir_mode, kind, file_mode, content)| Case {
            trusted,
            dir_mode,
            kind,
            file_mode,
            content,
        })
}

pub fn attack(cases: u32, seed: u64) -> bool {
    let started = Instant::now();
    let admitted = AtomicUsize::new(0);
    let outcome = super::runner(cases, seed).run(&case(), |case| {
        if expected(&case) {
            admitted.fetch_add(1, Ordering::Relaxed);
        }
        judge(&case).map_err(|why| TestCaseError::fail(format!("{why}\n{case:#?}")))
    });
    let admitted = admitted.into_inner();
    match outcome {
        Ok(()) if admitted == 0 => {
            vigil_log::out!("attack: root token: no case was admissible, so this proves nothing");
            false
        }
        Ok(()) => {
            vigil_log::out!(
                "attack: root token: {cases} cases held, {admitted} admissible, in {:.1?}",
                started.elapsed()
            );
            true
        }
        Err(failure) => {
            vigil_log::out!("attack: root token: FAILED with VIGIL_ATTACK_SEED={seed}\n{failure}");
            false
        }
    }
}

fn pinned() -> Vec<(&'static str, bool, Case)> {
    let fresh = token(SESSION, BOOT, &issued_at(Duration::from_secs(1)));
    let good = Case {
        trusted: true,
        dir_mode: 0o755,
        kind: Kind::Regular,
        file_mode: 0o644,
        content: fresh.clone(),
    };
    let with = |change: fn(&mut Case)| {
        let mut case = good.clone();
        change(&mut case);
        case
    };
    let saying = |content: Vec<u8>| Case {
        content,
        ..good.clone()
    };
    vec![
        ("root's fresh token", true, good.clone()),
        (
            "root's token at the edge of its window",
            true,
            saying(token(SESSION, BOOT, &issued_at(WINDOW))),
        ),
        (
            "root's token written this instant",
            true,
            saying(token(SESSION, BOOT, &issued_at(Duration::ZERO))),
        ),
        (
            "root's token, mode 0400",
            true,
            with(|c| c.file_mode = 0o400),
        ),
        ("no token", false, with(|c| c.kind = Kind::Missing)),
        ("a token the user owns", false, with(|c| c.trusted = false)),
        (
            "a token one nanosecond stale",
            false,
            saying(token(
                SESSION,
                BOOT,
                &(NOW - WINDOW - Duration::from_nanos(1))
                    .as_nanos()
                    .to_string(),
            )),
        ),
        (
            "a token dated one nanosecond ahead",
            false,
            saying(token(
                SESSION,
                BOOT,
                &(NOW + Duration::from_nanos(1)).as_nanos().to_string(),
            )),
        ),
        (
            "a token for another session",
            false,
            saying(token("c2", BOOT, &issued_at(Duration::ZERO))),
        ),
        (
            "a token from another boot",
            false,
            saying(token(SESSION, OTHER_BOOT, &issued_at(Duration::ZERO))),
        ),
        ("a symlinked token", false, with(|c| c.kind = Kind::Symlink)),
        (
            "a FIFO in place of the token",
            false,
            with(|c| c.kind = Kind::Fifo),
        ),
        (
            "a directory in place of the token",
            false,
            with(|c| c.kind = Kind::Directory),
        ),
        (
            "a token with a second hard link",
            false,
            with(|c| c.kind = Kind::HardLinked),
        ),
        (
            "a token in a directory others can write",
            false,
            with(|c| c.dir_mode = 0o777),
        ),
        (
            "a token in a directory its group can write",
            false,
            with(|c| c.dir_mode = 0o775),
        ),
        (
            "a token its group can write",
            false,
            with(|c| c.file_mode = 0o664),
        ),
        ("a setuid token", false, with(|c| c.file_mode = 0o4644)),
        (
            "a token with a leading zero",
            false,
            saying(token(
                SESSION,
                BOOT,
                &format!("0{}", issued_at(Duration::ZERO)),
            )),
        ),
        (
            "a token with a trailing space",
            false,
            saying(token(
                SESSION,
                BOOT,
                &format!("{} ", issued_at(Duration::ZERO)),
            )),
        ),
        (
            "a token with CRLF lines",
            false,
            saying(
                String::from_utf8(fresh.clone())
                    .unwrap()
                    .replace('\n', "\r\n")
                    .into_bytes(),
            ),
        ),
        (
            "a token with a fifth line",
            false,
            saying([fresh.as_slice(), b"extra\n"].concat()),
        ),
        (
            "a token padded past its size limit",
            false,
            saying([fresh.as_slice(), &[b'0'; MAX_LEN]].concat()),
        ),
        (
            "a token without its final newline",
            false,
            saying(fresh[..fresh.len() - 1].to_vec()),
        ),
    ]
}

pub fn replay() -> bool {
    let mut held = true;
    for (name, admits, case) in pinned() {
        let outcome = if expected(&case) != admits {
            Err(format!("the reference says {}", !admits))
        } else {
            judge(&case)
        };
        match outcome {
            Ok(()) => vigil_log::out!("attack: pinned token: {name}: held"),
            Err(why) => {
                vigil_log::out!("attack: pinned token: {name}: FAILED\n{why}");
                held = false;
            }
        }
    }
    held
}
