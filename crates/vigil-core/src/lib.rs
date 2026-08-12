//! Cross-crate interface surface for vigil (DESIGN.md §4).
//!
//! Every type or trait that crosses a crate boundary lives here, and nothing
//! else does. This crate depends on no other vigil crate and nothing
//! heavyweight. Subsystem crates depend on `vigil-core` plus their one
//! vendored subsystem and never on each other; only the binary assembles them.

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// Identifier for a connected output, stable for the life of the connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u32);

/// Static facts about an output at the time it was added.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Connector name as reported by DRM, e.g. `DP-1`.
    pub connector: String,
    /// Active mode, physical pixels.
    pub width: u32,
    pub height: u32,
    /// Vertical refresh in millihertz.
    pub refresh_mhz: u32,
    /// EDID manufacturer PNP id (e.g. `DEL`), when the monitor reports one.
    pub make: Option<String>,
    /// EDID model name (0xFC descriptor), when present.
    pub model: Option<String>,
    /// HiDPI scale for this output: derived from EDID physical size, or 1.0.
    pub scale: f32,
}

/// Output lifecycle, emitted by vigil-outputs.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputEvent {
    Added(OutputId, OutputInfo),
    Removed(OutputId),
    NeedsRedraw(OutputId),
}

// ---------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------

/// One frame's render target: XRGB8888 bytes, row-major, `stride` bytes per
/// row (stride may exceed `width * 4`).
pub struct FrameTarget<'a> {
    pub buffer: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

/// The pointer bitmap, shared by every path that draws one: `X` outline,
/// `#` fill, `.` transparent; scaled by the output's HiDPI factor when
/// rasterized. The hotspot is the arrow tip at (0, 0).
pub const CURSOR: &[&[u8]] = &[
    b"X...........",
    b"XX..........",
    b"X#X.........",
    b"X##X........",
    b"X###X.......",
    b"X####X......",
    b"X#####X.....",
    b"X######X....",
    b"X#######X...",
    b"X########X..",
    b"X#########X.",
    b"X#####XXXXXX",
    b"X##X##X.....",
    b"X#X.X##X....",
    b"XX..X##X....",
    b"X....X##X...",
    b".....X##X...",
    b"......X##X..",
    b"......XX....",
];

/// Rasterize [`CURSOR`] at `scale` into tightly packed ARGB8888
/// little-endian bytes (B, G, R, A). Nearest neighbor — it is a pointer.
/// Returns (bytes, width, height).
pub fn cursor_argb(scale: f32) -> (Vec<u8>, u32, u32) {
    let scale = f64::from(scale.max(1.0));
    let w = (CURSOR[0].len() as f64 * scale) as u32;
    let h = (CURSOR.len() as f64 * scale) as u32;
    let mut bytes = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let row = CURSOR[((y as f64 / scale) as usize).min(CURSOR.len() - 1)];
        for x in 0..w {
            let cell = row[((x as f64 / scale) as usize).min(row.len() - 1)];
            let i = ((y * w + x) * 4) as usize;
            match cell {
                b'X' => bytes[i..i + 4].copy_from_slice(&[0, 0, 0, 0xff]),
                b'#' => bytes[i..i + 4].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]),
                _ => {}
            }
        }
    }
    (bytes, w, h)
}

#[derive(Debug)]
pub enum PresentError {
    /// The output vanished (hotplug/VT switch); the caller should drop this
    /// presenter and wait for the next `OutputEvent`.
    DeviceLost,
    /// Anything else, already formatted for the log.
    Backend(String),
}

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresentError::DeviceLost => write!(f, "output device lost"),
            PresentError::Backend(msg) => write!(f, "present backend: {msg}"),
        }
    }
}

impl std::error::Error for PresentError {}

/// How a rendered frame reaches an output. Implemented by vigil-present-dumb
/// (software, permanent baseline) and vigil-present-gl (GL, M3).
/// What a render backend needs to know about the scene it is drawing.
///
/// Deliberately toolkit-free so the trait below can live here, next to
/// `Presenter`, without dragging the UI toolkit into this crate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneView {
    /// Logical scene size -- already rotated if the output is transformed.
    pub scene_size: (u32, u32),
    pub scale: f32,
    pub pointer: (f64, f64),
    pub cursor_visible: bool,
    /// Bumped on every scene mutation. A backend with no repaint bookkeeping
    /// of its own compares this to decide whether a frame is worth drawing.
    pub revision: u64,
}

