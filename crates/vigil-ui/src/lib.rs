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
        let adapter = // Full repaint per frame: correct with a double-buffered swapchain we
        // don't age-track yet; damage-tracked swapchain is a later optimization.
        MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
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
    adapter: Rc<MinimalSoftwareWindow>,
    component: ComponentInstance,
}

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
            adapter,
            component,
        })
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
            }
            InputEvent::PointerAbsolute { x, y } => {
                self.pointer_x = x.clamp(0.0, self.width.saturating_sub(1) as f64);
                self.pointer_y = y.clamp(0.0, self.height.saturating_sub(1) as f64);
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: self.pointer_position(),
                });
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
        let Ok(pixels) = bytemuck::try_cast_slice_mut::<u8, Xrgb8888>(target.buffer) else {
            if debug {
                eprintln!("vigil-ui: buffer not 4-byte aligned");
            }
            return false;
        };
        let pixel_stride = target.stride / 4;
        self.adapter.draw_if_needed(|renderer| {
            renderer.render(pixels, pixel_stride);
        })
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
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use image::Rgba;

    use super::*;

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);
    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);

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
