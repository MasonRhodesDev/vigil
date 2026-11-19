#![forbid(unsafe_code)]

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::ExitStatusExt as _;
use vigil_log::testing::{Stderr, exit_code};

#[test]
fn main_installs_the_panic_hook_before_reading_its_arguments() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_vigil-lock"))
        .arg(std::ffi::OsStr::from_bytes(b"hunter2\xff"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        (output.status.signal(), output.status.code()),
        (None, Some(101)),
        "{stderr}"
    );
    assert!(stderr.contains("panic at "), "{stderr}");
    assert!(!stderr.contains("hunter2"), "{stderr}");
}

#[test]
fn a_panic_exits_101_even_when_stderr_is_broken() {
    let arg = std::ffi::OsStr::from_bytes(b"hunter2\xff");
    for stderr in [Stderr::DevFull, Stderr::ReaderlessPipe, Stderr::StalledPipe] {
        assert_eq!(
            exit_code(env!("CARGO_BIN_EXE_vigil-lock"), &[arg], stderr),
            Some(101),
            "{stderr:?}"
        );
    }
}
