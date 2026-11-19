#![allow(dead_code)]

const OPEN: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">"#;

pub fn svg(body: &str) -> String {
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

fn curves(n: usize) -> String {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % 101
    };
    let mut d = String::from("M50 50");
    for _ in 0..n {
        let c: Vec<u64> = (0..6).map(|_| next()).collect();
        d += &format!(" C{} {} {} {} {} {}", c[0], c[1], c[2], c[3], c[4], c[5]);
    }
    d
}

pub type Family = fn(usize) -> String;

fn rotated_rects(n: usize, paint: &str) -> String {
    svg(&format!(
        r#"<rect x="-20" y="-20" width="140" height="140" {paint} transform="rotate(37 50 50)"/>"#
    )
    .repeat(n))
}

const STOPS: &str = r##"<stop offset="0" stop-color="#000"/><stop offset=".5" stop-color="#f00" stop-opacity=".5"/><stop offset="1" stop-color="#00f"/>"##;

// Rects over the whole box, each painted by one gradient of a kind and spread.
fn gradient(kind: &str, spread: &str, stops: &str, n: usize) -> String {
    let at = r#"cx="50" cy="50" r="3" gradientUnits="userSpaceOnUse""#;
    let open = match kind {
        "linear" => format!(r#"<linearGradient id="g" x2="0.01" spreadMethod="{spread}">"#),
        "radial" => format!(r#"<radialGradient id="g" {at} spreadMethod="{spread}">"#),
        _ => format!(
            r#"<radialGradient id="g" {at} fx="52" fy="50" fr="1" spreadMethod="{spread}">"#
        ),
    };
    let close = if kind == "linear" {
        "</linearGradient>"
    } else {
        "</radialGradient>"
    };
    svg(&format!(
        "{open}{stops}{close}{}",
        r#"<rect width="100" height="100" fill="url(#g)"/>"#.repeat(n)
    ))
}

fn many_stops(n: usize) -> String {
    let stops: String = (0..50)
        .map(|i| {
            format!(
                r##"<stop offset="{}" stop-color="#{:06x}"/>"##,
                f64::from(i) / 50.0,
                (i * 40503) % 0xff_ffff
            )
        })
        .collect();
    gradient("linear", "reflect", &stops, n)
}

fn focal_repeat(n: usize) -> String {
    gradient("focal", "repeat", STOPS, n)
}

fn used_path(uses: usize, paint: &str) -> String {
    svg(&format!(
        r##"<defs><path id="p" {paint} d="{}"/></defs>{}"##,
        zigzag(40_000),
        r##"<use href="#p"/>"##.repeat(uses)
    ))
}

// The round 2 review's inputs, each at the size that stalled or overran.
pub fn review_inputs() -> Vec<(&'static str, String)> {
    vec![
        (
            "B1 a descendant selector over 58 nested groups",
            svg(&format!(
                r#"<style>h g g g g g g g g{{fill:red}}</style>{}<rect width="100" height="100"/>{}"#,
                "<g>".repeat(58),
                "</g>".repeat(58)
            )),
        ),
        (
            "B2 a 260 KB stroked path used 4000 times",
            used_path(4000, r##"fill="none" stroke="#000" stroke-width="4""##),
        ),
        (
            "B2 a 260 KB filled path used 4000 times",
            used_path(4000, ""),
        ),
        (
            "C1 289 rotated rects at half fill opacity",
            rotated_rects(289, r#"fill-opacity=".5""#),
        ),
        (
            "C2 114 rects under a focal radial gradient that repeats",
            focal_repeat(114),
        ),
    ]
}

pub const FAMILIES: &[(&str, usize, Family)] = &[
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
    ("filled curves", 5000, |n| {
        svg(&format!(r#"<path d="{}"/>"#, curves(n)))
    }),
    ("wide stroked curves", 5000, |n| {
        svg(&format!(
            r##"<path d="{}" fill="none" stroke="#000" stroke-width="40"/>"##,
            curves(n)
        ))
    }),
    ("a stroke under a squashing scale", 5000, |n| {
        svg(&format!(
            r##"<g transform="scale(0.1 10)"><path d="{}" fill="none" stroke="#000" stroke-width="40" stroke-linejoin="round"/></g>"##,
            zigzag(n)
        ))
    }),
    ("nested layers past the canvas", 60, |n| {
        svg(&format!(
            r#"{}<rect x="299" y="299" width="500" height="500"/>{}"#,
            r#"<g opacity="0.9">"#.repeat(n),
            "</g>".repeat(n)
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
    ("alpha fills, the review's C1", 289, |n| {
        rotated_rects(n, r#"fill-opacity=".5""#)
    }),
    ("linear pad", 3000, |n| gradient("linear", "pad", STOPS, n)),
    ("linear reflect", 3000, |n| {
        gradient("linear", "reflect", STOPS, n)
    }),
    ("linear repeat", 3000, |n| {
        gradient("linear", "repeat", STOPS, n)
    }),
    ("radial pad", 3000, |n| gradient("radial", "pad", STOPS, n)),
    ("radial reflect", 3000, |n| {
        gradient("radial", "reflect", STOPS, n)
    }),
    ("radial repeat", 3000, |n| {
        gradient("radial", "repeat", STOPS, n)
    }),
    ("focal pad", 3000, |n| gradient("focal", "pad", STOPS, n)),
    ("focal reflect", 3000, |n| {
        gradient("focal", "reflect", STOPS, n)
    }),
    ("focal repeat, the review's C2", 114, focal_repeat),
    ("50 stops reflected", 3000, many_stops),
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
