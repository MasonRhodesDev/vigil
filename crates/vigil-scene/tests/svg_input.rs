use std::time::{Duration, Instant};

use vigil_scene::{Error, Logo, MAX_LOGO_BYTES};

const OPEN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 100 100">"#;

fn svg(body: &str) -> String {
    format!("{OPEN}{body}</svg>")
}

fn refused(text: &str) -> String {
    match Logo::svg(text.as_bytes()) {
        Err(Error::Refused(m) | Error::Invalid(m) | Error::TooLarge(m)) => m,
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("accepted: {}", &text[..text.len().min(200)]),
    }
}

fn coverage(text: &str, edge: u32) -> usize {
    let mask = Logo::svg(text.as_bytes()).unwrap().mask(edge).unwrap();
    mask.alpha().iter().filter(|&&a| a > 0).count()
}

#[test]
fn ordinary_logos_load() {
    assert!(coverage(&svg(r#"<path d="M50 6 L94 90 L50 70 L6 90 Z"/>"#), 64) > 500);
    let inkscape = r##"<?xml version="1.0"?><!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
        <svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd" viewBox="0 0 10 10">
        <sodipodi:namedview id="nv"/><defs><linearGradient id="g"><stop offset="0"/></linearGradient>
        <clipPath id="c"><rect width="8" height="8"/></clipPath><symbol id="s"><circle cx="5" cy="5" r="4"/></symbol></defs>
        <g clip-path="url(#c)" style="stroke-dasharray:none"><use xlink:href="#s"/><rect width="3" height="3" fill="url(#g)"/></g>
        <title>logo</title></svg>"##;
    assert!(coverage(inkscape, 32) > 100);
    let nested = (0..50).map(|_| "<g>").collect::<String>()
        + "<rect width=\"100\" height=\"100\"/>"
        + &(0..50).map(|_| "</g>").collect::<String>();
    assert_eq!(coverage(&svg(&nested), 16), 256);
}

#[test]
fn deep_nesting_is_refused_without_recursing() {
    let depth = 9000;
    let body = "<g>".repeat(depth) + &"</g>".repeat(depth);
    assert!(refused(&svg(&body)).contains("deeper"));
}

#[test]
fn reference_fan_out_is_refused() {
    let mut body = String::from(r#"<defs><rect id="l0" width="1" height="1"/>"#);
    for level in 1..10 {
        body += &format!(r#"<g id="l{level}">"#);
        for _ in 0..10 {
            body += &format!(r##"<use href="#l{}"/>"##, level - 1);
        }
        body += "</g>";
    }
    body += r##"</defs><use href="#l9"/>"##;
    assert!(refused(&svg(&body)).contains("expands"));

    let mut clips = String::from(r#"<clipPath id="c">"#);
    clips += &r#"<rect width="1" height="1"/>"#.repeat(1000);
    clips += "</clipPath>";
    clips += &r#"<rect width="9" height="9" clip-path="url(#c)"/>"#.repeat(100);
    assert!(refused(&svg(&clips)).contains("expands"));
}

#[test]
fn fan_out_cannot_hide_behind_decoys_or_whitespace() {
    let fan = |decoy: &str, href: &dyn Fn(usize) -> String| {
        let mut body = String::from(r#"<defs><rect id="l0" width="1" height="1"/>"#);
        for level in 1..10 {
            body += &format!(r#"<g id="l{level}">"#);
            for _ in 0..10 {
                body += &format!(r#"<use href="{}"/>"#, href(level - 1));
            }
            body += "</g>";
            body += &decoy.replace("ID", &format!("l{level}"));
        }
        body + &format!(r#"</defs><use href="{}"/>"#, href(9))
    };
    let plain = |l: usize| format!("#l{l}");
    let spaced = |l: usize| format!(" #l{l}");
    for (what, body) in [
        (
            "svg decoy",
            fan(r#"<rect id="ID" width="1" height="1"/>"#, &plain),
        ),
        (
            "foreign decoy",
            fan(r#"<x:decoy xmlns:x="urn:x" id="ID"/>"#, &plain),
        ),
        ("leading space", fan("", &spaced)),
    ] {
        let why = refused(&svg(&body));
        assert!(
            why.contains("expands") || why.contains("id"),
            "{what}: {why}"
        );
    }
}

#[test]
fn style_sheets_may_paint_with_gradients_but_not_fan_out() {
    let gradient = r##"<defs><style>.a{fill:url(#g)}</style><linearGradient id="g"><stop offset="0" stop-color="#fff"/></linearGradient></defs><rect class="a" width="100" height="100"/>"##;
    assert_eq!(coverage(&svg(gradient), 8), 64);
    let mut clips = String::from(r#"<style>rect { clip-path: url(#c) }</style><clipPath id="c">"#);
    clips += &r#"<circle r="1"/>"#.repeat(200);
    clips += "</clipPath>";
    clips += &r#"<rect width="9" height="9"/>"#.repeat(200);
    assert!(refused(&svg(&clips)).contains("expands"));
}

#[test]
fn reference_loops_are_refused() {
    assert!(refused(&svg(r##"<g id="a"><use href="#a"/></g>"##)).contains("loop"));
    assert!(
        refused(&svg(
            r##"<clipPath id="a" clip-path="url(#b)"/><clipPath id="b" clip-path="url(#a)"/>"##
        ))
        .contains("loop")
    );
}

#[test]
fn entities_svgz_and_oversize_are_refused() {
    let laughs = r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">]><svg xmlns="http://www.w3.org/2000/svg"><title>&b;</title></svg>"#;
    assert!(refused(laughs).contains("entities"));
    let svgz = [0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
    assert!(matches!(Logo::svg(&svgz), Err(Error::Invalid(_))));
    let big = svg(&" ".repeat(MAX_LOGO_BYTES as usize));
    assert!(matches!(Logo::svg(big.as_bytes()), Err(Error::TooLarge(_))));
    let dir = std::env::temp_dir().join(format!("vigil-scene-svg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.svg");
    std::fs::write(&path, &big).unwrap();
    assert!(matches!(Logo::load(&path), Err(Error::TooLarge(_))));
    assert!(matches!(Logo::load(&dir), Err(Error::Refused(_))));
}

#[test]
fn nothing_outside_the_svg_is_read() {
    let dir = std::env::temp_dir().join(format!("vigil-scene-href-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let secret = dir.join("secret.svg");
    std::fs::write(&secret, svg(r#"<rect width="100" height="100"/>"#)).unwrap();
    let href = secret.to_str().unwrap();
    for body in [
        format!(r#"<image href="{href}" width="100" height="100"/>"#),
        format!(r#"<image xlink:href="file://{href}" width="100" height="100"/>"#),
        r#"<image href="data:image/svg+xml;base64,PHN2Zy8+" width="9" height="9"/>"#.into(),
        r#"<foreignObject width="9" height="9"/>"#.into(),
        r#"<filter id="f"><feGaussianBlur stdDeviation="1e30"/></filter>"#.into(),
        r#"<pattern id="p" width="1e-9" height="1e-9"/>"#.into(),
        r#"<marker id="m"/>"#.into(),
        r#"<text>logo</text>"#.into(),
        r#"<script>alert(1)</script>"#.into(),
    ] {
        refused(&svg(&body));
    }
}

#[test]
fn dashed_strokes_are_refused() {
    let dashed =
        r##"<path d="M0 0 L100 100" stroke="#000" stroke-width="1" stroke-dasharray="0.0001"/>"##;
    assert!(refused(&svg(dashed)).contains("dashed"));
    let in_style =
        r##"<path d="M0 0 L100 100" style="stroke:#000;stroke-dasharray:0.001,0.001"/>"##;
    assert!(refused(&svg(in_style)).contains("dashed"));
}

#[test]
fn shapes_far_outside_the_box_are_refused() {
    for body in [
        r##"<ellipse cx="50" cy="50" rx="01e38" ry="20" stroke="#000"/>"##,
        r#"<rect width="2000000" height="9"/>"#,
        r#"<rect width="9" height="9" style="stroke-width:1e9"/>"#,
        r#"<g transform="rotate(30 50 50)"><polygon points="1-1e38,10 90,10 50,90"/></g>"#,
        r#"<path d="M-1e38 -1e38 L1e38 1e38 L-1e38 1e38 Z"/>"#,
        r##"<path d="M0 0 L100 100" stroke="#000" stroke-width="1e30"/>"##,
        r#"<rect width="100" height="100" transform="scale(1e6)"/>"#,
        r#"<clipPath id="c"><rect x="-1e9" width="9" height="9"/></clipPath><rect width="9" height="9" clip-path="url(#c)"/>"#,
    ] {
        let why = refused(&svg(body));
        assert!(
            why.contains("far outside") || why.contains("beyond"),
            "{body}: {why}"
        );
    }
    assert!(
        coverage(
            &svg(r#"<rect x="-500" y="-500" width="1100" height="1100"/>"#),
            8
        ) == 64
    );
}

#[test]
fn extreme_numbers_render_in_bounded_time() {
    for body in [
        r#"<path d="M-1e38 -1e38 L1e38 1e38 L-1e38 1e38 Z"/>"#,
        r##"<path d="M0 0 L100 100" stroke="#000" stroke-width="1e30"/>"##,
        r#"<circle cx="50" cy="50" r="1e-30"/>"#,
        r#"<rect width="100" height="100" transform="scale(1e-30)"/>"#,
        r#"<rect width="100" height="100" transform="rotate(1e30)"/>"#,
    ] {
        let start = Instant::now();
        if let Ok(logo) = Logo::svg(svg(body).as_bytes()) {
            let _ = logo.mask(512);
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{body}");
    }
    let huge = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1e30" height="1e30"><rect width="1e30" height="1e30"/></svg>"#;
    if let Ok(logo) = Logo::svg(huge.as_bytes()) {
        let _ = logo.mask(512);
    }
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const SEEDS: &[&str] = &[
    r##"<path d="M50 6 L94 90 L50 70 L6 90 Z" fill="#1793d1"/>"##,
    r##"<defs><linearGradient id="a"><stop offset="0" stop-color="#fff"/></linearGradient><clipPath id="c"><circle cx="50" cy="50" r="40"/></clipPath></defs><g clip-path="url(#c)"><rect id="r" width="80" height="80" fill="url(#a)"/><use href="#r" x="10"/></g>"##,
    r##"<g transform="rotate(30 50 50) scale(0.8)"><ellipse cx="50" cy="50" rx="40" ry="20" stroke="#000" stroke-width="4"/><polygon points="10,10 90,10 50,90"/></g>"##,
    r##"<mask id="m"><rect width="100" height="100" fill="#fff"/></mask><path d="M10 10 C 20 80, 80 80, 90 10 Q 50 50 10 10" mask="url(#m)"/>"##,
];

const TOKENS: &[&str] = &[
    "<g>",
    "</g>",
    r##"<use href="#r"/>"##,
    r##"<use href="#c"/>"##,
    r#" id="r""#,
    r#" id="c""#,
    "url(#c)",
    "url(#r)",
    "<!ENTITY",
    r#" stroke-dasharray="1""#,
    r#" stroke-width="1e38""#,
    " transform=\"matrix(1e38 0 0 1e38 0 0)\"",
    "1e38",
    "-1e38",
    "NaN",
    "inf",
    "<image href=\"x.png\"/>",
    "<![CDATA[",
    "&#0;",
    "\u{feff}",
    "<svg>",
    "</svg>",
    "<path d=\"",
    "M0 0",
    "A1e9 1e9 0 1 1",
];

const NUMBERS: &[&str] = &[
    "0", "-1", "1e38", "-1e38", "3.4e39", "1e-38", "NaN", "65535", "0.0001",
];

fn boundary(rng: &mut XorShift, data: &[u8]) -> usize {
    let ends: Vec<usize> = (0..data.len()).filter(|&i| data[i] == b'>').collect();
    ends.get(rng.below(ends.len()))
        .map_or(data.len(), |&i| i + 1)
}

fn mutate(rng: &mut XorShift, mut data: Vec<u8>) -> Vec<u8> {
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(data.len() + 1);
        match rng.below(8) {
            0 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
            1 | 2 => {
                let t = TOKENS[rng.below(TOKENS.len())].as_bytes();
                let at = if t[0] == b'<' {
                    boundary(rng, &data)
                } else {
                    at
                };
                data.splice(at..at, t.iter().copied());
            }
            3 | 4 => {
                let digits: Vec<usize> = (0..data.len())
                    .filter(|&i| data[i].is_ascii_digit())
                    .collect();
                if let Some(&i) = digits.get(rng.below(digits.len())) {
                    let n = NUMBERS[rng.below(NUMBERS.len())].as_bytes();
                    data.splice(i..i + 1, n.iter().copied());
                }
            }
            5 if at < data.len() => {
                let end = (at + rng.below(32)).min(data.len());
                data.drain(at..end);
            }
            6 => {
                let from = boundary(rng, &data);
                let to = boundary(rng, &data).max(from);
                let span = data[from..to.min(from + 256)].to_vec();
                for _ in 0..1 + rng.below(8) {
                    data.splice(from..from, span.iter().copied());
                }
            }
            _ => {
                let t = TOKENS[rng.below(TOKENS.len())].as_bytes();
                data.extend_from_slice(t);
            }
        }
    }
    data
}

#[test]
fn mutated_svgs_fail_cleanly_and_quickly() {
    let cases = std::env::var("VIGIL_SCENE_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000usize);
    let seed = std::env::var("VIGIL_SCENE_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x9e37_79b9_7f4a_7c15u64);
    let mut rng = XorShift(seed | 1);
    let started = Instant::now();
    let (mut loaded, mut slowest) = (0, Duration::ZERO);
    for case in 0..cases {
        let seed = svg(SEEDS[case % SEEDS.len()]).into_bytes();
        let data = mutate(&mut rng, seed);
        let start = Instant::now();
        if let Ok(logo) = Logo::svg(&data) {
            loaded += 1;
            let _ = logo.mask(97);
        }
        let took = start.elapsed();
        slowest = slowest.max(took);
        assert!(
            took < Duration::from_secs(3),
            "case {case} took {took:?}: {}",
            String::from_utf8_lossy(&data)
        );
    }
    assert!(
        loaded > cases / 10,
        "only {loaded} of {cases} mutants loaded"
    );
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "{cases} cases took {:?}, slowest {slowest:?}",
        started.elapsed()
    );
}

#[test]
fn raster_logos_keep_their_own_larger_cap() {
    let mut rng = XorShift(7);
    let noisy = image::RgbaImage::from_fn(640, 640, |x, y| {
        let inside = (x as i64 - 320).pow(2) + (y as i64 - 320).pow(2) < 250 * 250;
        let n = rng.next();
        image::Rgba([
            n as u8,
            (n >> 8) as u8,
            (n >> 16) as u8,
            if inside { 255 } else { 0 },
        ])
    });
    let dir = std::env::temp_dir().join(format!("vigil-scene-raster-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("logo.png");
    noisy.save(&path).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > MAX_LOGO_BYTES);
    let mask = Logo::load(&path).unwrap().mask(64).unwrap();
    assert!(mask.alpha().iter().filter(|&&a| a == 255).count() > 1500);
}
