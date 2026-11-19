#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use std::time::Duration;

use vigil_scene::{Format, Helper, Layer, Logo, Plate, Request, Scene};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn goldens() -> PathBuf {
    root().join("tests/golden/scene")
}

fn mark() -> PathBuf {
    root().join("tests/fixtures/mark.svg")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vigil-scene-golden-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn cli(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_vigil-scene"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "vigil-scene {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn pixels(path: &Path) -> (u32, u32, Vec<u8>) {
    let img = image::open(path).unwrap();
    let (w, h) = (img.width(), img.height());
    let raw = if img.color().has_alpha() {
        img.into_rgba8().into_raw()
    } else {
        img.into_rgb8().into_raw()
    };
    (w, h, raw)
}

fn same_pixels(got: &Path, want: &Path) {
    if std::env::var_os("VIGIL_SCENE_BLESS").is_some() && !want.exists() {
        std::fs::copy(got, want).unwrap();
    }
    let (g, w) = (pixels(got), pixels(want));
    assert_eq!((g.0, g.1), (w.0, w.1), "{} size", want.display());
    let off = g.2.iter().zip(&w.2).filter(|(a, b)| a != b).count();
    assert_eq!(off, 0, "{}: {off} channel values differ", want.display());
}

struct Case {
    panel: (u32, u32),
    transform: u8,
    level: f64,
    reference: u32,
}

fn splash_cases() -> Vec<(String, Case)> {
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(goldens()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        let Some(rest) = name
            .strip_prefix("splash-")
            .and_then(|r| r.strip_suffix(".png"))
        else {
            continue;
        };
        let f: Vec<&str> = rest.split('-').collect();
        let (w, h) = f[0].split_once('x').unwrap();
        cases.push((
            name.clone(),
            Case {
                panel: (w.parse().unwrap(), h.parse().unwrap()),
                transform: f[1][1..].parse().unwrap(),
                level: f64::from(f[2][1..].parse::<u32>().unwrap()) / 100.0,
                reference: f[3][1..].parse().unwrap(),
            },
        ));
    }
    cases.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(cases.len() >= 7, "only {} splash goldens", cases.len());
    cases
}

#[test]
fn cli_draws_every_splash_golden_exactly() {
    for (name, c) in splash_cases() {
        let out = scratch(&name);
        let stage = if c.level == 0.0 {
            "grub-handoff"
        } else {
            "splash"
        };
        cli(&[
            "render",
            "--stage",
            stage,
            "--mode",
            &format!("{}x{}", c.panel.0, c.panel.1),
            "--transform",
            &c.transform.to_string(),
            "--ref-short-side",
            &c.reference.to_string(),
            "--level",
            &c.level.to_string(),
            "--logo",
            mark().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]);
        same_pixels(&out, &goldens().join(&name));
    }
}

#[test]
fn sprites_drawn_at_their_places_rebuild_the_frame() {
    let logo = Logo::svg(&std::fs::read(mark()).unwrap()).unwrap();
    for (name, c) in splash_cases() {
        let spec = Scene {
            ref_short_side: (c.reference > 0).then_some(c.reference),
            ..Scene::default()
        };
        let scene = if c.transform % 2 == 1 {
            (c.panel.1, c.panel.0)
        } else {
            c.panel
        };
        let plate = Plate::new(&spec, &logo, scene, c.transform).unwrap();
        let stride = c.panel.0 as usize * 4;
        let mut built = vec![0u8; stride * c.panel.1 as usize];
        let g = plate.ground();
        for px in built.chunks_mut(4) {
            px.copy_from_slice(&[g[2], g[1], g[0], 0xff]);
        }
        plate
            .glow_sprite(c.level)
            .unwrap()
            .draw_over(&mut built, stride, c.panel);
        plate
            .logo_sprite()
            .unwrap()
            .draw_over(&mut built, stride, c.panel);
        assert!(built == plate.frame(c.level), "{name}");
    }
}

#[test]
fn rotated_mark_lands_where_the_boot_scene_places_it() {
    let out = scratch("menu-rotated.png");
    let placed = cli(&[
        "render",
        "--stage",
        "grub-menu",
        "--mode",
        "3840x2160",
        "--transform",
        "1",
        "--logo",
        mark().to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    let v: serde_json::Value = serde_json::from_str(&placed).unwrap();
    let (logo, x, y) = (346, 907, 1632);
    let (panel_x, panel_y) = (y, 2160 - x - logo);
    assert_eq!(
        (&v["x"], &v["y"], &v["width"], &v["height"]),
        (&panel_x.into(), &panel_y.into(), &logo.into(), &logo.into()),
        "the 2160x3840 row of PR 130's table, turned onto its 3840x2160 panel"
    );
}

#[test]
fn sprite_goldens() {
    let mark = mark();
    let mark = mark.to_str().unwrap();
    let cases: &[(&str, &[&str])] = &[
        (
            "grub-menu-1920x1080-t0.png",
            &["--stage", "grub-menu", "--mode", "1920x1080"],
        ),
        (
            "grub-menu-3840x2160-t1.png",
            &[
                "--stage",
                "grub-menu",
                "--mode",
                "3840x2160",
                "--transform",
                "1",
            ],
        ),
        (
            "glow-1201x799-t3-l042.png",
            &[
                "--stage",
                "glow",
                "--mode",
                "1201x799",
                "--transform",
                "3",
                "--level",
                "0.42",
            ],
        ),
        (
            "bar-1920x1080-t0-p050.png",
            &["--stage", "bar", "--mode", "1920x1080", "--progress", "0.5"],
        ),
        ("icon-40.png", &["--stage", "icon", "--edge", "40"]),
    ];
    for (name, args) in cases {
        let out = scratch(name);
        let mut argv = vec!["render"];
        argv.extend_from_slice(args);
        argv.extend_from_slice(&["--logo", mark, "--out", out.to_str().unwrap()]);
        cli(&argv);
        same_pixels(&out, &goldens().join(name));
    }
}

#[test]
fn spec_without_a_source_is_the_scene_json_export() {
    let out = scratch("scene.json");
    cli(&["spec", "--out", out.to_str().unwrap()]);
    let got: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let want: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("tests/fixtures/scene.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(got, want);
}

#[test]
fn the_helper_draws_every_golden_exactly() {
    let helper = Helper::new(env!("CARGO_BIN_EXE_vigil-scene"), Duration::from_secs(300));
    let svg = std::fs::read(mark()).unwrap();
    let raw = |name: &str| pixels(&goldens().join(name)).2;
    for (name, c) in splash_cases() {
        let spec = format!(r#"{{"version":2,"ref_short_side":{}}}"#, c.reference);
        let req = Request {
            svg: Some(&svg),
            spec: Some(spec.as_bytes()),
            mode: c.panel,
            transform: c.transform,
            layer: Layer::Frame,
            value: c.level,
        };
        let mut xrgb = vec![0u8; c.panel.0 as usize * c.panel.1 as usize * 4];
        helper.draw(&req, &mut xrgb).unwrap();
        let rgb: Vec<u8> = xrgb.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
        assert!(rgb == raw(&name), "{name}");
    }
    for (name, mode, transform, layer, value) in [
        (
            "grub-menu-1920x1080-t0.png",
            (1920, 1080),
            0,
            Layer::Logo,
            0.0,
        ),
        (
            "grub-menu-3840x2160-t1.png",
            (3840, 2160),
            1,
            Layer::Logo,
            0.0,
        ),
        (
            "glow-1201x799-t3-l042.png",
            (1201, 799),
            3,
            Layer::Glow,
            0.42,
        ),
        (
            "bar-1920x1080-t0-p050.png",
            (1920, 1080),
            0,
            Layer::Bar,
            0.5,
        ),
    ] {
        let req = Request {
            svg: Some(&svg),
            spec: None,
            mode,
            transform,
            layer,
            value,
        };
        let at = req.place().unwrap();
        let mut rgba = vec![0u8; at.width as usize * at.height as usize * 4];
        helper.draw(&req, &mut rgba).unwrap();
        assert!(rgba == raw(name), "{name}");
    }
    let icon = helper.mask(&svg, Format::Svg, 40).unwrap();
    assert!(icon.sprite(Scene::default().mark) == raw("icon-40.png"));
}
