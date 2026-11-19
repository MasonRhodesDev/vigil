mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{FAMILIES, Family};
use vigil_scene::{Error, Logo, MAX_EDGE, Plate, Scene};

const CHILD: &str = "VIGIL_SCENE_BUDGET_CHILD";

fn accepted(text: &str) -> bool {
    match Logo::svg(text.as_bytes()) {
        Ok(_) => true,
        Err(Error::Refused(_)) => false,
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn the_reviews_inputs_are_refused_before_drawing() {
    for (what, n, family) in FAMILIES {
        let text = family(*n);
        let start = Instant::now();
        let why = match Logo::svg(text.as_bytes()) {
            Err(Error::Refused(m)) => m,
            other => panic!("{what} x{n}: {:?}", other.map(|_| "accepted")),
        };
        assert!(
            why.contains("work") || why.contains("memory"),
            "{what}: {why}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{what} took {:?} to refuse",
            start.elapsed()
        );
    }
}

fn largest_accepted(family: Family, most: usize) -> usize {
    assert!(accepted(&family(1)), "one element is always within budget");
    let (mut good, mut bad) = (1, most + 1);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if accepted(&family(mid)) {
            good = mid;
        } else {
            bad = mid;
        }
    }
    good
}

fn peak_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[test]
fn child_draws_one_logo() {
    let Some(path) = std::env::var_os(CHILD) else {
        return;
    };
    let data = std::fs::read(path).unwrap();
    let start = Instant::now();
    let mask = Logo::svg(&data).unwrap().mask(MAX_EDGE).unwrap();
    let took = start.elapsed();
    assert_eq!(mask.edge(), MAX_EDGE);
    println!("DREW {} {}", took.as_millis(), peak_rss_kib());
}

fn draw_in_child(path: &Path) -> (Duration, u64) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_draws_one_logo", "--nocapture"])
        .args(["--test-threads", "1"])
        .env(CHILD, path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = stdout
        .lines()
        .find_map(|l| l.split_once("DREW ").map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("{stdout}"));
    let (ms, kib) = line.split_once(' ').unwrap();
    (
        Duration::from_millis(ms.parse().unwrap()),
        kib.parse().unwrap(),
    )
}

// Timed in release only: an unoptimised tiny-skia is tens of times slower,
// so a debug run would measure the build, not the budget.
#[test]
#[cfg_attr(debug_assertions, ignore = "timed; run with --release")]
fn the_largest_accepted_inputs_draw_quickly_in_bounded_memory() {
    let dir = std::env::temp_dir().join(format!("vigil-scene-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (i, (what, most, family)) in FAMILIES.iter().enumerate() {
        let n = largest_accepted(*family, *most);
        let path = dir.join(format!("{i}.svg"));
        std::fs::write(&path, family(n)).unwrap();
        let (took, kib) = draw_in_child(&path);
        println!("{what}: {n} accepted, drawn at {MAX_EDGE} in {took:?}, peak {kib} KiB");
        assert!(took < Duration::from_secs(4), "{what} x{n}: {took:?}");
        assert!(kib < 512 * 1024, "{what} x{n}: peak {kib} KiB");
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timed; run with --release")]
fn the_reviews_glow_specs_draw_quickly() {
    let mark =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mark.svg"))
            .unwrap();
    let logo = Logo::svg(&mark).unwrap();
    for spec in [
        r#"{"version":2,"geometry":{"glow_blur_div":1,"glow_mult":2}}"#,
        r#"{"version":2,"geometry":{"glow_blur_div":1,"glow_mult":2,"logo_frac":0.9}}"#,
        r#"{"version":2,"geometry":{"logo_frac":0.9}}"#,
    ] {
        let scene = Scene::parse(spec).unwrap();
        let start = Instant::now();
        let plate = Plate::new(&scene, &logo, (4096, 2160), 0).unwrap();
        let frame = plate.frame(0.42);
        let took = start.elapsed();
        println!("{spec}: {took:?}");
        assert_eq!(frame.len(), 4096 * 2160 * 4);
        assert!(took < Duration::from_secs(2), "{spec}: {took:?}");
    }
}
