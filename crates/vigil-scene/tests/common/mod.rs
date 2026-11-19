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
