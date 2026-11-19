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

/// Both settings survive fork and reset on exec, so every exec'd binary calls this itself.
pub fn deny_memory_dumps() -> std::io::Result<()> {
    use rustix::process::{DumpableBehavior, Resource, Rlimit, set_dumpable_behavior, setrlimit};
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    set_dumpable_behavior(DumpableBehavior::NotDumpable)?;
    Ok(())
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
