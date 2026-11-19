#![deny(unsafe_code)]
#![deny(
    unsafe_op_in_unsafe_fn,
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block,
    clippy::missing_safety_doc
)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use vigil_scene::{Error, Layout, Logo, MAX_LOGO_BYTES, MAX_SPEC_BYTES, Placement, Plate, Scene};

pub const VIGIL_SCENE_OK: i32 = 0;
pub const VIGIL_SCENE_ERR_ARGUMENT: i32 = -1;
pub const VIGIL_SCENE_ERR_TOO_LARGE: i32 = -2;
pub const VIGIL_SCENE_ERR_REFUSED: i32 = -3;
pub const VIGIL_SCENE_ERR_INVALID: i32 = -4;
pub const VIGIL_SCENE_ERR_BUFFER: i32 = -5;
pub const VIGIL_SCENE_ERR_PANIC: i32 = -6;

/// The whole frame, XRGB8888: bytes B, G, R, 0xff per pixel.
pub const VIGIL_SCENE_LAYER_FRAME: u32 = 0;
/// The mark, RGBA with straight alpha: bytes R, G, B, A per pixel.
pub const VIGIL_SCENE_LAYER_LOGO: u32 = 1;
/// The glow at `value`, RGBA with straight alpha.
pub const VIGIL_SCENE_LAYER_GLOW: u32 = 2;
/// The progress bar at `value`, RGBA, opaque.
pub const VIGIL_SCENE_LAYER_BAR: u32 = 3;

/// What to draw. `svg` and `spec_json` are each null with a length of 0, or
/// point to that many bytes. Without `spec_json` the built-in scene is used.
/// The mode is the panel's, and `transform` (0 to 3) turns the scene on it as
/// `wl_output.transform` does.
#[repr(C)]
pub struct VigilSceneRequest {
    pub svg: *const u8,
    pub svg_len: usize,
    pub spec_json: *const u8,
    pub spec_len: usize,
    pub mode_width: u32,
    pub mode_height: u32,
    pub transform: u32,
    pub layer: u32,
    pub value: f64,
}

/// Where the picture goes on the panel, in panel pixels, and its row stride
/// in bytes. A sprite may reach past the panel's edges.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VigilScenePlacement {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

#[derive(Clone, Copy)]
enum Layer {
    Frame,
    Logo,
    Glow,
    Bar,
}

struct Request<'a> {
    svg: Option<&'a [u8]>,
    spec: Option<&'a [u8]>,
    mode: (u32, u32),
    transform: u8,
    layer: Layer,
}

fn code(e: &Error) -> i32 {
    match e {
        Error::Io(_) | Error::Invalid(_) => VIGIL_SCENE_ERR_INVALID,
        Error::Refused(_) => VIGIL_SCENE_ERR_REFUSED,
        Error::TooLarge(_) => VIGIL_SCENE_ERR_TOO_LARGE,
    }
}

fn parse(raw: &VigilSceneRequest) -> Result<(u8, Layer, f64), i32> {
    let layer = match raw.layer {
        VIGIL_SCENE_LAYER_FRAME => Layer::Frame,
        VIGIL_SCENE_LAYER_LOGO => Layer::Logo,
        VIGIL_SCENE_LAYER_GLOW => Layer::Glow,
        VIGIL_SCENE_LAYER_BAR => Layer::Bar,
        _ => return Err(VIGIL_SCENE_ERR_ARGUMENT),
    };
    let transform = u8::try_from(raw.transform)
        .ok()
        .filter(|t| *t < 4)
        .ok_or(VIGIL_SCENE_ERR_ARGUMENT)?;
    if !(0.0..=1.0).contains(&raw.value) {
        return Err(VIGIL_SCENE_ERR_ARGUMENT);
    }
    Ok((transform, layer, raw.value))
}

struct Prepared {
    spec: Scene,
    layout: Layout,
    logo: Option<Logo>,
}

fn prepare(req: &Request<'_>) -> Result<Prepared, i32> {
    let spec = match req.spec {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| VIGIL_SCENE_ERR_INVALID)
            .and_then(|text| Scene::parse(text).map_err(|e| code(&e)))?,
        None => Scene::default(),
    };
    let layout = Layout::new(&spec, req.mode, req.transform).map_err(|e| code(&e))?;
    let logo = req.svg.map(Logo::svg).transpose().map_err(|e| code(&e))?;
    if logo.is_none() && matches!(req.layer, Layer::Logo | Layer::Glow) {
        return Err(VIGIL_SCENE_ERR_ARGUMENT);
    }
    Ok(Prepared { spec, layout, logo })
}

fn place(p: &Prepared, layer: Layer) -> Result<VigilScenePlacement, i32> {
    let (width, height) = p.layout.panel();
    let at = match layer {
        Layer::Frame => Some(Placement {
            x: 0,
            y: 0,
            width,
            height,
        }),
        Layer::Logo => p.layout.logo(),
        Layer::Glow => p.layout.glow(),
        Layer::Bar => p.layout.bar(),
    }
    .ok_or(VIGIL_SCENE_ERR_REFUSED)?;
    Ok(VigilScenePlacement {
        x: at.x,
        y: at.y,
        width: at.width,
        height: at.height,
        stride: at.width as usize * 4,
    })
}

