#![deny(unsafe_code)]

pub use zeroize::{Zeroize, Zeroizing};

#[derive(Default)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for Secret {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
    }
}

impl Eq for Secret {}

/// `RLIMIT_CORE` survives exec but `PR_SET_DUMPABLE` resets on it, so every exec'd binary calls this itself.
pub fn deny_memory_dumps() -> std::io::Result<()> {
    use rustix::process::{DumpableBehavior, Resource, Rlimit, set_dumpable_behavior, setrlimit};
    let core = setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );
    let dumpable = set_dumpable_behavior(DumpableBehavior::NotDumpable);
    core.and(dumpable).map_err(std::io::Error::from)
}

thread_local! {
    static COSMETIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

static PLAIN_EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// The first call in every main. A panic on any thread ends the process at once with status
/// 101, so it never runs half alive with a dead PAM worker, a lost signal or a poisoned lock,
/// and ext-session-lock keeps the session locked when its client dies. The report names only
/// the place, since a panic message can quote typed text. It goes out through `vigil_log::say!`,
/// which allocates nothing and never blocks or panics, so a broken stderr cannot hold up the
/// exit. Only work run through [`cosmetic`] survives its own panic. After [`crash_plainly`] the
/// hook instead writes the place with one write(2) and exits with the status given there.
pub fn exit_on_panic() {
    #[expect(clippy::disallowed_methods, reason = "this is the one panic hook")]
    std::panic::set_hook(Box::new(|info| {
        let plain = PLAIN_EXIT.load(std::sync::atomic::Ordering::Relaxed);
        if plain >= 0 {
            report_plainly(info.location());
            exit_now(plain);
        }
        let cosmetic = COSMETIC.try_with(std::cell::Cell::get).unwrap_or(false);
        let then = if cosmetic {
            "going on without it"
        } else {
            "exiting"
        };
        match info.location() {
            Some(at) => vigil_log::say!("panic at {}:{}; {then}", at.file(), at.line()),
            None => vigil_log::say!("panic at an unknown place; {then}"),
        }
        if !cosmetic {
            exit_now(101);
        }
    }));
}

/// For a process about to install a seccomp filter that allows little beyond write(2) and
/// exit. SIGSEGV and SIGBUS get their default action, so a fault ends the process by its own
/// signal rather than in std's handler, whose sigaction the filter kills. A stack overflow then
/// dies of SIGSEGV unannounced. Call it before the filter goes on.
pub fn die_of_faults() -> std::io::Result<()> {
    for signal in [libc::SIGSEGV, libc::SIGBUS] {
        default_action(signal)?;
    }
    Ok(())
}

/// For a process whose seccomp filter is on. From here on, a panic writes its place to stderr
/// in one write(2) and exits with `status`, because `say!` makes calls such a filter kills.
/// Call it only once the filter is on, so a panic while installing it still exits 101.
pub fn crash_plainly(status: u8) {
    PLAIN_EXIT.store(status.into(), std::sync::atomic::Ordering::Relaxed);
}

fn report_plainly(at: Option<&std::panic::Location<'_>>) {
    use std::io::Write;
    let mut buf = [0u8; 512];
    let mut rest = &mut buf[..];
    let _ = match at {
        Some(at) => writeln!(rest, "panic at {}:{}", at.file(), at.line()),
        None => writeln!(rest, "panic at an unknown place"),
    };
    let len = 512 - rest.len();
    let _ = rustix::io::write(rustix::stdio::stderr(), &buf[..len]);
}

#[expect(
    unsafe_code,
    reason = "neither std nor rustix 1.1 has a safe sigaction"
)]
fn default_action(signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: SIG_DFL installs no handler, so no code of this process runs on the signal.
    if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Runs `work` so that a panic inside it costs only its result. For work the process can go
/// on without, such as a wallpaper; never for anything authentication or the lock needs.
pub fn cosmetic<T>(work: impl FnOnce() -> T) -> Option<T> {
    let was = COSMETIC.replace(true);
    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).ok();
    COSMETIC.set(was);
    done
}

