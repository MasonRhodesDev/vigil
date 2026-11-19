use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;

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
    }
}

#[allow(unsafe_code)]
fn render(req: &VigilSceneRequest, out: &mut [u8]) -> (i32, VigilScenePlacement) {
    let mut at = VigilScenePlacement::default();
    let (ptr, len) = if out.is_empty() {
        (ptr::null_mut(), 0)
    } else {
        (out.as_mut_ptr(), out.len())
    };
    // SAFETY: req, out and at are live, aligned and disjoint for the call.
    let rc = unsafe { vigil_scene_render(req, ptr, len, &raw mut at) };
    (rc, at)
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
        (VIGIL_SCENE_LAYER_BAR, 0.5, plate.bar_sprite(0.5).rgba),
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
    let cases: [(&str, Edit, i32); 9] = [
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
    assert_eq!(
        render(&request(hostile, VIGIL_SCENE_LAYER_LOGO), &mut []).0,
        VIGIL_SCENE_ERR_REFUSED
    );
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
    assert_eq!(render(&req, &mut []).0, VIGIL_SCENE_ERR_TOO_LARGE);

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

// The test binary sits in <target>/<profile dir>/deps, and the cdylib must
// come from that same profile, whatever the cargo version names its files.
fn cdylib() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile = match exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
    {
        Some(d) if d == "debug" => "dev".to_owned(),
        Some(d) => d.to_string_lossy().into_owned(),
        None => "dev".to_owned(),
    };
    let built = Command::new(env!("CARGO"))
        .args(["build", "--frozen", "--lib", "--message-format=json"])
        .args(["-p", "vigil-scene-ffi", "--profile", &profile])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "cargo build: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    String::from_utf8_lossy(&built.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
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
