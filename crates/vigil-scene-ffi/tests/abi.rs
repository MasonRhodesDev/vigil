#![deny(unsafe_code)]

use vigil_scene_corpus as common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use vigil_scene::{Logo, Plate, Scene};
use vigil_scene_ffi::*;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn mark() -> Vec<u8> {
    std::fs::read(root().join("tests/fixtures/mark.svg")).unwrap()
}

fn request(svg: &[u8], layer: u32) -> VigilSceneRequest {
    VigilSceneRequest {
        svg: svg.as_ptr(),
        svg_len: svg.len(),
        spec_json: ptr::null(),
        spec_len: 0,
        mode_width: 1920,
        mode_height: 1080,
        transform: 0,
        layer,
        value: 0.0,
        deadline_ms: VIGIL_SCENE_MAX_DEADLINE_MS,
    }
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    }
}

// Cargo says where it put what it built, since the layout of the target dir
// changes between cargo versions.
fn cargo_build(args: &[&str], helper: Option<&Path>) -> Vec<serde_json::Value> {
    let mut cargo = Command::new(env!("CARGO"));
    cargo
        .args([
            "build",
            "--frozen",
            "--message-format=json",
            "--profile",
            profile(),
        ])
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if let Some(helper) = helper {
        cargo.env("VIGIL_SCENE_HELPER", helper);
    }
    let out = cargo.output().unwrap();
    assert!(
        out.status.success(),
        "cargo build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn helper() -> &'static Path {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    HELPER.get_or_init(|| {
        cargo_build(&["-p", "vigil-scene", "--bin", "vigil-scene"], None)
            .iter()
            .find_map(|m| m["executable"].as_str().map(PathBuf::from))
            .expect("cargo names the helper it built")
    })
}

#[allow(unsafe_code)]
fn render_by(helper: &Path, req: &VigilSceneRequest, out: &mut [u8]) -> (i32, VigilScenePlacement) {
    let mut at = VigilScenePlacement::default();
    let (ptr, len) = if out.is_empty() {
        (ptr::null_mut(), 0)
    } else {
        (out.as_mut_ptr(), out.len())
    };
    // SAFETY: req, out and at are live, aligned and disjoint for the call.
    let rc = unsafe { render_with(helper, req, ptr, len, &raw mut at) };
    (rc, at)
}

fn render(req: &VigilSceneRequest, out: &mut [u8]) -> (i32, VigilScenePlacement) {
    render_by(helper(), req, out)
}

#[test]
fn header_is_current() {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let config = cbindgen::Config::from_file(Path::new(crate_dir).join("cbindgen.toml")).unwrap();
    let mut generated = Vec::new();
    cbindgen::generate_with_config(crate_dir, config)
        .unwrap()
        .write(&mut generated);
    let path = Path::new(crate_dir).join("include/vigil_scene.h");
    if std::env::var_os("VIGIL_SCENE_BLESS").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    let committed = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&generated);
    assert!(text.contains("int32_t vigil_scene_render("), "{text}");
    assert!(generated == committed, "include/vigil_scene.h is stale");
}

#[test]
fn every_layer_matches_the_library() {
    let svg = mark();
    let logo = Logo::svg(&svg).unwrap();
    let plate = Plate::new(&Scene::default(), &logo, (1080, 1920), 3).unwrap();
    for (layer, value, want) in [
        (VIGIL_SCENE_LAYER_FRAME, 0.42, plate.frame(0.42)),
        (
            VIGIL_SCENE_LAYER_LOGO,
            0.0,
            plate.logo_sprite().unwrap().rgba,
        ),
        (
            VIGIL_SCENE_LAYER_GLOW,
            0.3,
            plate.glow_sprite(0.3).unwrap().rgba,
        ),
        (
            VIGIL_SCENE_LAYER_BAR,
            0.5,
            plate.bar_sprite(0.5).unwrap().rgba,
        ),
    ] {
        let mut req = request(&svg, layer);
        req.transform = 3;
        req.value = value;
        let (rc, at) = render(&req, &mut []);
        assert_eq!(rc, VIGIL_SCENE_OK);
        let mut out = vec![0u8; at.stride * at.height as usize];
        assert_eq!(render(&req, &mut out).0, VIGIL_SCENE_OK);
        assert!(out == want, "layer {layer}");
    }
}

