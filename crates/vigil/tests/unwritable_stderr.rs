#[path = "../../../tests/support/unwritable_stderr.rs"]
mod support;

use support::{Stderr, exit_code};

const GREETER: &str = env!("CARGO_BIN_EXE_vigil");

#[test]
fn a_full_disk_on_stderr_keeps_the_error_exit_code() {
    assert_eq!(exit_code(GREETER, &["--bogus"], Stderr::DevFull), Some(1));
}

#[test]
fn a_stderr_pipe_with_no_reader_keeps_the_error_exit_code() {
    assert_eq!(
        exit_code(GREETER, &["--bogus"], Stderr::ReaderlessPipe),
        Some(1)
    );
}

#[test]
fn a_stderr_pipe_nobody_drains_does_not_park_the_greeter() {
    assert_eq!(
        exit_code(GREETER, &["--bogus"], Stderr::StalledPipe),
        Some(1)
    );
}
