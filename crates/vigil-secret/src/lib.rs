#![forbid(unsafe_code)]

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

/// Call from the main thread. Its panics still unwind and run cleanup, while a panic on any
/// other thread aborts, so a dead worker cannot leave a running process that no longer works.
pub fn abort_on_worker_panic() {
    let main = std::thread::current().id();
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        if std::thread::current().id() != main {
            std::process::abort();
        }
    }));
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

    const SIGABRT: i32 = 6;

    fn child(name: &str) -> std::process::ExitStatus {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--test-threads=1"])
            .env("VIGIL_PANIC_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    #[test]
    fn a_worker_panic_ends_the_process() {
        if std::env::var_os("VIGIL_PANIC_CHILD").is_some() {
            deny_memory_dumps().unwrap();
            abort_on_worker_panic();
            let _ = std::thread::spawn(|| panic!("worker")).join();
            return;
        }
        use std::os::unix::process::ExitStatusExt;
        let status = child("tests::a_worker_panic_ends_the_process");
        assert_eq!(status.signal(), Some(SIGABRT), "{status}");
    }

    #[test]
    fn a_main_thread_panic_still_unwinds() {
        if std::env::var_os("VIGIL_PANIC_CHILD").is_some() {
            deny_memory_dumps().unwrap();
            abort_on_worker_panic();
            panic!("main");
        }
        use std::os::unix::process::ExitStatusExt;
        let status = child("tests::a_main_thread_panic_still_unwinds");
        assert_eq!(
            (status.signal(), status.code()),
            (None, Some(101)),
            "{status}"
        );
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