/// Turns a scene into pixels. The half of rendering that differs between the
/// software baseline and GL; everything about *what* the scene contains is
/// shared and lives with the window.
pub trait RenderBackend {
    /// Draw the scene into this frame's canvas. Returns whether anything was
    /// drawn -- a frame that draws nothing is not presented.
    fn render(&mut self, view: &SceneView, canvas: Canvas<'_>) -> bool;

    /// Force the next frame to present even if the scene is unchanged.
    fn request_present(&mut self);
}

/// What a presenter hands the renderer for one frame.
///
/// The two rendering paths differ in what "draw here" even means: the
/// software renderer fills a byte buffer, while GL issues commands against a
/// bound framebuffer and never sees pixels at all. This is that difference,
/// and the only place the rest of the code has to know about it.
pub enum Canvas<'a> {
    /// A CPU buffer to fill (the software baseline).
    Cpu(FrameTarget<'a>),
    /// The GL context is current and its default framebuffer is bound. The
    /// renderer issues GL calls; the presenter swaps and scans out.
    Gl { width: u32, height: u32 },
}

impl Canvas<'_> {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Canvas::Cpu(target) => (target.width, target.height),
            Canvas::Gl { width, height } => (*width, *height),
        }
    }
}

pub trait Presenter {
    fn size(&self) -> (u32, u32);

    /// Hand the caller a target to draw this frame into; the closure returns
    /// whether it actually drew. A frame that drew is submitted (page flip)
    /// and `Ok(true)` is returned; a frame that didn't is skipped without
    /// flipping (`Ok(false)`), so an idle scene never presents stale buffers.
    /// Flip completion is reported via the event loop, not by blocking here.
    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError>;

    /// Drop any assumption that the CRTC still holds our configuration, so
    /// the next frame does a full modeset instead of a page flip.
    ///
    /// Required after the kernel may have lost display state under us —
    /// system resume, or reclaiming the device after a VT switch. Flipping
    /// onto a CRTC that no longer has our mode either fails or scans out
    /// nothing, and the greeter has no way to notice it is showing a black
    /// screen.
    fn invalidate(&mut self);

    /// Move, show (`Some((x, y))` in panel pixels, hotspot at the point) or
    /// hide (`None`) a hardware cursor. Returns true when a cursor plane
    /// took the update — the scene must then not composite a cursor of its
    /// own. The default has no plane and says so.
    ///
    /// Position updates are latched and reach the screen on the next frame
    /// the presenter submits — including a cursor-only flip of the current
    /// framebuffer when the scene itself is clean.
    fn set_cursor(&mut self, pos: Option<(i32, i32)>) -> bool {
        let _ = pos;
        false
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

use std::os::fd::OwnedFd;
use std::path::Path;

/// Opens input device nodes on behalf of libinput. The session subsystem
/// implements this so vigil-input does not depend on a particular seat API.
pub trait DeviceOpener {
    fn open(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, String>;
    fn close(&mut self, fd: OwnedFd);
}

/// XKB keymap selection (RMLVO). Empty fields mean "system default" —
/// xkbcommon resolves each empty name itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapSettings {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

/// Normalized input, decoupled from libinput/xkb types. Key repeat is
/// synthesized by vigil-input and arrives as ordinary `Key` events.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key {
        keysym: u32,
        utf8: Option<String>,
        pressed: bool,
    },
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    PointerAbsolute {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
}

// ---------------------------------------------------------------------------
// Auth ⇄ UI
// ---------------------------------------------------------------------------

/// The auth state machine's view of the UI (implemented by vigil-ui).
pub trait AuthUi {
    fn show_prompt(&mut self, text: &str, secret: bool);
    fn show_info(&mut self, text: &str);
    fn show_error(&mut self, text: &str);
    fn set_busy(&mut self, busy: bool);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Reboot,
    Poweroff,
}

/// Messages flowing back from the UI to the auth state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum UiMessage {
    Respond(String),
    Cancel,
    SelectSession(usize),
    SelectUser(usize),
    Power(PowerAction),
}

/// Events flowing from an auth backend's worker (PAM conversation thread)
/// to the UI loop. The greetd backend drives `AuthUi` in-loop and does not
/// need these; the PAM backend crosses a thread boundary and does.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthEvent {
    Prompt {
        text: String,
        secret: bool,
    },
    Info(String),
    Error(String),
    /// The attempt finished: `Ok` = authenticated, `Err` = failure message.
    Done(Result<(), String>),
}

/// logind session events (org.freedesktop.login1), delivered from
/// vigil-login's worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginEvent {
    /// `Lock` signal — something asked the session to lock.
    Lock,
    /// `Unlock` signal — `loginctl unlock-session`; unlocks WITHOUT auth.
    Unlock,
    /// `PrepareForSleep(b)`: true = about to suspend, false = resumed.
    PrepareForSleep(bool),
}

/// Desktop appearance preference (`org.freedesktop.appearance` color-scheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    NoPreference,
    Dark,
    Light,
}