#[test]
#[allow(unsafe_code)]
fn bad_arguments_are_refused_before_any_drawing() {
    let svg = mark();
    let mut at = VigilScenePlacement::default();
    // SAFETY: a null request is the case under test; nothing is dereferenced.
    let rc = unsafe { vigil_scene_render(ptr::null(), ptr::null_mut(), 0, &raw mut at) };
    assert_eq!(rc, VIGIL_SCENE_ERR_ARGUMENT);
    let req = request(&svg, VIGIL_SCENE_LAYER_LOGO);
    // SAFETY: a null placement is the case under test.
    let rc = unsafe { vigil_scene_render(&raw const req, ptr::null_mut(), 0, ptr::null_mut()) };
    assert_eq!(rc, VIGIL_SCENE_ERR_ARGUMENT);
    let mut one = [0u8; 1];
    // SAFETY: a non-null out with a length of 0 is the case under test.
    let rc = unsafe { vigil_scene_render(&raw const req, one.as_mut_ptr(), 0, &raw mut at) };
    assert_eq!(rc, VIGIL_SCENE_ERR_ARGUMENT);

    type Edit = fn(&mut VigilSceneRequest);
    let cases: [(&str, Edit, i32); 10] = [
        ("unknown layer", |r| r.layer = 9, VIGIL_SCENE_ERR_ARGUMENT),
        ("transform 4", |r| r.transform = 4, VIGIL_SCENE_ERR_ARGUMENT),
        (
            "NaN value",
            |r| r.value = f64::NAN,
            VIGIL_SCENE_ERR_ARGUMENT,
        ),
        ("value over 1", |r| r.value = 1.5, VIGIL_SCENE_ERR_ARGUMENT),
        (
            "null svg with a length",
            |r| r.svg = ptr::null(),
            VIGIL_SCENE_ERR_ARGUMENT,
        ),
        (
            "svg without a length",
            |r| r.svg_len = 0,
            VIGIL_SCENE_ERR_ARGUMENT,
        ),
        (
            "deadline over the maximum",
            |r| r.deadline_ms = VIGIL_SCENE_MAX_DEADLINE_MS + 1,
            VIGIL_SCENE_ERR_ARGUMENT,
        ),
        ("mode 0", |r| r.mode_width = 0, VIGIL_SCENE_ERR_TOO_LARGE),
        (
            "mode too big",
            |r| r.mode_height = 20000,
            VIGIL_SCENE_ERR_TOO_LARGE,
        ),
        (
            "logo without svg",
            |r| {
                r.svg = ptr::null();
                r.svg_len = 0;
            },
            VIGIL_SCENE_ERR_ARGUMENT,
        ),
    ];
    for (what, edit, want) in cases {
        let mut req = request(&svg, VIGIL_SCENE_LAYER_LOGO);
        edit(&mut req);
        assert_eq!(render(&req, &mut []).0, want, "{what}");
    }

    let big = vec![b' '; 600 * 1024];
    assert_eq!(
        render(&request(&big, VIGIL_SCENE_LAYER_LOGO), &mut []).0,
        VIGIL_SCENE_ERR_TOO_LARGE
    );
    let hostile = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="/etc/passwd"/></svg>"#;
    let req = request(hostile, VIGIL_SCENE_LAYER_LOGO);
    let (rc, at) = render(&req, &mut []);
    assert_eq!(rc, VIGIL_SCENE_OK, "a size query does not read the logo");
    let mut out = vec![0u8; at.stride * at.height as usize];
    assert_eq!(render(&req, &mut out).0, VIGIL_SCENE_ERR_REFUSED);
    assert!(out.iter().all(|&b| b == 0), "nothing written on a refusal");
    let mut req = request(&svg, VIGIL_SCENE_LAYER_LOGO);
    let spec = b"{\"version\": 3}";
    req.spec_json = spec.as_ptr();
    req.spec_len = spec.len();
    assert_eq!(render(&req, &mut []).0, VIGIL_SCENE_ERR_INVALID);

    let mut req = request(&[], VIGIL_SCENE_LAYER_BAR);
    req.svg = ptr::null();
    let spec = br#"{"version":2,"ref_short_side":4000000000}"#;
    req.spec_json = spec.as_ptr();
    req.spec_len = spec.len();
    assert_eq!(render(&req, &mut []).0, VIGIL_SCENE_ERR_REFUSED);

    let req = request(&svg, VIGIL_SCENE_LAYER_LOGO);
    let mut short = vec![0u8; 10];
    let (rc, at) = render(&req, &mut short);
    assert_eq!(rc, VIGIL_SCENE_ERR_BUFFER);
    assert_eq!((at.x, at.y, at.width, at.height), (874, 421, 173, 173));
    assert!(
        short.iter().all(|&b| b == 0),
        "nothing written to a short buffer"
    );
}

