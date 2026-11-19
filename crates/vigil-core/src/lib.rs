#![forbid(unsafe_code)]

mod damage;

pub use damage::{BufferDamage, Damage, FrameDamage, Rect, next_frame_serial};
pub use vigil_secret::Secret;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u32);

#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    pub connector: String,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub make: Option<String>,
    pub model: Option<String>,
    pub scale: f32,
}

impl OutputInfo {
    pub fn description(&self) -> Option<String> {
        let value = [self.make.as_deref(), self.model.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        monitor_profiles::MonitorIdentity::new(&self.connector, Some(value)).description
    }
}

#[cfg(test)]
mod output_description_tests {
    use super::OutputInfo;

    fn info(connector: &str, model: &str) -> OutputInfo {
        OutputInfo {
            connector: connector.into(),
            width: 0,
            height: 0,
            refresh_mhz: 0,
            make: None,
            model: Some(model.into()),
            scale: 1.0,
        }
    }

    #[test]
    fn drops_only_the_matching_connector_suffix() {
        assert_eq!(
            info("DP-4", "HP Inc. HP E243 CNK7510Y4B (DP-4)").description(),
            Some("HP Inc. HP E243 CNK7510Y4B".into())
        );
        assert_eq!(
            info("DP-4", "HP Inc. HP E243 CNK7510Y4B (DP-1)").description(),
            Some("HP Inc. HP E243 CNK7510Y4B (DP-1)".into())
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputEvent {
    Added(OutputId, OutputInfo),
    Removed(OutputId),
    NeedsRedraw(OutputId),
}

pub struct FrameTarget<'a> {
    pub buffer: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub damage: Option<&'a mut FrameDamage>,
}

impl FrameTarget<'_> {
    pub fn reborrow(&mut self) -> FrameTarget<'_> {
        FrameTarget {
            buffer: &mut *self.buffer,
            width: self.width,
            height: self.height,
            stride: self.stride,
            damage: self.damage.as_deref_mut(),
        }
    }
}

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

#[inline]
pub fn scene_to_panel(transform: u8, sw: usize, sh: usize, sx: usize, sy: usize) -> (usize, usize) {
    match transform {
        1 => (sy, sw - 1 - sx),
        2 => (sw - 1 - sx, sh - 1 - sy),
        3 => (sh - 1 - sy, sx),
        _ => (sx, sy),
    }
}

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
    DeviceLost,
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneView {
    pub scene_size: (u32, u32),
    pub scale: f32,
    pub pointer: (f64, f64),
    pub cursor_visible: bool,
    pub revision: u64,
}

pub trait RenderBackend {
    fn render(&mut self, view: &SceneView, canvas: Canvas<'_>) -> bool;

    fn scene_needs_present(&mut self, _view: &SceneView) -> bool {
        true
    }

    fn request_present(&mut self);

    fn set_native_background(
        &mut self,
        _pixels: std::sync::Arc<[u8]>,
        _width: u32,
        _height: u32,
    ) -> bool {
        false
    }

    fn supports_native_background(&self) -> bool {
        false
    }

    fn clear_native_background(&mut self) {}
}

pub enum Canvas<'a> {
    Cpu(FrameTarget<'a>),
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

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError>;

    fn stage_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        let _ = draw;
        Ok(false)
    }

    fn commit_staged(&mut self) -> Result<bool, PresentError> {
        Ok(false)
    }

    fn invalidate(&mut self);

    fn has_cursor(&self) -> bool {
        false
    }

    fn set_cursor(&mut self, pos: Option<(i32, i32)>) -> bool {
        let _ = pos;
        false
    }

    fn flush_cursor(&mut self) -> Result<bool, PresentError> {
        Ok(false)
    }

    fn vblank(&mut self) {}

    fn flip_pending(&self) -> bool {
        false
    }

