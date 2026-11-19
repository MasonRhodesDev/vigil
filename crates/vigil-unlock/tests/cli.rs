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
fn a_user_is_refused_before_anything_is_touched() {
    if rustix::process::geteuid().is_root() {
        assert!(
            std::env::var_os("CI").is_none(),
            "this test needs a user other than root"
        );
        vigil_log::say!("skipped: running as root");
        return;
    }
    let output = run(&["c1"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("run as root"), "{stderr}");
    assert!(!stderr.contains("logind"), "it reached the bus: {stderr}");
}

#[test]
fn it_takes_exactly_one_target() {
    for args in [&[][..], &["c1", "c2"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    assert_eq!(run(&["--help"]).status.code(), Some(0));
}
