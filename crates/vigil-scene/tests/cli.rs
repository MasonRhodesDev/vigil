#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vigil-scene-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn mark() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mark.svg")
}

fn refused(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_vigil-scene"))
        .args(args)
        .output()
        .unwrap();
    assert!(!out.status.success(), "vigil-scene {args:?} succeeded");
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn a_hostile_scene_spec_cannot_ask_for_huge_pictures() {
    let out = scratch("out.png");
    let out = out.to_str().unwrap();
    for (name, spec) in [
        ("ref", r#"{"version":2,"ref_short_side":4000000000}"#),
        ("bar", r#"{"version":2,"geometry":{"bar_h_frac":1e9}}"#),
        ("logo", r#"{"version":2,"geometry":{"logo_frac":1e9}}"#),
        (
            "glow",
            r#"{"version":2,"geometry":{"glow_mult":4000000000}}"#,
        ),
        (
            "bar area",
            r#"{"version":2,"ref_short_side":16384,"geometry":{"logo_frac":0.01,"bar_w_frac":1,"bar_h_frac":1}}"#,
        ),
    ] {
        let path = scratch(&format!("{name}.json"));
        std::fs::write(&path, spec).unwrap();
        for stage in ["bar", "splash", "grub-menu", "glow"] {
            let why = refused(&[
                "render",
                "--stage",
                stage,
                "--mode",
                "1920x1080",
                "--scene",
                path.to_str().unwrap(),
                "--logo",
                mark().to_str().unwrap(),
                "--out",
                out,
            ]);
            assert!(
                why.contains("the scene asks for") || why.contains("is outside"),
                "{name} {stage}: {why}"
            );
        }
    }
    let big = scratch("big.json");
    std::fs::write(&big, " ".repeat(2 << 20)).unwrap();
    let why = refused(&[
        "render",
        "--stage",
        "bar",
        "--mode",
        "800x600",
        "--scene",
        big.to_str().unwrap(),
        "--out",
        out,
    ]);
    assert!(why.contains("over"), "{why}");
}

#[test]
fn malformed_arguments_are_refused() {
    let out = scratch("x.png");
    let out = out.to_str().unwrap();
    for (args, says) in [
        (
            vec!["render", "--stage", "nope", "--out", out],
            "unknown stage",
        ),
        (
            vec![
                "render", "--stage", "splash", "--mode", "1920", "--out", out,
            ],
            "WxH",
        ),
        (
            vec![
                "render", "--stage", "splash", "--mode", "20000x10", "--out", out,
            ],
            "outside",
        ),
        (
            vec![
                "render", "--stage", "splash", "--mode", "0x10", "--out", out,
            ],
            "outside",
        ),
        (
            vec![
                "render",
                "--stage",
                "splash",
                "--mode",
                "8x8",
                "--transform",
                "4",
                "--out",
                out,
            ],
            "transform",
        ),
        (
            vec![
                "render", "--stage", "splash", "--mode", "8x8", "--level", "2", "--out", out,
            ],
            "outside 0..1",
        ),
        (
            vec![
                "render", "--stage", "splash", "--mode", "8x8", "--level", "NaN", "--out", out,
            ],
            "outside 0..1",
        ),
        (
            vec![
                "render",
                "--stage",
                "grub-menu",
                "--mode",
                "8x8",
                "--out",
                out,
            ],
            "needs --logo",
        ),
        (
            vec!["render", "--stage", "icon", "--edge", "99999", "--out", out],
            "--edge",
        ),
        (
            vec![
                "render",
                "--stage",
                "splash",
                "--mode",
                "8x8",
                "--ref-short-side",
                "99999",
                "--out",
                out,
            ],
            "over",
        ),
        (
            vec![
                "render", "--stage", "splash", "--stage", "bar", "--out", out,
            ],
            "twice",
        ),
        (vec!["render", "--bogus", "1"], "unknown argument"),
        (vec!["render", "--stage"], "needs a value"),
    ] {
        let why = refused(&args);
        assert!(why.contains(says), "{args:?}: {why}");
    }
}
