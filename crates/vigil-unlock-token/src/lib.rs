#![forbid(unsafe_code)]
//! logind lets a session's owner Lock and Unlock their own session without polkit, and the
//! Unlock signal does not say who asked. So vigil-lock obeys an Unlock only while a token that
//! root wrote names its session and this boot and is at most [`WINDOW`] old.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use rustix::fs::{self, AtFlags, CWD, FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::time::{ClockId, clock_gettime};

pub const DIR: &str = "/run/vigil/unlock";
pub const WINDOW: Duration = Duration::from_secs(10);

const MAGIC: &str = "vigil-unlock 1";
const MAX_LEN: usize = 256;
const MAX_SESSION: usize = 64;
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const TOKEN_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o755;
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// Who must own the token, its directory, and that directory's parent. Root owns all three on
/// a real system; tests vary each one alone so every ownership check is pinned by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owners {
    pub token: u32,
    pub dir: u32,
    pub parent: u32,
}

impl Owners {
    pub const ROOT: Self = Self::all(0);

    pub const fn all(uid: u32) -> Self {
        Self {
            token: uid,
            dir: uid,
            parent: uid,
        }
    }
}

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
    DirSymlink,
    DirNotRoot,
    DirWritable,
    ParentNotRoot,
    ParentWritable,
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
            Self::DirSymlink => {
                f.write_str("the token directory or its parent is a symlink or not a directory")
            }
            Self::DirNotRoot => f.write_str("the token directory is not owned by root"),
            Self::DirWritable => f.write_str("the token directory is writable by others"),
            Self::ParentNotRoot => f.write_str("the token directory's parent is not owned by root"),
            Self::ParentWritable => {
                f.write_str("the token directory's parent is writable by others")
            }
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

/// Ok only for a fresh token that `owners.token` wrote for `session` in a directory, under a
/// parent, that only their owners can change. The checks run on descriptors, so a path
/// swapped between them cannot pass.
pub fn admit(dir: &Path, owners: Owners, session: &str, at: &Moment) -> Result<(), Refusal> {
    if !valid_session(session) {
        return Err(Refusal::BadSession);
    }
    let dir = trusted_dir(dir, owners)?;
    admit_in(&dir, owners, session, at)
}

/// [`admit`] against the system directory, root, and this moment.
pub fn admit_now(session: &str) -> Result<(), Refusal> {
    let at = Moment::now().map_err(|_| Refusal::NoMoment)?;
    admit(Path::new(DIR), Owners::ROOT, session, &at)
}

fn admit_in(dir: &OwnedFd, owners: Owners, session: &str, at: &Moment) -> Result<(), Refusal> {
    let token = fs::openat(
        dir,
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
    if stat.st_uid != owners.token {
        return Err(Refusal::NotRoot);
    }
    if stat.st_mode & 0o7777 & !TOKEN_MODE != 0 {
        return Err(Refusal::Loose);
    }
    if stat.st_nlink != 1 {
        return Err(Refusal::Linked);
    }
    let mut content = Vec::with_capacity(MAX_LEN + 1);
    File::from(token)
        .take(MAX_LEN as u64 + 1)
        .read_to_end(&mut content)
        .map_err(|e| Refusal::Unreadable(Errno::from_io_error(&e).unwrap_or(Errno::IO)))?;
    judge(&content, session, at)
}

fn open_level(at: impl rustix::fd::AsFd, path: &Path) -> Result<OwnedFd, Refusal> {
    fs::openat(
        at,
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| match errno {
        Errno::NOENT => Refusal::NoToken,
        Errno::LOOP | Errno::NOTDIR => Refusal::DirSymlink,
        errno => Refusal::Unreadable(errno),
    })
}

fn owned_shut(
    level: &OwnedFd,
    owner: u32,
    not_owned: Refusal,
    writable: Refusal,
) -> Result<(), Refusal> {
    let stat = fs::fstat(level).map_err(Refusal::Unreadable)?;
    if stat.st_uid != owner {
        return Err(not_owned);
    }
    if stat.st_mode & WRITABLE_BY_OTHERS != 0 {
        return Err(writable);
    }
    Ok(())
}

/// Opens `dir` through its parent, neither one followed as a symlink, and checks that only
/// their owners can change either. Whoever could rename or replace the token directory could
/// plant a token, so the parent counts as much as the directory itself.
fn trusted_dir(dir: &Path, owners: Owners) -> Result<OwnedFd, Refusal> {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(Refusal::Unreadable(Errno::INVAL));
    };
    let parent = open_level(CWD, parent)?;
    owned_shut(
        &parent,
        owners.parent,
        Refusal::ParentNotRoot,
        Refusal::ParentWritable,
    )?;
    let dir = open_level(&parent, Path::new(name))?;
    owned_shut(&dir, owners.dir, Refusal::DirNotRoot, Refusal::DirWritable)?;
    Ok(dir)
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

fn refused(why: Refusal) -> io::Error {
    io::Error::other(why.to_string())
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

/// Writes the token for `session` as `owners.token` would leave it. The token is written to
/// an unnamed file and linked in whole, so a reader never sees half a token and a run killed
/// part way leaves no temporary file behind.
pub fn issue(dir: &Path, owners: Owners, session: &str, at: &Moment) -> io::Result<()> {
    if !valid_session(session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{session:?} is not a logind session id"),
        ));
    }
    let dir = trusted_dir(dir, owners).map_err(refused)?;
    let fd = fs::openat(
        &dir,
        ".",
        OFlags::WRONLY | OFlags::TMPFILE | OFlags::CLOEXEC,
        Mode::from_raw_mode(TOKEN_MODE),
    )?;
    fs::fchmod(&fd, Mode::from_raw_mode(TOKEN_MODE))?;
    let mut file = File::from(fd);
    file.write_all(render(session, at).as_bytes())?;
    let unnamed = format!("/proc/self/fd/{}", file.as_raw_fd());
    let link = || {
        fs::linkat(
            CWD,
            unnamed.as_str(),
            &dir,
            session,
            AtFlags::SYMLINK_FOLLOW,
        )
    };
    match link() {
        Err(Errno::EXIST) => {
            fs::unlinkat(&dir, session, AtFlags::empty())?;
            link()?;
        }
        linked => linked?,
    }
    Ok(())
}

