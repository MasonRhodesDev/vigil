#![forbid(unsafe_code)]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vigil-unlock"))
        .args(args)
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .output()
        .expect("run vigil-unlock")
}

#[test]
fn only_root_reaches_logind() {
    let output = run(&["c1"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    if rustix::process::geteuid().is_root() {
        assert!(
            stderr.contains("logind:"),
            "root goes on to the bus: {stderr}"
        );
    } else {
        assert!(stderr.contains("run as root"), "{stderr}");
        assert!(
            !stderr.contains("logind:"),
            "a user reached the bus: {stderr}"
        );
    }
}

#[test]
fn it_takes_exactly_one_target() {
    for args in [&[][..], &["c1", "c2"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    assert_eq!(run(&["--help"]).status.code(), Some(0));
}
