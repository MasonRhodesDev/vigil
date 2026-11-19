#![deny(unsafe_code)]

// No test harness: libtest draws on a worker thread, where glibc's first
// allocation opens /sys to size a new arena. The helper draws on its main
// thread, and so does each probe here.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use rustix::process::Pid;
use vigil_scene::{Error, Format, Helper, Layer, Request, confine, lock_down};

const PROBE: &str = "VIGIL_SCENE_SANDBOX_PROBE";

fn probe(confined: bool, act: &str) -> ExitStatus {
    let on = if confined { "on" } else { "off" };
    Command::new(std::env::current_exe().unwrap())
        .env(PROBE, format!("{on}:{act}"))
        .status()
        .unwrap()
}

// Everything a probe needs is made before the sandbox goes up, so the act
// is its only system call outside drawing.
fn attempt(act: &str, svg: &[u8], parent: Pid) {
    match act {
        "open" => drop(std::fs::File::open("/proc/self/status")),
        "socket" => drop(std::net::UdpSocket::bind("127.0.0.1:0")),
        "signal" => drop(rustix::process::test_kill_process(parent)),
        "ptrace" => {
            // SAFETY: PTRACE_PEEKDATA passes no pointer and reads nothing
            // unless this process already traces the target, which it never
            // does, so the call can only fail.
            #[allow(unsafe_code)]
            let _ = unsafe {
                libc::ptrace(
                    libc::PTRACE_PEEKDATA,
                    parent.as_raw_nonzero().get(),
                    0usize,
                    0usize,
                )
            };
        }
        "draw" => {
            let pixels = Request {
                svg: Some(svg),
                spec: None,
                mode: (1920, 1080),
                transform: 1,
                layer: Layer::Frame,
                value: 0.5,
            }
            .draw()
            .unwrap();
            assert_eq!(pixels.len(), 1920 * 1080 * 4);
        }
        _ => panic!("no probe {act}"),
    }
}

fn run_probe(probe: &str) -> ! {
    let (confined, act) = probe.split_once(':').unwrap();
    let svg =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mark.svg"))
            .unwrap();
    let parent = rustix::process::getppid().unwrap();
    if confined == "on" {
        confine(Duration::from_secs(5)).unwrap();
        lock_down().unwrap();
    }
    attempt(act, &svg, parent);
    std::process::exit(0);
}

// Run by `Helper` as its helper: the logo names the act, which runs after
// lock_down as a parser bug in the real helper would.
fn crash_as_helper() -> ! {
    vigil_secret::exit_on_panic();
    let mut act = String::new();
    std::io::stdin().read_to_string(&mut act).unwrap();
    confine(Duration::from_secs(5)).unwrap();
    lock_down().unwrap();
    match act.as_str() {
        "panic" => panic!("a parser bug"),
        "fault" => {
            // SAFETY: address 8 is never mapped, so the volatile read faults
            // before it returns, which is the parser bug this act stands for.
            #[allow(unsafe_code)]
            let _ = unsafe { std::ptr::read_volatile(std::ptr::without_provenance::<u8>(8)) };
        }
        "open" => drop(std::fs::File::open("/proc/self/status")),
        _ => {}
    }
    std::process::exit(0);
}

fn crash_through_helper(act: &str) -> Error {
    let helper = Helper::new(std::env::current_exe().unwrap(), Duration::from_secs(300));
    helper.mask(act.as_bytes(), Format::Svg, 8).unwrap_err()
}

fn main() {
    if let Some(probe) = std::env::var_os(PROBE) {
        run_probe(&probe.into_string().unwrap());
    }
    if std::env::args().nth(1).as_deref() == Some("mask") {
        crash_as_helper();
    }
    for act in ["open", "socket", "signal", "ptrace"] {
        let free = probe(false, act);
        assert!(free.success(), "{act} without the sandbox: {free:?}");
        let confined = probe(true, act);
        assert_eq!(
            confined.signal(),
            Some(libc::SIGSYS),
            "{act} in the sandbox: {confined:?}"
        );
        vigil_log::out!("{act}: allowed outside the sandbox, SIGSYS inside it");
    }
    let drawn = probe(true, "draw");
    assert!(drawn.success(), "a frame drawn in the sandbox: {drawn:?}");
    vigil_log::out!("draw: a frame is drawn and the process exits 0 inside the sandbox");
    match crash_through_helper("panic") {
        Error::Refused(m) => assert!(m.contains("panic at"), "{m}"),
        other => panic!("a panic in the sandbox: {other:?}"),
    }
    match crash_through_helper("fault") {
        Error::Refused(m) => assert!(m.contains("signal 11"), "{m}"),
        other => panic!("a fault in the sandbox: {other:?}"),
    }
    match crash_through_helper("open") {
        Error::Helper(m) => assert!(m.contains("system call"), "{m}"),
        other => panic!("a forbidden call in the sandbox: {other:?}"),
    }
    vigil_log::out!("crashes: a panic or a fault is a refusal, a forbidden call a helper fault");
}
