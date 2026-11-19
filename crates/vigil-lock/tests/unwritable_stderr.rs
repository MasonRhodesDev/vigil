#[path = "../../../tests/support/unwritable_stderr.rs"]
mod support;

use support::{Stderr, exit_code};

const LOCKER: &str = env!("CARGO_BIN_EXE_vigil-lock");

#[test]
fn a_full_disk_on_stderr_keeps_the_usage_exit_code() {
    assert_eq!(exit_code(LOCKER, &["--bogus"], Stderr::DevFull), Some(2));
}

#[test]
fn a_stderr_pipe_with_no_reader_keeps_the_usage_exit_code() {
    assert_eq!(
        exit_code(LOCKER, &["--bogus"], Stderr::ReaderlessPipe),
        Some(2)
    );
}

#[test]
fn a_stderr_pipe_nobody_drains_does_not_park_the_locker() {
    assert_eq!(
        exit_code(LOCKER, &["--bogus"], Stderr::StalledPipe),
        Some(2)
    );
}
