//! UI subsystem (DESIGN.md §5): the custom Slint `Platform` (one full-output
//! `Window` per output — validated by spike M0b), per-output background
//! bitmaps, the software cursor as a scene element, and the AuthUi
//! implementation that binds theme contract properties.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use image::{DynamicImage, RgbaImage, imageops};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{
    Key, Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
};
use slint::{ComponentHandle, Image, LogicalPosition, PhysicalSize, Rgba8Pixel, SharedPixelBuffer};
use slint_interpreter::{ComponentInstance, Value};
use vigil_core::{
    AuthUi, BackgroundFit, FrameTarget, InputEvent, OutputId, PowerAction, UiMessage,
};

/// The custom Slint platform. Window adapters are created one at a time and
/// captured per output (M0b's adapter-capture pattern).
#[derive(Clone, Default)]
pub struct VigilPlatform {
    adapters: Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>,
}

impl VigilPlatform {
    /// Install as the process-wide Slint platform. Call once, before any
    /// component is instantiated.
    pub fn install() -> Result<Self, slint::platform::SetPlatformError> {
        let platform = Self::default();
        slint::platform::set_platform(Box::new(platform.clone()))?;
        Ok(platform)
    }

    /// Claim the adapter created by the most recently instantiated component.
    pub fn claim_last_adapter(&self) -> Option<Rc<MinimalSoftwareWindow>> {
        self.adapters.borrow_mut().pop()
    }
}

impl Platform for VigilPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        // Partial repaints into the window's persistent shadow buffer; each
        // present then copies the shadow to the (possibly alternating)
        // output buffer. Software-rendering a full 4K scene per frame is
        // what made the first on-metal run drop keystrokes.
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        self.adapters.borrow_mut().push(adapter.clone());
        Ok(adapter)
    }
}

/// One output's scene: Slint window + theme instance + per-output state.
pub struct OutputWindow {
    _id: OutputId,
    width: u32,
    height: u32,
    scale: f32,
    pointer_x: f64,
    pointer_y: f64,
    cursor_visible: bool,
    /// Persistent scene buffer (tightly packed XRGB8888). Slint partial-
    /// repaints into it; presents copy it out. Keeps cursor motion at
    /// memcpy cost instead of a full software re-render.
    shadow: Vec<u8>,
    /// The next present must copy out even if the scene didn't change
    /// (cursor moved/toggled, or a fresh swapchain buffer needs filling).
    needs_present: bool,
    adapter: Rc<MinimalSoftwareWindow>,
    component: ComponentInstance,
}