// std::process::exit would run C atexit handlers while other threads still run, and rustix
// hides its exit_group under a module name it rotates on purpose.
#[expect(unsafe_code, reason = "_exit has no safe wrapper in std or rustix 1.1")]
fn exit_now(status: i32) -> ! {
    // SAFETY: _exit takes no pointers and only ends the process.
    unsafe { libc::_exit(status) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_text() {
        let secret = Secret::from("hunter2");
        let rendered = format!("{secret:?} {:?}", Some(&secret));
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert_eq!(rendered, "Secret(<redacted>) Some(Secret(<redacted>))");
    }

    #[test]
    fn equality_compares_the_whole_text() {
        assert_eq!(Secret::from("abc"), Secret::from(String::from("abc")));
        assert_ne!(Secret::from("abc"), Secret::from("abd"));
        assert_ne!(Secret::from("abc"), Secret::from("abcd"));
        assert_eq!(Secret::default(), Secret::from(""));
    }

    fn child(name: &str) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--test-threads=1", "--nocapture"])
            .env("VIGIL_PANIC_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn in_child() -> bool {
        if std::env::var_os("VIGIL_PANIC_CHILD").is_none() {
            return false;
        }
        exit_on_panic();
        deny_memory_dumps().unwrap();
        true
    }

    fn exited(output: &std::process::Output) -> (Option<i32>, Option<i32>, String) {
        use std::os::unix::process::ExitStatusExt;
        (
            output.status.signal(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    #[test]
    fn a_worker_panic_exits_101_and_names_only_the_place() {
        if in_child() {
            let _ = std::thread::spawn(|| panic!("hunter2")).join();
            std::process::exit(0);
        }
        let (signal, code, stderr) = exited(&child(
            "tests::a_worker_panic_exits_101_and_names_only_the_place",
        ));
        assert_eq!((signal, code), (None, Some(101)), "{stderr}");
        assert!(stderr.contains("panic at "), "{stderr}");
        assert!(stderr.contains("lib.rs:"), "{stderr}");
        assert!(!stderr.contains("hunter2"), "{stderr}");
    }

    #[test]
    fn a_plain_panic_exits_with_its_status_and_names_only_the_place() {
        if in_child() {
            crash_plainly(12);
            let _ = std::thread::spawn(|| panic!("hunter2")).join();
            std::process::exit(0);
        }
        let (signal, code, stderr) = exited(&child(
            "tests::a_plain_panic_exits_with_its_status_and_names_only_the_place",
        ));
        assert_eq!((signal, code), (None, Some(12)), "{stderr}");
        assert!(stderr.starts_with("panic at "), "{stderr}");
        assert!(stderr.contains("lib.rs:"), "{stderr}");
        assert!(!stderr.contains("hunter2"), "{stderr}");
    }

    #[test]
    fn a_main_thread_panic_exits_101_without_unwinding() {
        struct Unwound;
        impl Drop for Unwound {
            fn drop(&mut self) {
                vigil_log::say!("unwound");
            }
        }
        if in_child() {
            let _guard = Unwound;
            panic!("main");
        }
        let (signal, code, stderr) = exited(&child(
            "tests::a_main_thread_panic_exits_101_without_unwinding",
        ));
        assert_eq!((signal, code), (None, Some(101)), "{stderr}");
        assert!(!stderr.contains("unwound"), "{stderr}");
    }

    #[test]
    fn a_cosmetic_panic_costs_only_its_result() {
        if in_child() {
            let lost = std::thread::spawn(|| cosmetic(|| -> u8 { panic!("hunter2") }))
                .join()
                .unwrap();
            let kept = cosmetic(|| 7);
            std::process::exit(if (lost, kept) == (None, Some(7)) {
                0
            } else {
                3
            });
        }
        let (signal, code, stderr) =
            exited(&child("tests::a_cosmetic_panic_costs_only_its_result"));
        assert_eq!((signal, code), (None, Some(0)), "{stderr}");
        assert!(stderr.contains("going on without it"), "{stderr}");
        assert!(!stderr.contains("hunter2"), "{stderr}");
    }

    #[test]
    fn a_panic_after_cosmetic_work_still_exits() {
        if in_child() {
            let _ = cosmetic(|| -> u8 { panic!("first") });
            panic!("second");
        }
        let (signal, code, stderr) =
            exited(&child("tests::a_panic_after_cosmetic_work_still_exits"));
        assert_eq!((signal, code), (None, Some(101)), "{stderr}");
        assert!(stderr.contains("going on without it"), "{stderr}");
        assert!(stderr.contains("exiting"), "{stderr}");
    }

    #[test]
    fn dumps_are_denied_for_this_process() {
        deny_memory_dumps().unwrap();
        let core = rustix::process::getrlimit(rustix::process::Resource::Core);
        assert_eq!((core.current, core.maximum), (Some(0), Some(0)));
        assert_eq!(
            rustix::process::dumpable_behavior().unwrap(),
            rustix::process::DumpableBehavior::NotDumpable
        );
    }
}
