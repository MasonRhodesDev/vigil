#![deny(unsafe_code)]

use vigil_scene_corpus as common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vigil_scene::{
    DEFAULT_DEADLINE, Error, Format, Helper, Layer, Logo, MAX_EDGE, Plate, Request, Scene,
};

const BIN: &str = env!("CARGO_BIN_EXE_vigil-scene");
const CHILD: &str = "VIGIL_SCENE_HELPER_TEST";
const PATIENT: Duration = Duration::from_secs(300);
const SLACK: Duration = Duration::from_millis(500);

fn mark() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mark.svg"))
        .unwrap()
}

fn request(svg: &[u8], layer: Layer) -> Request<'_> {
    Request {
        svg: Some(svg),
        spec: None,
        mode: (1920, 1080),
        transform: 0,
        layer,
        value: 0.5,
    }
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn children() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.unwrap().path().join("children")).ok())
        .flat_map(|c| c.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .collect()
}

fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

// Descriptor counts, children and peak RSS belong to the whole process, so
// each check runs alone in a fresh copy of this test binary.
fn in_fresh_process(test: &str, arg: &str) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads", "1"])
        .env(CHILD, arg)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
fn the_helper_draws_what_the_library_draws() {
    let svg = mark();
    let logo = Logo::svg(&svg).unwrap();
    let plate = Plate::new(&Scene::default(), &logo, (1080, 1920), 3).unwrap();
    let helper = Helper::new(BIN, PATIENT);
    for (layer, want) in [
        (Layer::Frame, plate.frame(0.5)),
        (Layer::Logo, plate.logo_sprite().unwrap().rgba),
        (Layer::Glow, plate.glow_sprite(0.5).unwrap().rgba),
        (Layer::Bar, plate.bar_sprite(0.5).unwrap().rgba),
    ] {
        let mut req = request(&svg, layer);
        req.transform = 3;
        let mut out = vec![0u8; want.len()];
        helper.draw(&req, &mut out).unwrap();
        assert!(out == want, "{layer:?}");
    }
    let mask = helper.mask(&svg, Format::Svg, 64).unwrap();
    assert_eq!(mask, logo.mask(64).unwrap());
}

