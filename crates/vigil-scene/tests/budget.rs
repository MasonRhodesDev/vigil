use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use vigil_scene::{Error, Logo, MAX_EDGE};

const OPEN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">"#;
const CHILD: &str = "VIGIL_SCENE_BUDGET_CHILD";

fn svg(body: &str) -> String {
    format!("{OPEN}{body}</svg>")
}

fn zigzag(n: usize) -> String {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % 101
    };
    let mut d = String::from("M50 50");
    for _ in 0..n {
        d += &format!(" L{} {}", next(), next());
    }
    d
}

type Family = fn(usize) -> String;

const FAMILIES: &[(&str, usize, Family)] = &[
    ("stroked path", 60_000, |n| {
        svg(&format!(
            r##"<path d="{}" fill="none" stroke="#000" stroke-width="40" stroke-linejoin="round"/>"##,
            zigzag(n)
        ))
    }),
    ("filled path", 60_000, |n| {
        svg(&format!(r#"<path d="{}"/>"#, zigzag(n)))
    }),
    ("use of an opacity rect", 9000, |n| {
        svg(&format!(
            r##"<defs><rect id="r" width="100" height="100" opacity="0.5"/></defs>{}"##,
            r##"<use href="#r"/>"##.repeat(n)
        ))
    }),
    ("use of an opaque rect", 9000, |n| {
        svg(&format!(
            r##"<defs><rect id="r" width="100" height="100"/></defs>{}"##,
            r##"<use href="#r"/>"##.repeat(n)
        ))
    }),
    ("nested opacity groups", 60, |n| {
        svg(&format!(
            r#"{}<rect width="100" height="100"/>{}"#,
            r#"<g opacity="0.9">"#.repeat(n),
            "</g>".repeat(n)
        ))
    }),
    ("clipPath chain", 126, |n| {
        let mut body = String::new();
        for i in 0..n {
            let next = if i + 1 < n {
                format!(r#" clip-path="url(#c{})""#, i + 1)
            } else {
                String::new()
            };
            body += &format!(
                r#"<clipPath id="c{i}"{next}><rect width="100" height="100"/></clipPath>"#
            );
        }
        svg(&(body + r#"<rect width="100" height="100" clip-path="url(#c0)"/>"#))
    }),
    ("mask chain", 126, |n| {
        let mut body = String::new();
        for i in 0..n {
            let next = if i + 1 < n {
                format!(r#" mask="url(#m{})""#, i + 1)
            } else {
                String::new()
            };
            body += &format!(
                r##"<mask id="m{i}"{next}><rect width="100" height="100" fill="#fff"/></mask>"##
            );
        }
        svg(&(body + r#"<rect width="100" height="100" mask="url(#m0)"/>"#))
    }),
    ("gradient stops", 9000, |n| {
        let stops: String = (0..n)
            .map(|i| {
                format!(
                    r##"<stop offset="{}" stop-color="#{:06x}"/>"##,
                    i as f64 / n as f64,
                    (i * 7919) % 0xff_ffff
                )
            })
            .collect();
        svg(&format!(
            r#"<linearGradient id="g">{stops}</linearGradient><rect width="100" height="100" fill="url(#g)"/>"#
        ))
    }),
];

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