/// The software cursor (DESIGN.md §3: scene element, no cursor plane).
/// `X` outline, `#` fill, `.` transparent; scaled by the output's HiDPI
/// factor at blit time.
const CURSOR: &[&[u8]] = &[
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

impl OutputWindow {
    /// Bind an interpreter component to the adapter captured while it was instantiated.
    pub fn new(
        id: OutputId,
        width: u32,
        height: u32,
        scale: f32,
        adapter: Rc<MinimalSoftwareWindow>,
        component: ComponentInstance,
    ) -> Result<Self, PlatformError> {
        let scale = scale.max(f32::EPSILON);
        component
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        adapter.set_size(PhysicalSize::new(width, height));
        component.show()?;
        Ok(Self {
            _id: id,
            width,
            height,
            scale,
            pointer_x: 0.0,
            pointer_y: 0.0,
            cursor_visible: false,
            shadow: vec![0u8; width as usize * height as usize * 4],
            needs_present: true,
            adapter,
            component,
        })
    }

    /// Whether this output draws the software cursor (the one under the
    /// pointer). The cursor is composited at present time, never rendered
    /// by Slint, so toggling or moving it costs a copy, not a re-render.
    pub fn set_cursor_visible(&mut self, visible: bool) {
        if self.cursor_visible != visible {
            self.cursor_visible = visible;
            self.needs_present = true;
        }
    }

    /// Set the pre-fit background bitmap (from `background` below).
    pub fn set_background(&mut self, rgba: Vec<u8>, width: u32, height: u32) {
        if rgba.len() != width as usize * height as usize * 4 {
            return;
        }
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, width, height);
        self.set_property("background-source", Value::Image(Image::from_rgba8(buffer)));
    }

    /// Whether this output hosts the login panel (theme `panel-visible`).
    pub fn set_panel_visible(&mut self, visible: bool) {
        self.set_property("panel-visible", Value::Bool(visible));
    }

    /// Route a normalized input event into this window.
    pub fn dispatch(&mut self, event: InputEvent) {
        let window = self.component.window();
        match event {
            InputEvent::PointerMotion { dx, dy } => {
                self.pointer_x =
                    (self.pointer_x + dx).clamp(0.0, self.width.saturating_sub(1) as f64);
                self.pointer_y =
                    (self.pointer_y + dy).clamp(0.0, self.height.saturating_sub(1) as f64);
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: self.pointer_position(),
                });
                if self.cursor_visible {
                    self.needs_present = true;
                }
            }
            InputEvent::PointerAbsolute { x, y } => {
                self.pointer_x = x.clamp(0.0, self.width.saturating_sub(1) as f64);
                self.pointer_y = y.clamp(0.0, self.height.saturating_sub(1) as f64);
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: self.pointer_position(),
                });
                if self.cursor_visible {
                    self.needs_present = true;
                }
            }
            InputEvent::PointerButton { button, pressed } => {
                let button = pointer_button(button);
                let position = self.pointer_position();
                let event = if pressed {
                    WindowEvent::PointerPressed { position, button }
                } else {
                    WindowEvent::PointerReleased { position, button }
                };
                window.dispatch_event(event);
            }
            InputEvent::Key {
                keysym,
                utf8,
                pressed,
            } => {
                if let Some(text) = key_text(keysym, utf8) {
                    let event = if pressed {
                        WindowEvent::KeyPressed { text }
                    } else {
                        WindowEvent::KeyReleased { text }
                    };
                    window.dispatch_event(event);
                }
            }
        }
    }

    /// Render into the target if dirty; returns whether pixels changed.
    pub fn render_if_needed(&mut self, target: FrameTarget<'_>) -> bool {
        let debug = std::env::var_os("VIGIL_DEBUG_FRAMES").is_some();
        if target.width != self.width
            || target.height != self.height
            || !target.stride.is_multiple_of(4)
            || target.buffer.len() < target.stride.saturating_mul(target.height as usize)
        {
            if debug {
                eprintln!(
                    "vigil-ui: target mismatch: got {}x{} stride {} len {}, want {}x{}",
                    target.width,
                    target.height,
                    target.stride,
                    target.buffer.len(),
                    self.width,
                    self.height
                );
            }
            return false;
        }
        // Slint partial-repaints into the persistent shadow (ReusedBuffer
        // contract: same buffer, contents preserved between renders).
        let shadow_stride = self.width as usize;
        {
            let shadow_pixels = bytemuck::cast_slice_mut::<u8, Xrgb8888>(&mut self.shadow);
            if self.adapter.draw_if_needed(|renderer| {
                renderer.render(shadow_pixels, shadow_stride);
            }) {
                self.needs_present = true;
            }
        }
        if !self.needs_present {
            return false;
        }
        // Copy out row-wise (the target may be an alternating swapchain
        // buffer with a wider stride), then composite the cursor on top —
        // the shadow itself never contains it.
        let row_bytes = self.width as usize * 4;
        for y in 0..self.height as usize {
            target.buffer[y * target.stride..y * target.stride + row_bytes]
                .copy_from_slice(&self.shadow[y * row_bytes..(y + 1) * row_bytes]);
        }
        if self.cursor_visible {
            let Ok(pixels) = bytemuck::try_cast_slice_mut::<u8, Xrgb8888>(target.buffer) else {
                if debug {
                    eprintln!("vigil-ui: buffer not 4-byte aligned");
                }
                return true;
            };
            self.blit_cursor(pixels, target.stride / 4);
        }
        self.needs_present = false;
        true
    }

    /// Overlay the software cursor into the just-rendered frame, scaled to
    /// the output's HiDPI factor (nearest neighbor — it is a pointer).
    fn blit_cursor(&self, pixels: &mut [Xrgb8888], pixel_stride: usize) {
        let scale = f64::from(self.scale.max(1.0));
        let out_w = (CURSOR[0].len() as f64 * scale) as usize;
        let out_h = (CURSOR.len() as f64 * scale) as usize;
        let (base_x, base_y) = (self.pointer_x as usize, self.pointer_y as usize);
        for oy in 0..out_h {
            let py = base_y + oy;
            if py >= self.height as usize {
                break;
            }
            let row = CURSOR[((oy as f64 / scale) as usize).min(CURSOR.len() - 1)];
            for ox in 0..out_w {
                let px = base_x + ox;
                if px >= self.width as usize {
                    break;
                }
                match row[((ox as f64 / scale) as usize).min(row.len() - 1)] {
                    b'X' => pixels[py * pixel_stride + px] = Xrgb8888(0),
                    b'#' => pixels[py * pixel_stride + px] = Xrgb8888(0x00ff_ffff),
                    _ => {}
                }
            }
        }
    }

    fn pointer_position(&self) -> LogicalPosition {
        LogicalPosition::new(
            (self.pointer_x / f64::from(self.scale)) as f32,
            (self.pointer_y / f64::from(self.scale)) as f32,
        )
    }

    /// Wire the theme's contract callbacks (submit/cancel/session-changed/
    /// power-action) into a single sink of [`UiMessage`]s.
    pub fn on_ui_message(&self, sink: std::rc::Rc<dyn Fn(UiMessage)>) {
        let s = sink.clone();
        let r = self.component.set_callback("submit", move |args| {
            if let Some(Value::String(text)) = args.first() {
                s(UiMessage::Respond(text.to_string()));
            }
            Value::Void
        });
        debug_assert!(r.is_ok());
        let s = sink.clone();
        let r = self.component.set_callback("cancel", move |_| {
            s(UiMessage::Cancel);
            Value::Void
        });
        debug_assert!(r.is_ok());
        let s = sink.clone();
        let r = self.component.set_callback("session-changed", move |args| {
            if let Some(Value::Number(index)) = args.first()
                && *index >= 0.0
            {
                s(UiMessage::SelectSession(*index as usize));
            }
            Value::Void
        });
        debug_assert!(r.is_ok());
        let s = sink.clone();
        let _ = self.component.set_callback("user-changed", move |args| {
            if let Some(Value::Number(index)) = args.first()
                && *index >= 0.0
            {
                s(UiMessage::SelectUser(*index as usize));
            }
            Value::Void
        });
        let r = self.component.set_callback("power-action", move |args| {
            if let Some(Value::String(action)) = args.first() {
                match action.as_str() {
                    "reboot" => sink(UiMessage::Power(PowerAction::Reboot)),
                    "poweroff" => sink(UiMessage::Power(PowerAction::Poweroff)),
                    _ => {}
                }
            }
            Value::Void
        });
        debug_assert!(r.is_ok());
    }

    /// Theme contract `clock-text`.
    pub fn set_clock(&mut self, text: &str) {
        self.set_property("clock-text", Value::String(text.into()));
    }

    /// Theme contract `caps-lock`.
    pub fn set_caps_lock(&mut self, on: bool) {
        self.set_property("caps-lock", Value::Bool(on));
    }

    /// Theme contract `status-banner` (reserved host-integration line;
    /// empty hides it).
    pub fn set_status_banner(&mut self, text: &str) {
        self.set_property("status-banner", Value::String(text.into()));
    }

    /// Optional theme property `user-name` (not in contract v1): set
    /// best-effort, silently skipped on themes that lack it.
    pub fn set_user_name(&mut self, name: &str) {
        let _ = self
            .component
            .set_property("user-name", Value::String(name.into()));
    }

    /// Theme contract `sessions`: display names of the launchable sessions.
    pub fn set_sessions(&mut self, names: &[String]) {
        let model: Vec<Value> = names
            .iter()
            .map(|n| Value::String(n.as_str().into()))
            .collect();
        self.set_property(
            "sessions",
            Value::Model(slint::ModelRc::new(slint::VecModel::from(model))),
        );
    }

    /// Theme contract `selected-session`.
    pub fn set_session_index(&mut self, index: usize) {
        self.set_property("selected-session", Value::Number(index as f64));
    }

    /// Optional theme property `users` (contract v2): the selectable user
    /// names, `Other…` last. Empty on themes that predate the list.
    pub fn set_users(&mut self, names: &[String]) {
        let model: Vec<Value> = names
            .iter()
            .map(|n| Value::String(n.as_str().into()))
            .collect();
        let _ = self.component.set_property(
            "users",
            Value::Model(slint::ModelRc::new(slint::VecModel::from(model))),
        );
    }

    /// Optional theme property `selected-user` (contract v2).
    pub fn set_user_index(&mut self, index: usize) {
        let _ = self
            .component
            .set_property("selected-user", Value::Number(index as f64));
    }

    fn set_property(&self, name: &str, value: Value) {
        let result = self.component.set_property(name, value);
        debug_assert!(result.is_ok());
    }
}