#[test]
fn the_helpers_refusal_reaches_the_caller() {
    let hostile = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="/etc/passwd"/></svg>"#;
    let helper = Helper::new(BIN, PATIENT);
    let mut out = vec![0u8; 173 * 173 * 4];
    match helper.draw(&request(hostile, Layer::Logo), &mut out) {
        Err(Error::Refused(m)) => assert!(m.contains("<image>"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(out.iter().all(|&b| b == 0), "nothing written on a refusal");
    let mut short = vec![0u8; 10];
    assert!(matches!(
        helper.draw(&request(&mark(), Layer::Logo), &mut short),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn a_missing_helper_is_its_own_error_and_the_ground_needs_none() {
    let helper = Helper::new("/nonexistent/vigil-scene", PATIENT);
    let svg = mark();
    let mut out = vec![0u8; 173 * 173 * 4];
    assert!(matches!(
        helper.draw(&request(&svg, Layer::Logo), &mut out),
        Err(Error::Helper(_))
    ));
    assert!(matches!(
        helper.mask(&svg, Format::Svg, 64),
        Err(Error::Helper(_))
    ));

    let plate = Plate::ground_only(&Scene::default(), (1920, 1080), 0);
    let want = plate.bar_sprite(0.5).unwrap().rgba;
    let mut req = request(&svg, Layer::Bar);
    req.svg = None;
    let mut out = vec![0u8; want.len()];
    helper.draw(&req, &mut out).unwrap();
    assert!(out == want);
}

#[test]
fn a_size_query_never_reads_the_logo() {
    let hostile = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="/etc/passwd"/></svg>"#;
    let at = request(hostile, Layer::Logo).place().unwrap();
    assert_eq!((at.x, at.y, at.width, at.height), (874, 421, 173, 173));
}

#[test]
fn child_stops_a_helper_past_its_deadline() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let svg = mark();
    let mut req = request(&svg, Layer::Frame);
    req.mode = (4096, 4096);
    let mut out = vec![0u8; 4096 * 4096 * 4];
    let (fds, before) = (open_fds(), children());
    let start = Instant::now();
    let got = Helper::new(BIN, Duration::from_millis(1)).draw(&req, &mut out);
    let took = start.elapsed();
    match got {
        Err(Error::Timeout(m)) => assert!(m.contains("deadline"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(took < SLACK, "returned after {took:?}");
    assert_eq!(open_fds(), fds, "a descriptor leaked");
    assert_eq!(children(), before, "a child was left unreaped");
    assert!(out.iter().all(|&b| b == 0), "nothing written when stopped");
}

#[test]
fn a_helper_past_its_deadline_is_killed_and_reaped() {
    in_fresh_process("child_stops_a_helper_past_its_deadline", "1");
}

#[test]
fn child_runs_one_review_input() {
    let Some(index) = std::env::var_os(CHILD) else {
        return;
    };
    let index: usize = index.to_str().unwrap().parse().unwrap();
    let (what, text) = &common::review_inputs()[index];
    let helper = Helper::new(BIN, DEFAULT_DEADLINE);
    let base = peak_rss_kib();
    let start = Instant::now();
    let mask = helper.mask(text.as_bytes(), Format::Svg, MAX_EDGE);
    let mut out = vec![0u8; 173 * 173 * 4];
    let drawn = helper.draw(&request(text.as_bytes(), Layer::Logo), &mut out);
    let took = start.elapsed();
    let grew = peak_rss_kib().saturating_sub(base);
    for got in [mask.as_ref().err(), drawn.as_ref().err()] {
        assert!(
            !matches!(got, Some(Error::Helper(_))),
            "{what}: the helper failed, not the logo: {got:?}"
        );
    }
    vigil_log::out!(
        "REVIEW {what}: mask {:?}, layer {:?}, {took:?}, parent grew {grew} KiB",
        mask.as_ref().map(|_| "drawn"),
        drawn.as_ref().map(|_| "drawn")
    );
    assert!(
        took < 2 * DEFAULT_DEADLINE + SLACK,
        "{what}: two calls took {took:?}"
    );
    assert!(grew < 32 * 1024, "{what}: the parent grew by {grew} KiB");
    assert_eq!(children(), Vec::<String>::new());
}

#[test]
fn every_review_input_returns_by_its_deadline_in_bounded_memory() {
    for i in 0..common::review_inputs().len() {
        let out = in_fresh_process("child_runs_one_review_input", &i.to_string());
        for line in out
            .lines()
            .filter_map(|l| l.find("REVIEW ").map(|i| &l[i..]))
        {
            vigil_log::out!("{line}");
        }
    }
}

#[test]
fn child_draws_the_ground_in_place() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let mut req = request(&[], Layer::Frame);
    req.svg = None;
    req.mode = (4096, 4096);
    let mut out = vec![1u8; req.bytes().unwrap()];
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    let base = peak_rss_kib();
    Helper::new("/nonexistent", PATIENT)
        .draw(&req, &mut out)
        .unwrap();
    let grew = peak_rss_kib().saturating_sub(base);
    let result = out.len() as u64 / 1024;
    vigil_log::out!("GROUND result {result} KiB, peak grew {grew} KiB");
    assert!(
        grew < result / 8,
        "the peak grew {grew} KiB for {result} KiB"
    );
    let bg = Scene::default().bg;
    assert_eq!(out[..4], [bg[2], bg[1], bg[0], 0xff]);
    out.fill(7);
    req.layer = Layer::Logo;
    assert!(
        Helper::new("/nonexistent", PATIENT)
            .draw(&req, &mut out)
            .is_err()
    );
    assert!(out.iter().all(|&b| b == 7), "nothing written on an error");
}

#[test]
fn the_ground_is_drawn_into_the_callers_buffer() {
    let out = in_fresh_process("child_draws_the_ground_in_place", "1");
    vigil_log::out!(
        "{}",
        out.lines().find(|l| l.contains("GROUND")).unwrap_or("")
    );
}

fn limits(pid: u32) -> Vec<(String, String, String)> {
    std::fs::read_to_string(format!("/proc/{pid}/limits"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| {
            let (name, rest) = l.split_at(26);
            let mut v = rest.split_whitespace();
            (
                name.trim().to_owned(),
                v.next().unwrap().to_owned(),
                v.next().unwrap().to_owned(),
            )
        })
        .collect()
}

fn status_field(pid: u32, key: &str) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .map(|v| v.trim().to_owned())
}

#[test]
fn the_shipped_helper_draws_under_its_syscall_filter() {
    let svg = mark();
    let filters = |status: &str| -> u32 {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .map_or(0, |v| v.trim().parse().unwrap())
    };
    let ours = filters(&std::fs::read_to_string("/proc/self/status").unwrap());
    let (reader, mut writer) = std::io::pipe().unwrap();
    let mut child = Command::new(BIN)
        .args(["layer", "--mode", "4096x4096", "--transform", "0"])
        .args(["--layer", "frame", "--value", "1", "--spec-len", "0"])
        .args(["--svg-len", &svg.len().to_string(), "--deadline-ms", "5000"])
        .args(["--abi", vigil_scene::HELPER_ABI])
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    std::io::Write::write_all(&mut writer, &svg).unwrap();
    drop(writer);
    let mut seen = None;
    while seen.is_none() && child.try_wait().unwrap().is_none() {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        if filters(&status) > ours {
            match std::fs::read(format!("/proc/{pid}/environ")) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                environ => seen = Some((status, environ)),
            }
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.is_empty() || said == "vigil-scene: notice: this kernel has no Landlock\n",
        "Landlock failed rather than being absent: {said}"
    );
    let (status, environ) = seen.expect("the helper ran under a new seccomp filter");
    assert_eq!(filters(&status), ours + 1);
    assert!(status.contains("NoNewPrivs:\t1"), "{status}");
    let caught = status
        .lines()
        .find_map(|l| l.strip_prefix("SigCgt:"))
        .map(|v| u64::from_str_radix(v.trim(), 16).unwrap())
        .unwrap();
    for signal in [libc::SIGSEGV, libc::SIGBUS] {
        assert_eq!(
            caught & 1 << (signal - 1),
            0,
            "signal {signal} still has a handler that makes calls the filter kills"
        );
    }
    if !rustix::process::getuid().is_root() {
        let denied = environ.expect_err("a helper that is not dumpable hides its environ");
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    }
}

#[test]
fn the_helper_confines_itself_before_it_reads_a_byte() {
    let (reader, writer) = std::io::pipe().unwrap();
    let inherited = File::open("/dev/null").unwrap();
    rustix::io::fcntl_setfd(&inherited, rustix::io::FdFlags::empty()).unwrap();
    let mut child = Command::new(BIN)
        .args([
            "mask",
            "--format",
            "svg",
            "--edge",
            "64",
            "--deadline-ms",
            "2500",
            "--abi",
            vigil_scene::HELPER_ABI,
        ])
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(inherited);
    let pid = child.id();
    let status = PathBuf::from(format!("/proc/{pid}/status"));
    let start = Instant::now();
    while !std::fs::read_to_string(&status)
        .unwrap()
        .contains("NoNewPrivs:\t1")
    {
        assert!(start.elapsed() < Duration::from_secs(10), "never confined");
        std::thread::sleep(Duration::from_millis(5));
    }
    let got = limits(pid);
    let limit = |name: &str| {
        got.iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, soft, hard)| (soft.as_str(), hard.as_str()))
            .unwrap()
    };
    assert_eq!(limit("Max cpu time"), ("4", "4"));
    assert_eq!(limit("Max address space"), ("1073741824", "1073741824"));
    assert_eq!(limit("Max open files"), ("3", "3"));
    assert_eq!(limit("Max core file size"), ("0", "0"));
    assert_eq!(limit("Max processes"), ("0", "0"));
    assert_eq!(limit("Max file size"), ("268439552", "268439552"));
    let mut fds: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    fds.sort();
    assert_eq!(fds, ["0", "1", "2"], "only stdio survives into the helper");
    child.kill().unwrap();
    child.wait().unwrap();
    drop(writer);
}

#[test]
fn the_helper_never_raises_a_soft_limit_it_inherits() {
    let (reader, writer) = std::io::pipe().unwrap();
    let mut child = Command::new("sh")
        .args(["-c", r#"ulimit -S -t 1 && exec "$@""#, "sh", BIN])
        .args(["mask", "--format", "svg", "--edge", "64"])
        .args(["--deadline-ms", "2500", "--abi", vigil_scene::HELPER_ABI])
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = Instant::now();
    while status_field(pid, "NoNewPrivs:").as_deref() != Some("1") {
        assert!(start.elapsed() < Duration::from_secs(10), "never confined");
        std::thread::sleep(Duration::from_millis(5));
    }
    let cpu = limits(pid)
        .into_iter()
        .find(|(name, _, _)| name == "Max cpu time")
        .map(|(_, soft, hard)| (soft, hard));
    child.kill().unwrap();
    child.wait().unwrap();
    drop(writer);
    assert_eq!(cpu, Some(("1".into(), "4".into())));
}

#[test]
fn a_helper_of_another_version_is_a_helper_fault() {
    let out = Command::new(BIN)
        .args([
            "mask",
            "--format",
            "svg",
            "--edge",
            "8",
            "--deadline-ms",
            "1000",
        ])
        .args(["--abi", "0.0.0-elsewhere"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(14), "{said}");
    assert!(said.contains("0.0.0-elsewhere"), "{said}");
}

// A script this process opened for writing cannot be run while any child
// forked by another test thread still holds that descriptor, and a rename
// keeps the inode, so a short-lived shell writes it instead.
fn fake_script(name: &str, script: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vigil-scene-fake-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let written = Command::new("sh")
        .args([
            "-c",
            r#"printf '#!/bin/sh\n%s\n' "$2" > "$1" && chmod 755 "$1""#,
            "sh",
        ])
        .arg(&path)
        .arg(script)
        .status()
        .unwrap();
    assert!(written.success(), "writing {}", path.display());
    path
}

const BRIEF: Duration = Duration::from_millis(300);

fn fake_helper(name: &str, script: &str) -> Helper {
    Helper::new(fake_script(name, script), PATIENT)
}

// Starting sh alone can take longer than BRIEF on a loaded host, so only a
// helper that never ends gets so short a deadline.
fn spinner(name: &str) -> Helper {
    Helper::new(fake_script(name, "while :; do :; done"), BRIEF)
}

#[test]
fn a_fake_helper_runs_while_other_threads_spawn() {
    let svg = mark();
    let req = request(&svg, Layer::Logo);
    let need = req.bytes().unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                }
            });
        }
        for i in 0..200 {
            let script = fake_script(&format!("busy-{i}"), &format!("printf '%0{need}d' 0"));
            let helper = Helper::new(script, PATIENT);
            let mut out = vec![0u8; need];
            let got = helper.draw(&req, &mut out);
            if got.is_err() {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            got.unwrap_or_else(|e| panic!("run {i}: {e}"));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

#[test]
fn whatever_the_helper_does_the_caller_gets_pixels_or_an_error_in_time() {
    let svg = mark();
    let req = request(&svg, Layer::Logo);
    let need = req.bytes().unwrap();
    type Expect = fn(&Error) -> bool;
    let crash = |e: &Error| matches!(e, Error::Refused(m) if m.contains("signal"));
    let spin = spinner("spins");
    let mut out = vec![0u8; need];
    let start = Instant::now();
    let got = spin.draw(&req, &mut out);
    let took = start.elapsed();
    assert!(took < BRIEF + SLACK, "spins: returned after {took:?}");
    assert!(
        matches!(&got, Err(Error::Timeout(m)) if m.contains("deadline")),
        "spins: {got:?}"
    );
    assert!(out.iter().all(|&b| b == 0), "spins: nothing written");
    let cases: [(&str, String, Expect); 11] = [
        (
            "signal",
            "kill -9 $$".into(),
            |e| matches!(e, Error::Refused(m) if m.contains("signal 9")),
        ),
        ("segv", "ulimit -c 0; kill -s SEGV $$".into(), crash),
        ("bus", "ulimit -c 0; kill -s BUS $$".into(), crash),
        ("abort", "ulimit -c 0; kill -s ABRT $$".into(), crash),
        ("ill", "ulimit -c 0; kill -s ILL $$".into(), crash),
        ("fpe", "ulimit -c 0; kill -s FPE $$".into(), crash),
        (
            "forbidden",
            "ulimit -c 0; kill -s SYS $$".into(),
            |e| matches!(e, Error::Helper(m) if m.contains("system call")),
        ),
        ("short", "printf abc".into(), |e| {
            matches!(e, Error::Invalid(_))
        }),
        ("long", format!("printf '%0{}d' 0", need + 1), |e| {
            matches!(e, Error::Invalid(_))
        }),
        (
            "unknown",
            "echo odd >&2; exit 77".into(),
            |e| matches!(e, Error::Helper(m) if m.contains("77") && m.contains("odd")),
        ),
        (
            "refuses",
            "echo 'vigil-scene: no' >&2; exit 12".into(),
            |e| *e == Error::Refused("no".into()),
        ),
    ];
    for (name, script, want) in cases {
        let helper = fake_helper(name, &script);
        let mut out = vec![0u8; need];
        let got = helper.draw(&req, &mut out);
        assert!(got.as_ref().is_err_and(want), "{name}: {got:?}");
        assert!(out.iter().all(|&b| b == 0), "{name}: nothing written");
    }
    let exact = fake_helper("exact", &format!("printf '%0{need}d' 0"));
    let mut out = vec![0u8; need];
    exact.draw(&req, &mut out).unwrap();
    assert!(out.iter().all(|&b| b == b'0'));
}

#[test]
fn child_draws_with_sigchld_ignored() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let spin = spinner("spin-ignored");
    let need = request(&[], Layer::Logo).bytes().unwrap();
    let noticed = fake_helper(
        "noticed",
        &format!("echo 'vigil-scene: notice: no Landlock' >&2; printf '%0{need}d' 0"),
    );
    // SAFETY: SIG_IGN is a valid disposition, and this process runs this one
    // test alone, so no other code relies on SIGCHLD.
    #[allow(unsafe_code)]
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }
    let svg = mark();
    let want = Plate::new(
        &Scene::default(),
        &Logo::svg(&svg).unwrap(),
        (1920, 1080),
        0,
    )
    .unwrap()
    .logo_sprite()
    .unwrap()
    .rgba;
    let mut out = vec![0u8; want.len()];
    Helper::new(BIN, PATIENT)
        .draw(&request(&svg, Layer::Logo), &mut out)
        .unwrap();
    assert!(out == want);
    let mut zeros = vec![0u8; need];
    noticed
        .draw(&request(&svg, Layer::Logo), &mut zeros)
        .expect("a notice on stderr is not a failure");
    assert!(zeros.iter().all(|&b| b == b'0'));
    let hostile = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="/etc/passwd"/></svg>"#;
    assert!(matches!(
        Helper::new(BIN, PATIENT).draw(&request(hostile, Layer::Logo), &mut out),
        Err(Error::Refused(_))
    ));
    let start = Instant::now();
    assert!(spin.draw(&request(&svg, Layer::Logo), &mut out).is_err());
    assert!(start.elapsed() < BRIEF + SLACK);
    assert_eq!(children(), Vec::<String>::new());
}

#[test]
fn a_host_that_ignores_sigchld_still_gets_its_pixels() {
    in_fresh_process("child_draws_with_sigchld_ignored", "1");
}

#[test]
fn the_helper_dies_with_the_thread_that_started_it() {
    let (reader, writer) = std::io::pipe().unwrap();
    let mut child = std::thread::spawn(move || {
        let child = Command::new(BIN)
            .args([
                "mask",
                "--format",
                "svg",
                "--edge",
                "64",
                "--deadline-ms",
                "60000",
                "--abi",
                vigil_scene::HELPER_ABI,
            ])
            .stdin(reader)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let status = format!("/proc/{}/status", child.id());
        let start = Instant::now();
        while !std::fs::read_to_string(&status)
            .unwrap()
            .contains("NoNewPrivs:\t1")
        {
            assert!(start.elapsed() < Duration::from_secs(10), "never confined");
            std::thread::sleep(Duration::from_millis(5));
        }
        child
    })
    .join()
    .unwrap();
    let status = child.wait().unwrap();
    drop(writer);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
}

#[test]
fn a_raster_logo_draws_the_same_through_the_helper() {
    use image::ImageEncoder;
    let img = image::RgbaImage::from_fn(300, 200, |x, y| {
        image::Rgba([0, 0, 0, if (x / 20 + y / 20) % 2 == 0 { 255 } else { 0 }])
    });
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(img.as_raw(), 300, 200, image::ExtendedColorType::Rgba8)
        .unwrap();
    let want = Logo::raster(&png).unwrap().mask(64).unwrap();
    let got = Helper::new(BIN, PATIENT)
        .mask(&png, Format::Raster, 64)
        .unwrap();
    assert_eq!(got, want);
}

#[test]
fn a_fifo_or_a_huge_spec_never_stalls_the_caller() {
    let dir = std::env::temp_dir().join(format!("vigil-scene-fifo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("logo.svg");
    let _ = std::fs::remove_file(&fifo);
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let start = Instant::now();
    assert!(matches!(
        vigil_scene::read_logo(&fifo),
        Err(Error::Refused(_))
    ));
    assert_eq!(Scene::load(&fifo), Scene::default());
    assert!(start.elapsed() < Duration::from_secs(1));

    let deep = "[".repeat(1 << 20);
    let mut req = request(&[], Layer::Bar);
    req.svg = None;
    req.spec = Some(deep.as_bytes());
    let start = Instant::now();
    assert!(matches!(req.place(), Err(Error::Invalid(_))));
    assert!(start.elapsed() < Duration::from_secs(1));
}