impl ColorScheme {
    /// Portal values: 1 = prefer dark, 2 = prefer light; the spec says
    /// unknown values are treated as 0 (no preference).
    pub fn from_portal(value: u32) -> Self {
        match value {
            1 => Self::Dark,
            2 => Self::Light,
            _ => Self::NoPreference,
        }
    }

    /// Theme contract string; "" means the theme picks for itself.
    pub fn as_theme_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::NoPreference => "",
        }
    }
}

/// Portal `accent-color` is sRGB in [0,1]; the spec says out-of-range means
/// "unset", and a non-finite component is equally unusable.
pub fn accent_from_portal(rgb: (f64, f64, f64)) -> Option<(f32, f32, f32)> {
    let ok = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    (ok(rgb.0) && ok(rgb.1) && ok(rgb.2)).then_some((rgb.0 as f32, rgb.1 as f32, rgb.2 as f32))
}

/// Appearance changes from the settings portal, one per key (mirrors
/// `SettingChanged`), delivered from vigil-login's worker thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppearanceEvent {
    Scheme(ColorScheme),
    /// `None` = unset; the theme keeps its own default accent.
    Accent(Option<(f32, f32, f32)>),
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Seat/session lifecycle, emitted by vigil-session. On `Pause` the outputs
/// must stop touching DRM; on `Activate` they re-modeset and redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    Pause,
    Activate,
}

// ---------------------------------------------------------------------------
// Backgrounds
// ---------------------------------------------------------------------------

/// Background fit behavior, one per output (DESIGN.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundFit {
    Stretch,
    #[default]
    Fill,
    Fit,
    Center,
    Tile,
}

impl BackgroundFit {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stretch" => Some(Self::Stretch),
            "fill" => Some(Self::Fill),
            "fit" => Some(Self::Fit),
            "center" => Some(Self::Center),
            "tile" => Some(Self::Tile),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_rows_are_uniform() {
        for row in CURSOR {
            assert_eq!(row.len(), CURSOR[0].len());
        }
    }

    #[test]
    fn cursor_argb_scale_1_matches_bitmap() {
        let (b, w, h) = cursor_argb(1.0);
        assert_eq!((w, h), (12, 19));
        assert_eq!(&b[0..4], &[0, 0, 0, 0xff], "outline at (0, 0)");
        let fill = ((2 * 12) + 1) * 4;
        assert_eq!(&b[fill..fill + 4], &[0xff, 0xff, 0xff, 0xff], "fill at (1, 2)");
        let clear = 11 * 4;
        assert_eq!(b[clear + 3], 0, "transparent at (11, 0)");
    }

    #[test]
    fn cursor_argb_scale_2_doubles() {
        let (b, w, h) = cursor_argb(2.0);
        assert_eq!((w, h), (24, 38));
        assert_eq!(&b[0..4], &[0, 0, 0, 0xff]);
        let px11 = ((24) + 1) * 4;
        assert_eq!(&b[px11..px11 + 4], &[0, 0, 0, 0xff]);
        let clear = 23 * 4;
        assert_eq!(b[clear + 3], 0);
    }

    #[test]
    fn background_fit_parses_all_documented_modes() {
        for (s, v) in [
            ("stretch", BackgroundFit::Stretch),
            ("fill", BackgroundFit::Fill),
            ("fit", BackgroundFit::Fit),
            ("center", BackgroundFit::Center),
            ("tile", BackgroundFit::Tile),
        ] {
            assert_eq!(BackgroundFit::parse(s), Some(v));
        }
        assert_eq!(BackgroundFit::parse("cover"), None);
    }

    #[test]
    fn color_scheme_from_portal_maps_spec_values() {
        assert_eq!(ColorScheme::from_portal(1), ColorScheme::Dark);
        assert_eq!(ColorScheme::from_portal(2), ColorScheme::Light);
        assert_eq!(ColorScheme::from_portal(0), ColorScheme::NoPreference);
        assert_eq!(ColorScheme::from_portal(7), ColorScheme::NoPreference);
    }

    #[test]
    fn color_scheme_theme_strings() {
        assert_eq!(ColorScheme::Dark.as_theme_str(), "dark");
        assert_eq!(ColorScheme::Light.as_theme_str(), "light");
        assert_eq!(ColorScheme::NoPreference.as_theme_str(), "");
    }

    #[test]
    fn accent_in_range_converts() {
        assert_eq!(accent_from_portal((0.0, 0.5, 1.0)), Some((0.0, 0.5, 1.0)));
    }

    #[test]
    fn accent_out_of_range_is_unset() {
        assert_eq!(accent_from_portal((1.5, 0.0, 0.0)), None);
        assert_eq!(accent_from_portal((-0.1, 0.0, 0.0)), None);
    }

    #[test]
    fn accent_non_finite_is_unset() {
        assert_eq!(accent_from_portal((f64::NAN, 0.0, 0.0)), None);
    }
}