impl AuthUi for OutputWindow {
    fn show_prompt(&mut self, text: &str, secret: bool) {
        self.set_property("prompt-text", Value::String(text.into()));
        self.set_property("prompt-is-secret", Value::Bool(secret));
        self.set_property("info-message", Value::String("".into()));
        self.set_property("error-message", Value::String("".into()));
        self.set_property("auth-state", Value::String("prompting".into()));
    }
    fn show_info(&mut self, text: &str) {
        self.set_property("info-message", Value::String(text.into()));
    }
    fn show_error(&mut self, text: &str) {
        self.set_property("error-message", Value::String(text.into()));
        self.set_property("auth-state", Value::String("error".into()));
    }
    fn set_busy(&mut self, busy: bool) {
        let state = if busy { "busy" } else { "idle" };
        self.set_property("auth-state", Value::String(state.into()));
    }
}

#[derive(Clone, Copy)]
#[repr(transparent)]
struct Xrgb8888(u32);

unsafe impl bytemuck::Zeroable for Xrgb8888 {}
unsafe impl bytemuck::Pod for Xrgb8888 {}

impl TargetPixel for Xrgb8888 {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let inverse_alpha = u16::from(u8::MAX - color.alpha);
        let red = (((self.0 >> 16) & 0xff) as u16 * inverse_alpha / 255) as u8 + color.red;
        let green = (((self.0 >> 8) & 0xff) as u16 * inverse_alpha / 255) as u8 + color.green;
        let blue = ((self.0 & 0xff) as u16 * inverse_alpha / 255) as u8 + color.blue;
        *self = Self::from_rgb(red, green, blue);
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue))
    }
}