fn draw(p: &Prepared, layer: Layer, value: f64) -> Result<Vec<u8>, i32> {
    let (scene, transform) = (p.layout.scene(), p.layout.transform());
    let plate = match &p.logo {
        Some(logo) => Plate::new(&p.spec, logo, scene, transform).map_err(|e| code(&e))?,
        None => Plate::ground_only(&p.spec, scene, transform),
    };
    let sprite = match layer {
        Layer::Frame => return Ok(plate.frame(value)),
        Layer::Logo => plate.logo_sprite(),
        Layer::Glow => plate.glow_sprite(value),
        Layer::Bar => plate.bar_sprite(value),
    };
    sprite.map(|s| s.rgba).ok_or(VIGIL_SCENE_ERR_REFUSED)
}

/// # Safety
///
/// `ptr` is null with `len` 0, or points to `len` bytes that stay readable and
/// unchanged until the returned slice is dropped.
#[allow(unsafe_code)]
unsafe fn bytes<'a>(ptr: *const u8, len: usize, cap: usize) -> Result<Option<&'a [u8]>, i32> {
    match (ptr.is_null(), len) {
        (true, 0) => Ok(None),
        (true, _) | (false, 0) => Err(VIGIL_SCENE_ERR_ARGUMENT),
        (false, len) if len > cap => Err(VIGIL_SCENE_ERR_TOO_LARGE),
        // SAFETY: ptr is non-null and the caller guarantees len readable bytes
        // behind it; len is at most cap, far below isize::MAX.
        (false, len) => Ok(Some(unsafe { std::slice::from_raw_parts(ptr, len) })),
    }
}

/// Draws one layer of the scene into `out` and says where it goes.
///
/// Returns `VIGIL_SCENE_OK`, or a negative `VIGIL_SCENE_ERR_*`. With `out`
/// null and `out_len` 0 it only fills `placement`, without drawing, so a
/// caller can size the buffer; the bytes needed are `placement.stride * placement.height`. When
/// `out_len` is too small it fills `placement` and returns
/// `VIGIL_SCENE_ERR_BUFFER`.
///
/// # Safety
///
/// `request` and `placement` are non-null and point to a valid, aligned
/// `VigilSceneRequest` and `VigilScenePlacement`. The request's `svg` and
/// `spec_json` are null with a length of 0, or point to that many readable
/// bytes. `out` is null with `out_len` 0, or points to `out_len` writable
/// bytes. No buffer overlaps another, and none changes during the call.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vigil_scene_render(
    request: *const VigilSceneRequest,
    out: *mut u8,
    out_len: usize,
    placement: *mut VigilScenePlacement,
) -> i32 {
    if request.is_null() || !request.is_aligned() || placement.is_null() || !placement.is_aligned()
    {
        return VIGIL_SCENE_ERR_ARGUMENT;
    }
    if out.is_null() != (out_len == 0) {
        return VIGIL_SCENE_ERR_ARGUMENT;
    }
    // SAFETY: request is non-null and aligned, and the caller guarantees it
    // points to a valid VigilSceneRequest for the whole call.
    let raw = unsafe { &*request };
    let (transform, layer, value) = match parse(raw) {
        Ok(fields) => fields,
        Err(code) => return code,
    };
    // SAFETY: the caller guarantees svg points to svg_len readable bytes or is
    // null with a length of 0; bytes checks that pairing and the length.
    let svg = match unsafe { bytes(raw.svg, raw.svg_len, MAX_LOGO_BYTES as usize) } {
        Ok(svg) => svg,
        Err(code) => return code,
    };
    // SAFETY: as for svg, for spec_json and spec_len.
    let spec = match unsafe { bytes(raw.spec_json, raw.spec_len, MAX_SPEC_BYTES as usize) } {
        Ok(spec) => spec,
        Err(code) => return code,
    };
    let req = Request {
        svg,
        spec,
        mode: (raw.mode_width, raw.mode_height),
        transform,
        layer,
    };
    let (prepared, at) = match catch_unwind(AssertUnwindSafe(|| {
        prepare(&req).and_then(|p| place(&p, layer).map(|at| (p, at)))
    })) {
        Ok(Ok(placed)) => placed,
        Ok(Err(code)) => return code,
        Err(_) => return VIGIL_SCENE_ERR_PANIC,
    };
    // SAFETY: placement is non-null and aligned, and the caller guarantees it
    // is valid for writes and overlaps no other argument.
    unsafe { placement.write(at) };
    if out.is_null() {
        return VIGIL_SCENE_OK;
    }
    if out_len < at.stride * at.height as usize {
        return VIGIL_SCENE_ERR_BUFFER;
    }
    let pixels = match catch_unwind(AssertUnwindSafe(|| draw(&prepared, layer, value))) {
        Ok(Ok(pixels)) => pixels,
        Ok(Err(code)) => return code,
        Err(_) => return VIGIL_SCENE_ERR_PANIC,
    };
    if pixels.len() > out_len {
        return VIGIL_SCENE_ERR_BUFFER;
    }
    // SAFETY: out is non-null, the caller guarantees out_len writable bytes
    // that overlap nothing else, and pixels.len() is at most out_len.
    let dst = unsafe { std::slice::from_raw_parts_mut(out, pixels.len()) };
    dst.copy_from_slice(&pixels);
    VIGIL_SCENE_OK
}