// The C program links a cdylib from a profile like the test's own, built to
// run the helper this test built.
fn cdylib() -> PathBuf {
    cargo_build(&["--lib", "-p", "vigil-scene-ffi"], Some(helper()))
        .iter()
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "vigil_scene_ffi")
        .flat_map(|m| m["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|f| f.as_str().map(PathBuf::from))
        .find(|f| f.extension().is_some_and(|e| e == "so"))
        .expect("cargo names the cdylib it built")
}

#[test]
fn c_program_gets_the_same_pixels_through_the_header() {
    let lib = cdylib();
    let dir = std::env::temp_dir().join(format!("vigil-scene-abi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("smoke");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let built = Command::new(&cc)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&bin)
        .arg(crate_dir.join("tests/c/smoke.c"))
        .arg("-I")
        .arg(crate_dir.join("include"))
        .arg(&lib)
        .arg(format!("-Wl,-rpath,{}", lib.parent().unwrap().display()))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{cc}: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let pixels = dir.join("logo.rgba");
    let ran = Command::new(&bin)
        .arg(root().join("tests/fixtures/mark.svg"))
        .arg(&pixels)
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout).trim(),
        "1632 907 346 346 1384"
    );

    let logo = Logo::svg(&mark()).unwrap();
    let plate = Plate::new(&Scene::default(), &logo, (2160, 3840), 1).unwrap();
    assert!(std::fs::read(&pixels).unwrap() == plate.logo_sprite().unwrap().rgba);
}

const LAYERS: [u32; 4] = [
    VIGIL_SCENE_LAYER_FRAME,
    VIGIL_SCENE_LAYER_LOGO,
    VIGIL_SCENE_LAYER_GLOW,
    VIGIL_SCENE_LAYER_BAR,
];

fn with_spec(svg: &[u8], spec: &[u8], layer: u32) -> i32 {
    let mut req = request(svg, layer);
    req.spec_json = spec.as_ptr();
    req.spec_len = spec.len();
    render(&req, &mut []).0
}

#[test]
fn the_reviews_scene_specs_are_refused_not_drawn() {
    let svg = mark();
    for centre in ["1e18", "1e300", "-1e300"] {
        let spec = format!(r#"{{"version":2,"geometry":{{"centre_frac_of_h":{centre}}}}}"#);
        for layer in LAYERS {
            assert_eq!(
                with_spec(&svg, spec.as_bytes(), layer),
                VIGIL_SCENE_ERR_REFUSED,
                "centre {centre}, layer {layer}"
            );
        }
    }
    let bar = br#"{"version":2,"ref_short_side":16384,"geometry":{"logo_frac":0.01,"bar_w_frac":1,"bar_h_frac":1}}"#;
    assert_eq!(
        with_spec(&svg, bar, VIGIL_SCENE_LAYER_BAR),
        VIGIL_SCENE_ERR_TOO_LARGE
    );
    for spec in [
        r#"{"version":2,"geometry":{"logo_frac":0}}"#,
        r#"{"version":2,"geometry":{"gap_frac":-1}}"#,
        r#"{"version":2,"geometry":{"glow_mult":0}}"#,
        r#"{"version":2,"motion":{"in_s":1e300}}"#,
        r#"{"version":2,"ref_short_side":16385}"#,
    ] {
        assert_eq!(
            with_spec(&svg, spec.as_bytes(), VIGIL_SCENE_LAYER_FRAME),
            VIGIL_SCENE_ERR_REFUSED,
            "{spec}"
        );
    }
}

#[test]
fn a_size_query_places_what_a_render_draws() {
    let svg = mark();
    for transform in 0..4 {
        for layer in LAYERS {
            let mut req = request(&svg, layer);
            req.transform = transform;
            req.value = 0.5;
            let (rc, query) = render(&req, &mut []);
            assert_eq!(rc, VIGIL_SCENE_OK);
            let mut out = vec![0u8; query.stride * query.height as usize];
            let (rc, drawn) = render(&req, &mut out);
            assert_eq!(rc, VIGIL_SCENE_OK);
            assert_eq!(query, drawn, "layer {layer}, transform {transform}");
        }
    }
}

#[test]
fn a_size_query_does_not_draw() {
    let svg = mark();
    let mut small = request(&svg, VIGIL_SCENE_LAYER_FRAME);
    small.mode_width = 640;
    small.mode_height = 480;
    let (rc, at) = render(&small, &mut []);
    assert_eq!(rc, VIGIL_SCENE_OK);
    let mut out = vec![0u8; at.stride * at.height as usize];
    let start = Instant::now();
    assert_eq!(render(&small, &mut out).0, VIGIL_SCENE_OK);
    let drawing_small = start.elapsed();

    let mut huge = request(&svg, VIGIL_SCENE_LAYER_FRAME);
    huge.mode_width = 8192;
    huge.mode_height = 8192;
    let start = Instant::now();
    let (rc, at) = render(&huge, &mut []);
    let querying_huge = start.elapsed();
    assert_eq!(rc, VIGIL_SCENE_OK);
    assert_eq!((at.width, at.height, at.stride), (8192, 8192, 8192 * 4));
    assert!(
        querying_huge < drawing_small,
        "a size query for 8192x8192 took {querying_huge:?}, drawing 640x480 took {drawing_small:?}"
    );
}

#[test]
fn a_missing_helper_is_its_own_code_and_the_ground_still_draws() {
    let svg = mark();
    let gone = Path::new("/nonexistent/vigil-scene");
    let req = request(&svg, VIGIL_SCENE_LAYER_LOGO);
    let (rc, at) = render_by(gone, &req, &mut []);
    assert_eq!(rc, VIGIL_SCENE_OK);
    let mut out = vec![0u8; at.stride * at.height as usize];
    assert_eq!(render_by(gone, &req, &mut out).0, VIGIL_SCENE_ERR_HELPER);

    let mut ground = request(&[], VIGIL_SCENE_LAYER_FRAME);
    ground.svg = ptr::null();
    let (rc, at) = render_by(gone, &ground, &mut []);
    assert_eq!(rc, VIGIL_SCENE_OK);
    let mut out = vec![0u8; at.stride * at.height as usize];
    assert_eq!(render_by(gone, &ground, &mut out).0, VIGIL_SCENE_OK);
    let want = Plate::ground_only(&Scene::default(), (1920, 1080), 0).frame(0.0);
    assert!(out == want);
}

const CHILD: &str = "VIGIL_SCENE_ABI_TEST";

#[test]
fn a_helper_stopped_at_its_deadline_is_a_timeout_not_a_refusal() {
    let dir = std::env::temp_dir().join(format!("vigil-scene-abi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let spin = dir.join("spins");
    let written = Command::new("sh")
        .args([
            "-c",
            r#"printf '#!/bin/sh\nwhile :; do :; done\n' > "$1" && chmod 755 "$1""#,
        ])
        .args(["sh".as_ref(), spin.as_os_str()])
        .status()
        .unwrap();
    assert!(written.success());
    let mut req = request(&mark(), VIGIL_SCENE_LAYER_LOGO);
    req.deadline_ms = 200;
    let (_, at) = render_by(&spin, &req, &mut []);
    let mut out = vec![0u8; at.stride * at.height as usize];
    let start = Instant::now();
    assert_eq!(render_by(&spin, &req, &mut out).0, VIGIL_SCENE_ERR_TIMEOUT);
    assert!(start.elapsed() < Duration::from_millis(700));
    assert!(out.iter().all(|&b| b == 0));
    std::fs::remove_dir_all(&dir).unwrap();
}

fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[test]
fn child_calls_the_abi_with_one_review_input() {
    let Some(index) = std::env::var_os(CHILD) else {
        return;
    };
    let index: usize = index.to_str().unwrap().parse().unwrap();
    let (what, text) = &common::review_inputs()[index];
    let helper = PathBuf::from(std::env::var_os("VIGIL_SCENE_HELPER").unwrap());
    let base = peak_rss_kib();
    let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
    let mut req = request(text.as_bytes(), VIGIL_SCENE_LAYER_FRAME);
    req.deadline_ms = 0;
    let start = Instant::now();
    let (rc, at) = render_by(&helper, &req, &mut []);
    assert_eq!(rc, VIGIL_SCENE_OK);
    let mut out = vec![0u8; at.stride * at.height as usize];
    let (rc, _) = render_by(&helper, &req, &mut out);
    let took = start.elapsed();
    let grew = peak_rss_kib().saturating_sub(base);
    println!("ABI {what}: code {rc}, {took:?}, parent grew {grew} KiB");
    assert_ne!(
        rc, VIGIL_SCENE_ERR_HELPER,
        "{what}: the helper failed, not the logo"
    );
    let deadline = Duration::from_millis(VIGIL_SCENE_DEFAULT_DEADLINE_MS.into());
    assert!(
        took < deadline + Duration::from_millis(500),
        "{what}: {took:?}"
    );
    let frame = (1920 * 1080 * 4) / 1024;
    assert!(
        grew < frame + 16 * 1024,
        "{what}: the parent grew by {grew} KiB"
    );
    assert_eq!(std::fs::read_dir("/proc/self/fd").unwrap().count(), fds);
}

#[test]
fn every_review_input_returns_through_the_abi_by_its_deadline() {
    for i in 0..common::review_inputs().len() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_calls_the_abi_with_one_review_input"])
            .args(["--nocapture", "--test-threads", "1"])
            .env(CHILD, i.to_string())
            .env("VIGIL_SCENE_HELPER", helper())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for line in stdout
            .lines()
            .filter_map(|l| l.find("ABI ").map(|i| &l[i..]))
        {
            println!("{line}");
        }
    }
}
