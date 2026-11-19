#![forbid(unsafe_code)]
//! logind lets a session's owner Lock and Unlock their own session without polkit, and the
//! Unlock signal does not say who asked. So vigil-lock obeys an Unlock only while a token that
//! root wrote names its session and this boot and is at most [`WINDOW`] old.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::time::{ClockId, clock_gettime};

pub const DIR: &str = "/run/vigil/unlock";
pub const ROOT: u32 = 0;
pub const WINDOW: Duration = Duration::from_secs(10);

const MAGIC: &str = "vigil-unlock 1";
const MAX_LEN: usize = 256;
const MAX_SESSION: usize = 64;
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const TOKEN_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// Where and when a token is written or judged. `since_boot` reads `CLOCK_BOOTTIME`, which
/// never steps with the wall clock and keeps counting through suspend, and `boot` keeps a
/// token from one boot out of the next, where the same count recurs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moment {
    pub boot: String,
    pub since_boot: Duration,
}

impl Moment {
    pub fn now() -> io::Result<Self> {
        let boot = std::fs::read_to_string(BOOT_ID)?.trim_end().to_owned();
        if boot.is_empty() || !boot.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{BOOT_ID} is not a boot id"),
            ));
        }
        let now = clock_gettime(ClockId::Boottime);
        let since_boot = Duration::new(
            u64::try_from(now.tv_sec).unwrap_or_default(),
            u32::try_from(now.tv_nsec).unwrap_or_default(),
        );
        Ok(Self { boot, since_boot })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    BadSession,
    NoMoment,
    NoToken,
    Symlink,
    NotRegular,
    NotRoot,
    Loose,
    Linked,
    DirNotRoot,
    DirWritable,
    Malformed,
    OtherSession,
    OtherBoot,
    Future,
    Stale,
    Unreadable(Errno),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSession => f.write_str("the session id is not one logind issues"),
            Self::NoMoment => f.write_str("this boot's id or clock is unreadable"),
            Self::NoToken => f.write_str("no root token"),
            Self::Symlink => f.write_str("the token is a symlink"),
            Self::NotRegular => f.write_str("the token is not a regular file"),
            Self::NotRoot => f.write_str("the token is not owned by root"),
            Self::Loose => f.write_str("the token's mode is looser than 0644"),
            Self::Linked => f.write_str("the token has another hard link"),
            Self::DirNotRoot => f.write_str("the token directory is not owned by root"),
            Self::DirWritable => f.write_str("the token directory is writable by others"),
            Self::Malformed => f.write_str("the token is malformed"),
            Self::OtherSession => f.write_str("the token names another session"),
            Self::OtherBoot => f.write_str("the token is from another boot"),
            Self::Future => f.write_str("the token is dated after now"),
            Self::Stale => f.write_str("the token is stale"),
            Self::Unreadable(errno) => write!(f, "the token is unreadable ({errno})"),
        }
    }
}

/// logind's ids are letters and digits, so this also keeps a path out of the file name.
pub fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn render(session: &str, at: &Moment) -> String {
    let issued = u64::try_from(at.since_boot.as_nanos()).unwrap_or(u64::MAX);
    format!(
        "{MAGIC}\nsession {session}\nboot {}\nissued {issued}\n",
        at.boot
    )
}

/// Ok only for a fresh token that `owner` wrote for `session` in a directory only `owner` can
/// change. The checks run on descriptors, so a path swapped between them cannot pass.
pub fn admit(dir: &Path, owner: u32, session: &str, at: &Moment) -> Result<(), Refusal> {
    if !valid_session(session) {
        return Err(Refusal::BadSession);
    }
    let dir = open_dir(dir).map_err(|errno| match errno {
        Errno::NOENT => Refusal::NoToken,
        errno => Refusal::Unreadable(errno),
    })?;
    let token = fs::openat(
        &dir,
        session,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| match errno {
        Errno::NOENT => Refusal::NoToken,
        Errno::LOOP => Refusal::Symlink,
        errno => Refusal::Unreadable(errno),
    })?;
    let stat = fs::fstat(&token).map_err(Refusal::Unreadable)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(Refusal::NotRegular);
    }
    if stat.st_uid != owner {
        return Err(Refusal::NotRoot);
    }
    if stat.st_mode & 0o7777 & !TOKEN_MODE != 0 {
        return Err(Refusal::Loose);
    }
    if stat.st_nlink != 1 {
        return Err(Refusal::Linked);
    }
    trusted_dir(&dir, owner)?;
    let mut content = Vec::with_capacity(MAX_LEN + 1);
    File::from(token)
        .take(MAX_LEN as u64 + 1)
        .read_to_end(&mut content)
        .map_err(|e| Refusal::Unreadable(Errno::from_io_error(&e).unwrap_or(Errno::IO)))?;
    judge(&content, session, at)
}

/// [`admit`] against the system directory, root, and this moment.
pub fn admit_now(session: &str) -> Result<(), Refusal> {
    let at = Moment::now().map_err(|_| Refusal::NoMoment)?;
    admit(Path::new(DIR), ROOT, session, &at)
}

