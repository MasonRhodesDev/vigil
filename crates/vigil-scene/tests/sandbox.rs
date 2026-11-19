#![deny(unsafe_code)]

// No test harness: libtest draws on a worker thread, where glibc's first
// allocation opens /sys to size a new arena. The helper draws on its main
// thread, and so does each probe here.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use rustix::process::Pid;
use vigil_scene::{Layer, Request, confine, lock_down};

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

fn main() {
    if let Some(probe) = std::env::var_os(PROBE) {
        run_probe(&probe.into_string().unwrap());
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
        println!("{act}: allowed outside the sandbox, SIGSYS inside it");
    }
    let drawn = probe(true, "draw");
    assert!(drawn.success(), "a frame drawn in the sandbox: {drawn:?}");
    println!("draw: a frame is drawn and the process exits 0 inside the sandbox");
}
