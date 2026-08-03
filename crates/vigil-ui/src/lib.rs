//! UI subsystem (DESIGN.md §5): the custom Slint `Platform` (one full-output
//! `Window` per output — validated by spike M0b), per-output background
//! bitmaps, the software cursor as a scene element, and the AuthUi
//! implementation that binds theme contract properties.

use vigil_core::{AuthUi, BackgroundFit, InputEvent, OutputId};

/// The custom Slint platform. Window adapters are created one at a time and
/// captured per output (M0b's adapter-capture pattern).
pub struct VigilPlatform {
    _private: (),
}

impl VigilPlatform {
    /// Install as the process-wide Slint platform. Call once, before any
    /// component is instantiated.
    pub fn install() -> Result<(), slint::PlatformError> {
        todo!("M1: slint::platform::set_platform")
    }
}

/// One output's scene: Slint window + theme instance + per-output state.
pub struct OutputWindow {
    _private: (),
}

impl OutputWindow {
    pub fn new(_id: OutputId, _width: u32, _height: u32, _scale: f32) -> Self {
        todo!("M1")
    }

    /// Set the pre-fit background bitmap (from `background` below).
    pub fn set_background(&mut self, _rgba: Vec<u8>, _width: u32, _height: u32) {
        todo!("M1")
    }

    /// Whether this output hosts the login panel (theme `panel-visible`).
    pub fn set_panel_visible(&mut self, _visible: bool) {
        todo!("M1")
    }

    /// Route a normalized input event into this window.
    pub fn dispatch(&mut self, _event: InputEvent) {
        todo!("M1")
    }

    /// Render into the target if dirty; returns whether pixels changed.
    pub fn render_if_needed(&mut self, _target: vigil_core::FrameTarget<'_>) -> bool {
        todo!("M1")
    }
}

impl AuthUi for OutputWindow {
    fn show_prompt(&mut self, _text: &str, _secret: bool) {
        todo!("M1")
    }
    fn show_info(&mut self, _text: &str) {
        todo!("M1")
    }
    fn show_error(&mut self, _text: &str) {
        todo!("M1")
    }
    fn set_busy(&mut self, _busy: bool) {
        todo!("M1")
    }
}

/// Decode `_path` and produce an RGBA bitmap of exactly `_out_w x _out_h`
/// per `_fit` (stretch/fill/fit/center/tile). Pure image math (M2 completes
/// all five modes; M1 ships fill).
pub fn background(
    _path: &std::path::Path,
    _fit: BackgroundFit,
    _out_w: u32,
    _out_h: u32,
) -> Result<Vec<u8>, String> {
    todo!("M1: fill; M2: all five modes")
}