    fn crtc_id(&self) -> Option<u32> {
        None
    }
}

use std::os::fd::OwnedFd;
use std::path::Path;

pub trait DeviceOpener {
    fn open(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, String>;
    fn close(&mut self, fd: OwnedFd);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeymapSettings {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct KeyText(String);

impl KeyText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for KeyText {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for KeyText {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl Drop for KeyText {
    fn drop(&mut self) {
        vigil_secret::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for KeyText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyText(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key {
        keysym: u32,
        utf8: Option<KeyText>,
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

pub trait AuthUi {
    fn show_prompt(&mut self, text: &str, secret: bool);
    fn show_info(&mut self, text: &str);
    fn show_error(&mut self, text: &str);
    fn set_busy(&mut self, busy: bool);
    fn reset_input(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Reboot,
    Poweroff,
}

#[derive(Debug, PartialEq)]
pub enum UiMessage {
    Respond(Secret),
    Cancel,
    SelectSession(usize),
    SelectUser(usize),
    Power(PowerAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Denied(String),
    Locked {
        message: String,
        remaining: Option<std::time::Duration>,
    },
    Conversation(String),
}

impl AuthError {
    pub fn message(&self) -> &str {
        match self {
            Self::Denied(message) | Self::Locked { message, .. } | Self::Conversation(message) => {
                message
            }
        }
    }

    pub fn is_conversation(&self) -> bool {
        matches!(self, Self::Conversation(_))
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthEvent {
    Prompt { text: String, secret: bool },
    Info(String),
    Error(String),
    Done(Result<(), AuthError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginEvent {
    Lock,
    Unlock,
    PrepareForSleep(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    NoPreference,
    Dark,
    Light,
}

impl ColorScheme {
    pub fn from_portal(value: u32) -> Self {
        match value {
            1 => Self::Dark,
            2 => Self::Light,
            _ => Self::NoPreference,
        }
    }

    pub fn as_theme_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::NoPreference => "",
        }
    }
}

pub fn accent_from_portal(rgb: (f64, f64, f64)) -> Option<(f32, f32, f32)> {
    let ok = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    (ok(rgb.0) && ok(rgb.1) && ok(rgb.2)).then_some((rgb.0 as f32, rgb.1 as f32, rgb.2 as f32))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppearanceEvent {
    Scheme(ColorScheme),
    Accent(Option<(f32, f32, f32)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    Pause,
    Activate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
    fn key_text_reads_back_and_never_prints() {
        let event = InputEvent::Key {
            keysym: 0x7a,
            utf8: Some(KeyText::from("zq")),
            pressed: true,
        };
        let InputEvent::Key {
            utf8: Some(text), ..
        } = &event
        else {
            unreachable!()
        };
        assert_eq!(text.as_str(), "zq");
        let rendered = format!("{event:?}");
        assert!(!rendered.contains("zq"), "{rendered}");
        assert_eq!(text.clone(), KeyText::from(String::from("zq")));
    }

    #[test]
    fn transform_90_rotates_the_scene_counter_clockwise() {
        let (sw, sh) = (2, 3);
        assert_eq!(scene_to_panel(1, sw, sh, 0, 0), (0, 1));
        assert_eq!(scene_to_panel(1, sw, sh, 0, 2), (2, 1));
        assert_eq!(scene_to_panel(1, sw, sh, 1, 0), (0, 0));
    }

    #[test]
    fn transform_270_rotates_the_scene_clockwise() {
        let (sw, sh) = (2, 3);
        assert_eq!(scene_to_panel(3, sw, sh, 0, 0), (2, 0));
        assert_eq!(scene_to_panel(3, sw, sh, 0, 2), (0, 0));
        assert_eq!(scene_to_panel(3, sw, sh, 1, 0), (2, 1));
    }

    #[test]
    fn transform_180_flips_both_axes() {
        let (sw, sh) = (2, 3);
        assert_eq!(scene_to_panel(2, sw, sh, 0, 0), (1, 2));
        assert_eq!(scene_to_panel(2, sw, sh, 1, 2), (0, 0));
    }

    #[test]
    fn transform_0_is_the_identity() {
        assert_eq!(scene_to_panel(0, 2, 3, 1, 2), (1, 2));
    }

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
        assert_eq!(
            &b[fill..fill + 4],
            &[0xff, 0xff, 0xff, 0xff],
            "fill at (1, 2)"
        );
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

pub fn uptime_ms() -> u128 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
}