fn pointer_button(button: u32) -> PointerEventButton {
    match button {
        0x110 => PointerEventButton::Left,
        0x111 => PointerEventButton::Right,
        0x112 => PointerEventButton::Middle,
        0x116 => PointerEventButton::Back,
        0x115 => PointerEventButton::Forward,
        _ => PointerEventButton::Other,
    }
}

fn key_text(keysym: u32, utf8: Option<String>) -> Option<slint::SharedString> {
    // Special keys FIRST: xkb yields control utf8 for these ("\r", "\x08", …)
    // which would shadow the slint key codes slint actually listens for
    // (e.g. TextInput `accepted` wants Key::Return = '\n', not '\r').
    let key = match keysym {
        0xff0d => Key::Return,
        0xff08 => Key::Backspace,
        0xff09 => Key::Tab,
        0xff1b => Key::Escape,
        0xff51 => Key::LeftArrow,
        0xff52 => Key::UpArrow,
        0xff53 => Key::RightArrow,
        0xff54 => Key::DownArrow,
        0xffff => Key::Delete,
        0xff50 => Key::Home,
        0xff57 => Key::End,
        _ => {
            return utf8
                .filter(|text| !text.is_empty() && !text.chars().all(char::is_control))
                .map(Into::into);
        }
    };
    Some(key.into())
}

/// Per-output background resolution, one precedence rule per dimension:
/// CLI > `[output."NAME"]` override > `[look]` > default. An override keys
/// on the exact connector name; with two GPUs exposing the same name
/// (e.g. two DP-1s) the override applies to every one of them.
pub struct Looks {
    pub cli_background: Option<std::path::PathBuf>,
    pub cli_fit: Option<BackgroundFit>,
    pub config: vigil_config::Config,
}

impl Looks {
    pub fn for_connector(&self, connector: &str) -> (Option<std::path::PathBuf>, BackgroundFit) {
        let over = self.config.output.get(connector);
        let background = self
            .cli_background
            .clone()
            .or_else(|| over.and_then(|o| o.background.clone()))
            .or_else(|| self.config.look.background.clone());
        let fit = self
            .cli_fit
            .or_else(|| over.and_then(|o| parse_fit(o.fit.as_deref(), connector)))
            .or_else(|| parse_fit(self.config.look.fit.as_deref(), "look"))
            .unwrap_or_default();
        (background, fit)
    }
}

/// Parse a config fit string, logging bad values (they fall through to the
/// next precedence level rather than erroring).
fn parse_fit(value: Option<&str>, context: &str) -> Option<BackgroundFit> {
    let value = value?;
    let parsed = BackgroundFit::parse(value);
    if parsed.is_none() {
        eprintln!("vigil-ui: config: unknown fit `{value}` ({context})");
    }
    parsed
}