fn open_dir(dir: &Path) -> Result<OwnedFd, Errno> {
    fs::open(
        dir,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn trusted_dir(dir: &OwnedFd, owner: u32) -> Result<(), Refusal> {
    let stat = fs::fstat(dir).map_err(Refusal::Unreadable)?;
    if stat.st_uid != owner {
        return Err(Refusal::DirNotRoot);
    }
    if stat.st_mode & WRITABLE_BY_OTHERS != 0 {
        return Err(Refusal::DirWritable);
    }
    Ok(())
}

fn judge(content: &[u8], session: &str, at: &Moment) -> Result<(), Refusal> {
    if content.len() > MAX_LEN {
        return Err(Refusal::Malformed);
    }
    let text = std::str::from_utf8(content).map_err(|_| Refusal::Malformed)?;
    let body = text.strip_suffix('\n').ok_or(Refusal::Malformed)?;
    let fields: Vec<&str> = body.split('\n').collect();
    let [magic, named, boot, issued] = fields[..] else {
        return Err(Refusal::Malformed);
    };
    let (Some(named), Some(boot), Some(issued)) = (
        named.strip_prefix("session "),
        boot.strip_prefix("boot "),
        issued.strip_prefix("issued ").and_then(canonical_nanos),
    ) else {
        return Err(Refusal::Malformed);
    };
    if magic != MAGIC {
        return Err(Refusal::Malformed);
    }
    if named != session {
        return Err(Refusal::OtherSession);
    }
    if boot != at.boot {
        return Err(Refusal::OtherBoot);
    }
    let issued = Duration::from_nanos(issued);
    let age = at.since_boot.checked_sub(issued).ok_or(Refusal::Future)?;
    if age > WINDOW {
        return Err(Refusal::Stale);
    }
    Ok(())
}

fn canonical_nanos(digits: &str) -> Option<u64> {
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    canonical.then(|| digits.parse().ok()).flatten()
}

/// Creates `dir` and its parent with mode 0755 when they are missing. Root runs this, so a
/// lock still reads its token on an install whose tmpfiles entry has not run yet.
pub fn prepare(dir: &Path) -> io::Result<()> {
    for path in [dir.parent(), Some(dir)].into_iter().flatten() {
        match fs::mkdir(path, Mode::from_raw_mode(DIR_MODE)) {
            Ok(()) => fs::chmod(path, Mode::from_raw_mode(DIR_MODE))?,
            Err(Errno::EXIST) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}

/// Writes the token for `session` as `owner` would leave it: a temporary file in the same
/// directory, renamed over the name [`admit`] opens, so a reader never sees half a token.
pub fn issue(dir: &Path, owner: u32, session: &str, at: &Moment) -> io::Result<()> {
    if !valid_session(session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{session:?} is not a logind session id"),
        ));
    }
    let dir = open_dir(dir)?;
    trusted_dir(&dir, owner).map_err(|why| io::Error::other(why.to_string()))?;
    let temp = format!(".{session}.{}", std::process::id());
    let written = fs::openat(
        &dir,
        &temp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(TOKEN_MODE),
    )
    .map_err(io::Error::from)
    .and_then(|fd| {
        fs::fchmod(&fd, Mode::from_raw_mode(TOKEN_MODE))?;
        File::from(fd).write_all(render(session, at).as_bytes())?;
        fs::renameat(&dir, &temp, &dir, session)?;
        Ok(())
    });
    if written.is_err() {
        let _ = fs::unlinkat(&dir, &temp, AtFlags::empty());
    }
    written
}

pub fn revoke(dir: &Path, session: &str) -> io::Result<()> {
    if !valid_session(session) {
        return Ok(());
    }
    match fs::unlinkat(open_dir(dir)?, session, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(errno) => Err(errno.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Moment {
        Moment {
            boot: "0f0e0d0c-0b0a-0908-0706-050403020100".into(),
            since_boot: Duration::from_secs(secs),
        }
    }

    #[test]
    fn a_rendered_token_is_judged_fresh_through_its_window_and_stale_after() {
        let token = render("c1", &at(100));
        assert_eq!(judge(token.as_bytes(), "c1", &at(100)), Ok(()));
        assert_eq!(judge(token.as_bytes(), "c1", &at(110)), Ok(()));
        assert_eq!(judge(token.as_bytes(), "c1", &at(111)), Err(Refusal::Stale));
        assert_eq!(judge(token.as_bytes(), "c1", &at(99)), Err(Refusal::Future));
    }

    #[test]
    fn this_machine_has_a_boot_id_and_a_boot_clock() {
        let now = Moment::now().expect("read the boot id");
        assert_eq!(now.boot.len(), 36, "{now:?}");
        assert!(now.since_boot > Duration::ZERO);
    }

    #[test]
    fn prepare_issue_admit_revoke_round_trip() {
        let base = std::env::temp_dir().join(format!("vigil-token-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).expect("create the base directory");
        let dir = base.join("vigil").join("unlock");
        prepare(&dir).expect("create both levels");
        prepare(&dir).expect("prepare again");
        for level in [dir.parent().expect("parent"), dir.as_path()] {
            let mode = fs::stat(level).expect("stat").st_mode & 0o7777;
            assert_eq!(mode, DIR_MODE, "{}", level.display());
        }
        let me = rustix::process::getuid().as_raw();
        let now = Moment::now().expect("this boot");
        issue(&dir, me, "c1", &now).expect("issue");
        assert_eq!(admit(&dir, me, "c1", &now), Ok(()));
        revoke(&dir, "c1").expect("revoke");
        revoke(&dir, "c1").expect("revoke again");
        assert_eq!(admit(&dir, me, "c1", &now), Err(Refusal::NoToken));
        assert_eq!(
            std::fs::read_dir(&dir).expect("list").count(),
            0,
            "a temporary file stayed"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_ids_that_could_name_a_path_are_not_valid() {
        for id in ["", ".", "..", "a/b", "c1\n", ".c1", "c 1"] {
            assert!(!valid_session(id), "{id:?}");
        }
        assert!(!valid_session(&"a".repeat(MAX_SESSION + 1)));
        for id in ["1", "c1", "c-2", "_x"] {
            assert!(valid_session(id), "{id:?}");
        }
    }
}