pub fn revoke(dir: &Path, session: &str) -> io::Result<()> {
    if !valid_session(session) {
        return Ok(());
    }
    let dir = open_level(CWD, dir).map_err(refused)?;
    match fs::unlinkat(&dir, session, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(errno) => Err(errno.into()),
    }
}

/// Removes every entry of a trusted `dir` that is not a fresh token. A run cut short before it
/// revoked leaves a token that lapses after [`WINDOW`]; the next run clears it away here.
pub fn sweep(dir: &Path, owners: Owners, at: &Moment) -> io::Result<()> {
    let opened = trusted_dir(dir, owners).map_err(refused)?;
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let fresh = name
            .to_str()
            .is_some_and(|name| valid_session(name) && admit_in(&opened, owners, name, at).is_ok());
        if !fresh {
            match fs::unlinkat(&opened, name.as_os_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(Errno::ISDIR) => {
                    let _ = fs::unlinkat(&opened, name.as_os_str(), AtFlags::REMOVEDIR);
                }
                Err(errno) => return Err(errno.into()),
            }
        }
    }
    Ok(())
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

    fn me() -> u32 {
        rustix::process::getuid().as_raw()
    }

    fn mine() -> Owners {
        Owners::all(me())
    }

    fn someone_else() -> u32 {
        me().wrapping_add(1)
    }

    struct Scene {
        base: std::path::PathBuf,
        dir: std::path::PathBuf,
    }

    impl Scene {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir()
                .join(format!("vigil-token-unit-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir(&base).expect("create the base directory");
            let dir = base.join("vigil").join("unlock");
            prepare(&dir).expect("create both levels");
            Self { base, dir }
        }

        fn parent(&self) -> &Path {
            self.dir.parent().expect("parent")
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<_> = std::fs::read_dir(&self.dir)
                .expect("list")
                .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scene {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn chmod(path: &Path, mode: u32) {
        fs::chmod(path, Mode::from_raw_mode(mode)).expect("chmod");
    }

    #[test]
    fn prepare_issue_admit_revoke_round_trip() {
        let scene = Scene::new("round-trip");
        prepare(&scene.dir).expect("prepare again");
        for level in [scene.parent(), scene.dir.as_path()] {
            let mode = fs::stat(level).expect("stat").st_mode & 0o7777;
            assert_eq!(mode, DIR_MODE, "{}", level.display());
        }
        let now = Moment::now().expect("this boot");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        issue(&scene.dir, mine(), "c1", &now).expect("issue over an earlier token");
        assert_eq!(admit(&scene.dir, mine(), "c1", &now), Ok(()));
        assert_eq!(scene.names(), ["c1"], "only the token is in the directory");
        revoke(&scene.dir, "c1").expect("revoke");
        revoke(&scene.dir, "c1").expect("revoke again");
        assert_eq!(admit(&scene.dir, mine(), "c1", &now), Err(Refusal::NoToken));
        assert!(scene.names().is_empty(), "{:?}", scene.names());
    }

    #[test]
    fn a_token_someone_else_owns_is_refused_in_a_trusted_directory() {
        let scene = Scene::new("token-owner");
        let now = Moment::now().expect("this boot");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        let owners = Owners {
            token: someone_else(),
            ..mine()
        };
        assert_eq!(admit(&scene.dir, owners, "c1", &now), Err(Refusal::NotRoot));
    }

    #[test]
    fn a_trusted_token_in_a_directory_someone_else_owns_is_refused() {
        let scene = Scene::new("dir-owner");
        let now = Moment::now().expect("this boot");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        let owners = Owners {
            dir: someone_else(),
            ..mine()
        };
        assert_eq!(
            admit(&scene.dir, owners, "c1", &now),
            Err(Refusal::DirNotRoot)
        );
    }

    #[test]
    fn a_token_under_a_parent_someone_else_owns_is_refused() {
        let scene = Scene::new("parent-owner");
        let now = Moment::now().expect("this boot");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        let owners = Owners {
            parent: someone_else(),
            ..mine()
        };
        assert_eq!(
            admit(&scene.dir, owners, "c1", &now),
            Err(Refusal::ParentNotRoot)
        );
        assert!(issue(&scene.dir, owners, "c1", &now).is_err());
    }

    #[test]
    fn a_token_under_a_parent_others_can_write_is_refused() {
        let scene = Scene::new("parent-writable");
        let now = Moment::now().expect("this boot");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        chmod(scene.parent(), 0o777);
        assert_eq!(
            admit(&scene.dir, mine(), "c1", &now),
            Err(Refusal::ParentWritable)
        );
        assert!(issue(&scene.dir, mine(), "c1", &now).is_err());
    }

    #[test]
    fn a_symlinked_token_directory_is_refused() {
        let scene = Scene::new("dir-symlink");
        let now = Moment::now().expect("this boot");
        let real = scene.parent().join("real");
        std::fs::rename(&scene.dir, &real).expect("move the directory aside");
        std::os::unix::fs::symlink(&real, &scene.dir).expect("link it back");
        issue(&real, mine(), "c1", &now).expect("issue in the real directory");
        assert_eq!(admit(&real, mine(), "c1", &now), Ok(()));
        assert_eq!(
            admit(&scene.dir, mine(), "c1", &now),
            Err(Refusal::DirSymlink)
        );
        assert!(issue(&scene.dir, mine(), "c1", &now).is_err());
    }

    #[test]
    fn a_symlinked_parent_is_refused() {
        let scene = Scene::new("parent-symlink");
        let now = Moment::now().expect("this boot");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        let linked = scene.base.join("linked");
        std::os::unix::fs::symlink(scene.parent(), &linked).expect("link the parent");
        assert_eq!(
            admit(&linked.join("unlock"), mine(), "c1", &now),
            Err(Refusal::DirSymlink)
        );
    }

    #[test]
    fn a_temporary_file_left_by_a_killed_run_does_not_block_the_next() {
        let scene = Scene::new("stale-temp");
        let now = Moment::now().expect("this boot");
        std::fs::write(
            scene.dir.join(format!(".c1.{}", std::process::id())),
            "half",
        )
        .expect("plant a temporary file");
        issue(&scene.dir, mine(), "c1", &now).expect("issue");
        assert_eq!(admit(&scene.dir, mine(), "c1", &now), Ok(()));
    }

    #[test]
    fn sweep_removes_what_a_cut_short_run_left_and_keeps_fresh_tokens() {
        let scene = Scene::new("sweep");
        let now = Moment::now().expect("this boot");
        let then = Moment {
            since_boot: now.since_boot - WINDOW - Duration::from_secs(1),
            ..now.clone()
        };
        issue(&scene.dir, mine(), "fresh", &now).expect("issue a fresh token");
        issue(&scene.dir, mine(), "stale", &then).expect("issue a stale token");
        std::fs::write(scene.dir.join(".c1.123"), "half").expect("plant a temporary file");
        std::fs::write(scene.dir.join("junk"), "not a token").expect("plant junk");
        sweep(&scene.dir, mine(), &now).expect("sweep");
        assert_eq!(scene.names(), ["fresh"]);
    }

    #[test]
    fn sweep_leaves_an_untrusted_directory_alone() {
        let scene = Scene::new("sweep-untrusted");
        let now = Moment::now().expect("this boot");
        std::fs::write(scene.dir.join("junk"), "not a token").expect("plant junk");
        let owners = Owners {
            dir: someone_else(),
            ..mine()
        };
        assert!(sweep(&scene.dir, owners, &now).is_err());
        assert_eq!(scene.names(), ["junk"]);
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