/// Decode `_path` and produce an RGBA bitmap of exactly `_out_w x _out_h`
/// per `_fit` (stretch/fill/fit/center/tile). Pure image math (M2 completes
/// all five modes; M1 ships fill).
pub fn background(
    path: &Path,
    fit: BackgroundFit,
    out_w: u32,
    out_h: u32,
) -> Result<Vec<u8>, String> {
    if out_w == 0 || out_h == 0 {
        return Err("background dimensions must be non-zero".to_owned());
    }
    let source = image::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(fit_background(&source, fit, out_w, out_h).into_raw())
}

fn fit_background(source: &DynamicImage, fit: BackgroundFit, out_w: u32, out_h: u32) -> RgbaImage {
    let source = source.to_rgba8();
    let (in_w, in_h) = source.dimensions();
    let mut output = RgbaImage::from_pixel(out_w, out_h, image::Rgba([0, 0, 0, 255]));
    match fit {
        BackgroundFit::Stretch => imageops::resize(&source, out_w, out_h, imageops::Lanczos3),
        BackgroundFit::Fill | BackgroundFit::Fit => {
            let fill = fit == BackgroundFit::Fill;
            let scale_w = out_w as f64 / in_w as f64;
            let scale_h = out_h as f64 / in_h as f64;
            let scale = if fill {
                scale_w.max(scale_h)
            } else {
                scale_w.min(scale_h)
            };
            let width = (in_w as f64 * scale).round().max(1.0) as u32;
            let height = (in_h as f64 * scale).round().max(1.0) as u32;
            let resized = imageops::resize(&source, width, height, imageops::Lanczos3);
            let x = (i64::from(out_w) - i64::from(width)).div_euclid(2);
            let y = (i64::from(out_h) - i64::from(height)).div_euclid(2);
            imageops::overlay(&mut output, &resized, x, y);
            output
        }
        BackgroundFit::Center => {
            let x = (i64::from(out_w) - i64::from(in_w)).div_euclid(2);
            let y = (i64::from(out_h) - i64::from(in_h)).div_euclid(2);
            imageops::overlay(&mut output, &source, x, y);
            output
        }
        BackgroundFit::Tile => {
            for y in (0..out_h).step_by(in_h as usize) {
                for x in (0..out_w).step_by(in_w as usize) {
                    imageops::overlay(&mut output, &source, i64::from(x), i64::from(y));
                }
            }
            output
        }
    }
}

/// Last state pushed through [`AuthUi`], kept so scenes rebuilt after a VT
/// switch, resume, or hotplug can be brought back to what the user was
/// looking at — a fresh theme instance starts blank (found twice: greeter
/// VT round trip, then the lockscreen's post-resume "bare password box").
#[derive(Default)]
pub struct UiSnapshot {
    pub prompt: (String, bool),
    pub info: String,
    pub error: String,
    pub busy: bool,
}

impl UiSnapshot {
    /// Record an [`AuthUi`] call so `apply` can replay it later. Callers'
    /// AuthUi impls forward each method here alongside their fan-out.
    pub fn on_prompt(&mut self, text: &str, secret: bool) {
        self.prompt = (text.to_owned(), secret);
        self.info.clear();
        self.error.clear();
    }

    pub fn apply(&self, window: &mut OutputWindow) {
        window.show_prompt(&self.prompt.0, self.prompt.1);
        if !self.info.is_empty() {
            window.show_info(&self.info);
        }
        if !self.error.is_empty() {
            window.show_error(&self.error);
        }
        if self.busy {
            window.set_busy(true);
        }
    }
}

/// Advance Slint timers and animations at the start of an event-loop iteration.
pub fn advance_timers() {
    slint::platform::update_timers_and_animations();
}

