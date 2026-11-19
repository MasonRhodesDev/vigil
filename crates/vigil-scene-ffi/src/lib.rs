#![deny(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Duration;

use vigil_scene::{
    DEFAULT_DEADLINE, Error, Helper, Layer, MAX_DEADLINE, MAX_LOGO_BYTES, MAX_SPEC_BYTES, Request,
};

pub const VIGIL_SCENE_OK: i32 = 0;
pub const VIGIL_SCENE_ERR_ARGUMENT: i32 = -1;
pub const VIGIL_SCENE_ERR_TOO_LARGE: i32 = -2;
pub const VIGIL_SCENE_ERR_REFUSED: i32 = -3;
pub const VIGIL_SCENE_ERR_INVALID: i32 = -4;
pub const VIGIL_SCENE_ERR_BUFFER: i32 = -5;
pub const VIGIL_SCENE_ERR_PANIC: i32 = -6;
/// The helper could not be started: it is missing from its installed path,
/// or it is not the version this library was built with.
pub const VIGIL_SCENE_ERR_HELPER: i32 = -7;

/// The whole frame, XRGB8888: bytes B, G, R, 0xff per pixel.
pub const VIGIL_SCENE_LAYER_FRAME: u32 = 0;
/// The mark, RGBA with straight alpha: bytes R, G, B, A per pixel.
pub const VIGIL_SCENE_LAYER_LOGO: u32 = 1;
/// The glow at `value`, RGBA with straight alpha.
pub const VIGIL_SCENE_LAYER_GLOW: u32 = 2;
/// The progress bar at `value`, RGBA, opaque.
pub const VIGIL_SCENE_LAYER_BAR: u32 = 3;

/// The deadline a request with `deadline_ms` 0 gets.
pub const VIGIL_SCENE_DEFAULT_DEADLINE_MS: u32 = 1500;
/// The longest deadline a request may ask for.
pub const VIGIL_SCENE_MAX_DEADLINE_MS: u32 = 5000;

const _: () = assert!(VIGIL_SCENE_DEFAULT_DEADLINE_MS as u128 == DEFAULT_DEADLINE.as_millis());
const _: () = assert!(VIGIL_SCENE_MAX_DEADLINE_MS as u128 == MAX_DEADLINE.as_millis());

/// What to draw. `svg` and `spec_json` are each null with a length of 0, or
/// point to that many bytes. Without `spec_json` the built-in scene is used.
/// The mode is the panel's, and `transform` (0 to 3) turns the scene on it as
/// `wl_output.transform` does. `deadline_ms` bounds the drawing of a logo:
/// 0 asks for `VIGIL_SCENE_DEFAULT_DEADLINE_MS`, and more than
/// `VIGIL_SCENE_MAX_DEADLINE_MS` is refused.
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
    pub deadline_ms: u32,
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

fn code(e: &Error) -> i32 {
    match e {
        Error::Io(_) | Error::Invalid(_) => VIGIL_SCENE_ERR_INVALID,
        Error::Refused(_) => VIGIL_SCENE_ERR_REFUSED,
        Error::TooLarge(_) => VIGIL_SCENE_ERR_TOO_LARGE,
        Error::Helper(_) => VIGIL_SCENE_ERR_HELPER,
    }
}

struct Fields {
    transform: u8,
    layer: Layer,
    deadline: Duration,
}

fn parse(raw: &VigilSceneRequest) -> Result<Fields, i32> {
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
    let deadline = match raw.deadline_ms {
        0 => DEFAULT_DEADLINE,
        ms if ms <= VIGIL_SCENE_MAX_DEADLINE_MS => Duration::from_millis(ms.into()),
        _ => return Err(VIGIL_SCENE_ERR_ARGUMENT),
    };
    Ok(Fields {
        transform,
        layer,
        deadline,
    })
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
/// null and `out_len` 0 it only fills `placement`, from the spec and the mode,
/// without reading the logo, so a caller can size the buffer; the bytes
/// needed are `placement.stride * placement.height`. When `out_len` is too
/// small it fills `placement` and returns `VIGIL_SCENE_ERR_BUFFER`.
///
/// A request with a logo is drawn by the helper process installed with this
/// library, and the call returns within `deadline_ms`, plus the kernel's
/// teardown of a killed helper and the copy of the layer into `out`, whatever
/// the logo holds. A request without one is drawn
/// in the calling process. On any error `out` is left unwritten, and a caller
/// should draw the ground-only scene by asking again without the logo. The
/// same logo is refused again on the next call, so a caller that remembers
/// the refusal stalls for one deadline at most.
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
    // SAFETY: the caller's guarantees are the ones render_with needs.
    unsafe {
        render_with(
            Path::new(vigil_scene::installed_helper!()),
            request,
            out,
            out_len,
            placement,
        )
    }
}

/// `vigil_scene_render` with the helper at `helper`, for tests.
///
/// # Safety
///
/// As for `vigil_scene_render`.
#[doc(hidden)]
#[allow(unsafe_code)]
pub unsafe fn render_with(
    helper: &Path,
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
    let fields = match parse(raw) {
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
    if svg.is_none() && matches!(fields.layer, Layer::Logo | Layer::Glow) {
        return VIGIL_SCENE_ERR_ARGUMENT;
    }
    let req = Request {
        svg,
        spec,
        mode: (raw.mode_width, raw.mode_height),
        transform: fields.transform,
        layer: fields.layer,
        value: raw.value,
    };
    let at = match catch_unwind(AssertUnwindSafe(|| req.place())) {
        Ok(Ok(at)) => VigilScenePlacement {
            x: at.x,
            y: at.y,
            width: at.width,
            height: at.height,
            stride: at.width as usize * 4,
        },
        Ok(Err(e)) => return code(&e),
        Err(_) => return VIGIL_SCENE_ERR_PANIC,
    };
    // SAFETY: placement is non-null and aligned, and the caller guarantees it
    // is valid for writes and overlaps no other argument.
    unsafe { placement.write(at) };
    if out.is_null() {
        return VIGIL_SCENE_OK;
    }
    let need = at.stride * at.height as usize;
    if out_len < need {
        return VIGIL_SCENE_ERR_BUFFER;
    }
    // SAFETY: out is non-null, the caller guarantees out_len writable bytes
    // that overlap nothing else, and need is at most out_len.
    let dst = unsafe { std::slice::from_raw_parts_mut(out, need) };
    let helper = Helper::new(helper, fields.deadline);
    match catch_unwind(AssertUnwindSafe(|| helper.draw(&req, dst))) {
        Ok(Ok(())) => VIGIL_SCENE_OK,
        Ok(Err(e)) => code(&e),
        Err(_) => VIGIL_SCENE_ERR_PANIC,
    }
}