/// Return the maximum duration the event loop may sleep before the next Slint timer.
pub fn duration_until_next_timer_update() -> Option<Duration> {
    slint::platform::duration_until_next_timer_update()
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use image::Rgba;

    use super::*;

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);
    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);
    const BASE: &str = "[look]\nbackground = \"/global.png\"\nfit = \"tile\"\n[output.\"DP-1\"]\nbackground = \"/side.png\"\nfit = \"center\"";

    fn looks(toml: &str, cli_bg: Option<&str>, cli_fit: Option<BackgroundFit>) -> Looks {
        Looks {
            cli_background: cli_bg.map(PathBuf::from),
            cli_fit,
            config: vigil_config::parse(toml).unwrap(),
        }
    }

    #[test]
    fn override_beats_look() {
        assert_eq!(
            looks(BASE, None, None).for_connector("DP-1"),
            (Some("/side.png".into()), BackgroundFit::Center)
        );
    }

    #[test]
    fn missing_override_falls_to_look() {
        assert_eq!(
            looks(BASE, None, None).for_connector("eDP-2"),
            (Some("/global.png".into()), BackgroundFit::Tile)
        );
    }

    #[test]
    fn cli_beats_override() {
        assert_eq!(
            looks(BASE, Some("/cli.png"), Some(BackgroundFit::Fit)).for_connector("DP-1"),
            (Some("/cli.png".into()), BackgroundFit::Fit)
        );
    }

    #[test]
    fn bad_override_fit_falls_back_to_look_fit() {
        let (_, fit) = looks(
            "[output.\"DP-1\"]\nfit = \"cover\"\n[look]\nfit = \"tile\"",
            None,
            None,
        )
        .for_connector("DP-1");
        assert_eq!(fit, BackgroundFit::Tile);
    }

    #[test]
    fn no_config_no_cli_defaults() {
        assert_eq!(
            looks("", None, None).for_connector("DP-1"),
            (None, BackgroundFit::Fill)
        );
    }

    fn stripes() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(
            2,
            1,
            |x, _| if x == 0 { RED } else { BLUE },
        ))
    }

    fn pixels(image: &RgbaImage) -> Vec<Rgba<u8>> {
        image.pixels().copied().collect()
    }

    #[test]
    fn stretch_resizes_to_the_full_output() {
        let image = fit_background(&stripes(), BackgroundFit::Stretch, 2, 2);
        assert_eq!(pixels(&image), vec![RED, BLUE, RED, BLUE]);
    }

    #[test]
    fn fill_covers_and_center_crops() {
        let solid = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 1, RED));
        let image = fit_background(&solid, BackgroundFit::Fill, 2, 2);
        assert_eq!(pixels(&image), vec![RED; 4]);
    }

    #[test]
    fn fit_letterboxes_with_opaque_black() {
        let image = fit_background(&stripes(), BackgroundFit::Fit, 2, 3);
        assert_eq!(pixels(&image), vec![BLACK, BLACK, RED, BLUE, BLACK, BLACK]);
    }

    #[test]
    fn center_places_unscaled_image_and_crops_larger_images() {
        let image = fit_background(&stripes(), BackgroundFit::Center, 4, 3);
        assert_eq!(
            pixels(&image),
            vec![
                BLACK, BLACK, BLACK, BLACK, BLACK, RED, BLUE, BLACK, BLACK, BLACK, BLACK, BLACK
            ]
        );
        let cropped = fit_background(&stripes(), BackgroundFit::Center, 1, 1);
        assert_eq!(pixels(&cropped), vec![BLUE]);
    }

    #[test]
    fn tile_repeats_from_top_left() {
        let image = fit_background(&stripes(), BackgroundFit::Tile, 5, 2);
        assert_eq!(
            pixels(&image),
            vec![RED, BLUE, RED, BLUE, RED, RED, BLUE, RED, BLUE, RED]
        );
    }

    #[test]
    fn interpreter_component_renders_into_xrgb_target() {
        let platform = VigilPlatform::install().unwrap();
        // Compile a tiny interpreter component here instead of depending on vigil-theme;
        // the crate boundary requires vigil-ui to remain theme-implementation agnostic.
        let source = r#"
            export component Smoke inherits Window {
                background: #204060;
            }
        "#;
        let result = block_on(
            slint_interpreter::Compiler::default()
                .build_from_source(source.to_owned(), "smoke.slint".into()),
        );
        assert!(!result.has_errors());
        let component = result.component("Smoke").unwrap().create().unwrap();
        let adapter = platform.claim_last_adapter().unwrap();
        let mut window = OutputWindow::new(OutputId(1), 2, 2, 1.0, adapter, component).unwrap();
        let mut buffer = vec![0_u8; 16];
        assert!(window.render_if_needed(FrameTarget {
            buffer: &mut buffer,
            width: 2,
            height: 2,
            stride: 8,
        }));
        assert_eq!(
            buffer,
            [
                0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0
            ]
        );
    }

    struct ThreadWaker(std::thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::park(),
            }
        }
    }
}
