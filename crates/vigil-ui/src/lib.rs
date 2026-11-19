use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use image::DynamicImage;
use image::{RgbaImage, imageops};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{
    Key, Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
};
use slint::{
    Color, ComponentHandle, Image, LogicalPosition, Model, PhysicalSize, Rgba8Pixel,
    SharedPixelBuffer, WindowSize,
};
use slint_idle_runtime::{DirtySet, Metrics, RedrawHandle, WakeHandle};
use slint_interpreter::{ComponentInstance, Value};
use slint_kit::TokenSet;
use vigil_core::{
    AuthUi, BackgroundFit, Damage, FrameTarget, InputEvent, OutputId, PowerAction, Rect, UiMessage,
};
use vigil_core::{Canvas as CoreCanvas, RenderBackend, SceneView};

mod focus;
mod scanout;

use focus::{Card, Effect, Focus, Picker, Stop};

#[derive(Clone, Default)]
pub struct VigilPlatform {
    adapters: Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>,
    next: Rc<RefCell<Option<Rc<dyn WindowAdapter>>>>,
    runtime: Rc<RefCell<Option<RuntimeBinding>>>,
    next_output: Rc<RefCell<Option<OutputId>>>,
}

#[derive(Clone)]
struct RuntimeBinding {
    wake: WakeHandle,
    dirty: Arc<DirtySet<OutputId>>,
    metrics: Arc<Metrics>,
}

struct TrackedSoftwareWindow {
    inner: Rc<MinimalSoftwareWindow>,
    redraw: RedrawHandle<OutputId>,
}

impl WindowAdapter for TrackedSoftwareWindow {
    fn window(&self) -> &slint::Window {
        self.inner.window()
    }

    fn set_size(&self, size: WindowSize) {
        WindowAdapter::set_size(self.inner.as_ref(), size);
    }

    fn size(&self) -> PhysicalSize {
        WindowAdapter::size(self.inner.as_ref())
    }

    fn request_redraw(&self) {
        WindowAdapter::request_redraw(self.inner.as_ref());
        self.redraw.request_redraw();
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        WindowAdapter::renderer(self.inner.as_ref())
    }
}

impl VigilPlatform {
    pub fn install() -> Result<Self, slint::platform::SetPlatformError> {
        let platform = Self::default();
        platform.register()?;
        Ok(platform)
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self) -> Result<(), slint::platform::SetPlatformError> {
        slint::platform::set_platform(Box::new(self.clone()))
    }

    pub fn set_runtime(
        &self,
        wake: WakeHandle,
        dirty: Arc<DirtySet<OutputId>>,
        metrics: Arc<Metrics>,
    ) {
        *self.runtime.borrow_mut() = Some(RuntimeBinding {
            wake,
            dirty,
            metrics,
        });
    }

    pub fn set_next_output(&self, output: OutputId) {
        *self.next_output.borrow_mut() = Some(output);
    }

    pub fn clear_next_output(&self) {
        *self.next_output.borrow_mut() = None;
    }

    pub fn claim_last_adapter(&self) -> Option<Rc<MinimalSoftwareWindow>> {
        self.adapters.borrow_mut().pop()
    }

    pub fn use_next_adapter(&self, adapter: Rc<dyn WindowAdapter>) {
        *self.next.borrow_mut() = Some(adapter);
    }

    pub fn clear_adapter_override(&self) {
        *self.next.borrow_mut() = None;
    }

    pub fn adapters_created(&self) -> usize {
        self.adapters.borrow().len()
    }
}

impl Platform for VigilPlatform {
    fn cursor_flash_cycle(&self) -> Duration {
        Duration::ZERO
    }

    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        if let Some(adapter) = self.next.borrow().clone() {
            return Ok(adapter);
        }
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        self.adapters.borrow_mut().push(adapter.clone());
        let Some(runtime) = self.runtime.borrow().clone() else {
            return Ok(adapter);
        };
        let output = self
            .next_output
            .borrow()
            .as_ref()
            .copied()
            .expect("output must be set before a runtime-backed theme is instantiated");
        let redraw = runtime
            .wake
            .redraw_handle(runtime.dirty, runtime.metrics, output);
        redraw.request_redraw();
        Ok(Rc::new(TrackedSoftwareWindow {
            inner: adapter,
            redraw,
        }))
    }
}

pub struct OutputWindow {
    _id: OutputId,
    backend: Box<dyn RenderBackend>,
    width: u32,
    height: u32,
    scale: f32,
    pointer_x: f64,
    pointer_y: f64,
    cursor_visible: bool,
    revision: std::cell::Cell<u64>,
    component: ComponentInstance,
    panel: (u32, u32),
    focus: Focus,
    held: u8,
    down: Option<u32>,
    reveal_timer: slint::Timer,
    reveal_idle: Duration,
}

// Long enough to read back a typed password, short enough that a shown one
// does not stay on an unattended screen. Any key or click restarts it.
pub const REVEAL_IDLE: Duration = Duration::from_secs(10);

pub struct SoftwareBackend {
    shadow: Vec<u8>,
    needs_present: bool,
    adapter: Rc<MinimalSoftwareWindow>,
    panel_width: u32,
    panel_height: u32,
    transform: u8,
    native_background: Option<std::sync::Arc<[u8]>>,
    native_overlay: Vec<u8>,
    full_repaint: bool,
    unpresented: Damage,
    cursor_drawn: Option<Rect>,
    presented: Option<u64>,
}

impl OutputWindow {
    pub fn new(
        id: OutputId,
        width: u32,
        height: u32,
        scale: f32,
        adapter: Rc<MinimalSoftwareWindow>,
        component: ComponentInstance,
    ) -> Result<Self, PlatformError> {
        Self::with_transform(id, width, height, scale, 0, adapter, component)
    }

    pub fn with_backend(
        id: OutputId,
        width: u32,
        height: u32,
        scale: f32,
        component: ComponentInstance,
        backend: Box<dyn RenderBackend>,
    ) -> Result<Self, PlatformError> {
        let scale = scale.max(f32::EPSILON);
        component
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        component.show()?;
        let window = Self {
            _id: id,
            backend,
            width,
            height,
            scale,
            pointer_x: 0.0,
            pointer_y: 0.0,
            cursor_visible: false,
            revision: std::cell::Cell::new(0),
            component,
            panel: (width, height),
            focus: Focus::default(),
            held: 0,
            down: None,
            reveal_timer: slint::Timer::default(),
            reveal_idle: REVEAL_IDLE,
        };
        window.publish_focus();
        Ok(window)
    }

    pub fn with_transform(
        id: OutputId,
        width: u32,
        height: u32,
        scale: f32,
        transform: u8,
        adapter: Rc<MinimalSoftwareWindow>,
        component: ComponentInstance,
    ) -> Result<Self, PlatformError> {
        let scale = scale.max(f32::EPSILON);
        let transform = transform % 4;
        let (panel_width, panel_height) = (width, height);
        let (width, height) = if transform % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        };
        component
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        adapter.set_size(PhysicalSize::new(width, height));
        component.show()?;
        let window = Self {
            _id: id,
            backend: Box::new(SoftwareBackend {
                shadow: vec![0u8; width as usize * height as usize * 4],
                needs_present: true,
                adapter,
                panel_width,
                panel_height,
                transform,
                native_background: None,
                native_overlay: Vec::new(),
                full_repaint: true,
                unpresented: Damage::Full,
                cursor_drawn: None,
                presented: None,
            }),
            width,
            height,
            scale,
            pointer_x: 0.0,
            pointer_y: 0.0,
            cursor_visible: false,
            revision: std::cell::Cell::new(0),
            component,
            panel: (panel_width, panel_height),
            focus: Focus::default(),
            held: 0,
            down: None,
            reveal_timer: slint::Timer::default(),
            reveal_idle: REVEAL_IDLE,
        };
        window.publish_focus();
        Ok(window)
    }

    pub fn request_present(&mut self) {
        self.backend.request_present();
        self.component.window().request_redraw();
    }

    pub fn request_present_deferred(&mut self) {
        self.backend.request_present();
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        if self.cursor_visible != visible {
            self.cursor_visible = visible;
            self.request_present();
        }
    }

    pub fn set_background(&mut self, rgba: Vec<u8>, width: u32, height: u32) {
        self.set_background_pixels(&rgba, width, height);
    }

    pub fn set_background_pixels(&mut self, rgba: &[u8], width: u32, height: u32) {
        if rgba.len() != width as usize * height as usize * 4 {
            return;
        }
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba, width, height);
        self.set_property("background-source", Value::Image(Image::from_rgba8(buffer)));
    }

    pub fn set_native_background_xrgb(
        &mut self,
        xrgb: std::sync::Arc<[u8]>,
        width: u32,
        height: u32,
    ) -> bool {
        if xrgb.len() != width as usize * height as usize * 4
            || (width, height) != self.scene_size()
        {
            return false;
        }
        if self
            .component
            .set_property("native-background", Value::Bool(true))
            .is_err()
        {
            return false;
        }
        if !self.backend.set_native_background(xrgb, width, height) {
            let _ = self
                .component
                .set_property("native-background", Value::Bool(false));
            return false;
        }
        let _ = self
            .component
            .set_property("background-source", Value::Image(Image::default()));
        self.touch();
        true
    }

    pub fn supports_native_background(&self) -> bool {
        self.backend.supports_native_background()
            && self.component.get_property("native-background").is_ok()
    }

    pub fn clear_background(&mut self) {
        self.backend.clear_native_background();
        let _ = self
            .component
            .set_property("native-background", Value::Bool(false));
        self.set_property("background-source", Value::Image(Image::default()));
    }

    pub fn set_panel_visible(&mut self, visible: bool) {
        if !visible {
            for (at, keysym) in vigil_core::MODIFIER_KEYSYMS.into_iter().enumerate() {
                if self.held & (1 << at) != 0 {
                    self.send_key(keysym, None, false);
                }
            }
            self.held = 0;
            self.down = None;
            self.reset_focus();
            self.hide_password();
        }
        self.set_property("panel-visible", Value::Bool(visible));
    }

    pub fn set_power_visible(&mut self, visible: bool) {
        self.set_optional_property("power-visible", Value::Bool(visible));
        if !visible {
            self.reset_focus();
        }
    }

    pub fn field_focused(&self) -> bool {
        self.card()
            .is_none_or(|card| self.focus.settle(&card).stop == Stop::Field)
    }

    pub fn set_reveal_idle(&mut self, idle: Duration) {
        self.reveal_idle = idle;
    }

    fn watch_reveal(&self) {
        if !matches!(self.component.get_property("reveal"), Ok(Value::Bool(true))) {
            self.reveal_timer.stop();
            return;
        }
        let component = self.component.as_weak();
        self.reveal_timer
            .start(slint::TimerMode::SingleShot, self.reveal_idle, move || {
                if let Some(component) = component.upgrade() {
                    let _ = component.set_property("reveal", Value::Bool(false));
                }
            });
    }

    fn hide_password(&self) {
        self.set_optional_property("reveal", Value::Bool(false));
    }

    fn reset_focus(&mut self) {
        self.set_optional_property("confirm-power", Value::String("".into()));
        self.focus = Focus::default();
        self.publish_focus();
    }

    fn publish_focus(&self) {
        self.set_optional_property(
            "keyboard-focus",
            Value::String(self.focus.property().into()),
        );
        self.set_optional_property("key-hint", Value::String(self.focus.stop.hint().into()));
    }

    fn card(&self) -> Option<Card> {
        let c = &self.component;
        c.get_property("keyboard-focus").ok()?;
        if !matches!(c.get_property("panel-visible"), Ok(Value::Bool(true))) {
            return None;
        }
        let flag = |name| matches!(c.get_property(name), Ok(Value::Bool(true)));
        let picker = |list, index| Picker {
            count: match c.get_property(list) {
                Ok(Value::Model(model)) => model.row_count(),
                _ => 0,
            },
            index: match c.get_property(index) {
                Ok(Value::Number(n)) if n >= 0.0 => n as usize,
                _ => 0,
            },
        };
        let confirm = c.get_property("confirm-power");
        Some(Card {
            users: picker("users", "selected-user"),
            sessions: picker("sessions", "selected-session"),
            reveal: flag("prompt-is-secret") && c.get_property("reveal").is_ok(),
            power: confirm.is_ok()
                && !matches!(c.get_property("power-visible"), Ok(Value::Bool(false))),
            confirming: match confirm {
                Ok(Value::String(name)) => focus::parse_action(&name),
                _ => None,
            },
        })
    }

    fn modifiers(&self) -> (bool, bool, bool) {
        (
            self.held & 0b11 != 0,
            self.held & 0b1100 != 0,
            self.held & 0b11_0000 != 0,
        )
    }

    fn focus_key(&self, keysym: u32, utf8: Option<&str>) -> focus::Key {
        use focus::Key as K;
        let (shift, control, alt) = self.modifiers();
        let plain = !control && !alt;
        match keysym {
            0xff09 if plain && shift => K::BackTab,
            0xff09 if plain => K::Tab,
            0xfe20 if plain => K::BackTab,
            0xff0d | 0xff8d => K::Enter,
            0x20 if plain => K::Space,
            0xff1b => K::Escape,
            0xff51 => K::Left,
            0xff52 => K::Up,
            0xff53 => K::Right,
            0xff54 => K::Down,
            0xffc5 if alt && !control => K::RevealShortcut,
            _ if plain && utf8.is_some_and(printable) => K::Text,
            _ => K::Other,
        }
    }

    fn key(&mut self, keysym: u32, utf8: Option<String>, pressed: bool) {
        if let Some(at) = vigil_core::MODIFIER_KEYSYMS
            .iter()
            .position(|k| *k == keysym)
        {
            let kind = 0b11 << (at & !1);
            let was = self.held & kind != 0;
            if pressed {
                self.held |= 1 << at;
            } else {
                self.held &= !(1 << at);
            }
            // Slint keeps one flag per modifier kind, so it hears only the
            // first press and the last release of the pair.
            if was != (self.held & kind != 0) {
                self.send_key(keysym, utf8, pressed);
            }
            return;
        }
        let repeat = pressed && self.down == Some(keysym);
        if pressed {
            self.down = Some(keysym);
        } else if self.down == Some(keysym) {
            self.down = None;
        }
        if repeat && vigil_core::ONE_SHOT_KEYSYMS.contains(&keysym) {
            return;
        }
        // Slint's own Tab moves its focus and selects the field, so typing
        // then replaces the password.
        let tab = matches!(keysym, 0xff09 | 0xfe20);
        let card = match self.card() {
            Some(_) if tab && !pressed => return,
            Some(card) if pressed => card,
            _ => return self.send_key(keysym, utf8, pressed),
        };
        let key = self.focus_key(keysym, utf8.as_deref());
        let (focus, effect) = self.focus.step(&card, key);
        if repeat
            && effect != Effect::Forward
            && !matches!(key, focus::Key::Tab | focus::Key::BackTab)
        {
            return;
        }
        self.focus = focus;
        let c = &self.component;
        let action = |action| Value::String(focus::action_name(action).into());
        match effect {
            Effect::Forward if tab => {}
            Effect::Forward => self.send_key(keysym, utf8, true),
            Effect::Consume => {}
            Effect::User(index) => {
                let _ = c.invoke("user-changed", &[Value::Number(index as f64)]);
            }
            Effect::Session(index) => {
                let _ = c.invoke("session-changed", &[Value::Number(index as f64)]);
            }
            Effect::ToggleReveal => {
                if let Ok(Value::Bool(shown)) = c.get_property("reveal") {
                    self.set_optional_property("reveal", Value::Bool(!shown));
                }
            }
            Effect::Arm(power) => self.set_optional_property("confirm-power", action(power)),
            Effect::Disarm => self.set_optional_property("confirm-power", Value::String("".into())),
            Effect::Fire(power) => {
                self.set_optional_property("confirm-power", Value::String("".into()));
                let _ = c.invoke("power-action", &[action(power)]);
            }
        }
        self.publish_focus();
    }

    fn send_key(&self, keysym: u32, utf8: Option<String>, pressed: bool) {
        let text = if self.modifiers().1 {
            control_text(keysym)
        } else {
            key_text(keysym, utf8)
        };
        if let Some(text) = text {
            let event = if pressed {
                WindowEvent::KeyPressed { text }
            } else {
                WindowEvent::KeyReleased { text }
            };
            self.component.window().dispatch_event(event);
        }
    }

    pub fn set_warning_progress(&mut self, frost: f32, wallpaper: f32) {
        self.set_optional_property("warning-frost", Value::Number(frost as f64));
        self.set_optional_property("warning-wallpaper", Value::Number(wallpaper as f64));
        self.request_present();
    }

    pub fn set_warning_element(&mut self, selector: &str, progress: f32) {
        let property = match selector {
            "clock" => "warning-clock-progress",
            "user_selector" => "warning-user-selector-progress",
            "password" => "warning-password-progress",
            "status" => "warning-status-progress",
            "power" => "warning-power-progress",
            _ => return,
        };
        self.set_optional_property(property, Value::Number(progress.clamp(0.0, 1.0) as f64));
    }

    pub fn dispatch(&mut self, event: InputEvent) {
        if !matches!(
            event,
            InputEvent::PointerMotion { .. } | InputEvent::PointerAbsolute { .. }
        ) {
            self.touch();
        }
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
                    self.request_present();
                }
            }
            InputEvent::PointerAbsolute { x, y } => {
                self.pointer_x = x.clamp(0.0, self.width.saturating_sub(1) as f64);
                self.pointer_y = y.clamp(0.0, self.height.saturating_sub(1) as f64);
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: self.pointer_position(),
                });
                if self.cursor_visible {
                    self.request_present();
                }
            }
            InputEvent::PointerButton { button, pressed } => {
                if pressed && !matches!(self.focus.stop, Stop::Cancel(_) | Stop::Confirm(_)) {
                    self.focus = Focus::default();
                    self.publish_focus();
                }
                let window = self.component.window();
                let button = pointer_button(button);
                let position = self.pointer_position();
                let event = if pressed {
                    WindowEvent::PointerPressed { position, button }
                } else {
                    WindowEvent::PointerReleased { position, button }
                };
                window.dispatch_event(event);
                self.watch_reveal();
            }
            InputEvent::Key {
                keysym,
                utf8,
                pressed,
            } => {
                self.key(keysym, utf8, pressed);
                self.watch_reveal();
            }
        }
    }

    pub fn scene_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn panel_size(&self) -> (u32, u32) {
        self.panel
    }

    pub fn pointer(&self) -> (f64, f64) {
        (self.pointer_x, self.pointer_y)
    }

    fn view(&self) -> SceneView {
        SceneView {
            scene_size: (self.width, self.height),
            scale: self.scale,
            pointer: (self.pointer_x, self.pointer_y),
            cursor_visible: self.cursor_visible,
            revision: self.revision.get(),
        }
    }

    pub fn render(&mut self, canvas: CoreCanvas<'_>) -> bool {
        let view = self.view();
        self.backend.render(&view, canvas)
    }

    pub fn render_if_needed(&mut self, target: FrameTarget<'_>) -> bool {
        self.render(CoreCanvas::Cpu(target))
    }

    pub fn scene_needs_present(&mut self) -> bool {
        let view = self.view();
        self.backend.scene_needs_present(&view)
    }

    fn pointer_position(&self) -> LogicalPosition {
        LogicalPosition::new(
            (self.pointer_x / f64::from(self.scale)) as f32,
            (self.pointer_y / f64::from(self.scale)) as f32,
        )
    }

    pub fn on_ui_message(&self, sink: std::rc::Rc<dyn Fn(UiMessage)>) {
        let s = sink.clone();
        let component = self.component.as_weak();
        let r = self.component.set_callback("submit", move |args| {
            if let Some(component) = component.upgrade() {
                let _ = component.set_property("reveal", Value::Bool(false));
            }
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

    pub fn set_clock(&mut self, text: &str) {
        self.set_property("clock-text", Value::String(text.into()));
    }

    pub fn set_caps_lock(&mut self, on: bool) {
        self.set_property("caps-lock", Value::Bool(on));
    }

    pub fn set_status_banner(&mut self, text: &str) {
        self.set_property("status-banner", Value::String(text.into()));
    }

    pub fn set_user_name(&mut self, name: &str) {
        self.set_optional_property("user-name", Value::String(name.into()));
        let initial: String = name.chars().take(1).flat_map(char::to_uppercase).collect();
        self.set_optional_property("user-initial", Value::String(initial.into()));
    }

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

    pub fn set_session_index(&mut self, index: usize) {
        self.set_property("selected-session", Value::Number(index as f64));
    }

    pub fn set_users(&mut self, names: &[String]) {
        let model: Vec<Value> = names
            .iter()
            .map(|n| Value::String(n.as_str().into()))
            .collect();
        self.set_optional_property(
            "users",
            Value::Model(slint::ModelRc::new(slint::VecModel::from(model))),
        );
    }

    pub fn set_user_index(&mut self, index: usize) {
        self.set_optional_property("selected-user", Value::Number(index as f64));
    }

    pub fn set_color_scheme(&mut self, scheme: &str) {
        self.set_optional_property("color-scheme", Value::String(scheme.into()));
    }

    pub fn set_accent_color(&mut self, rgb: (f32, f32, f32)) {
        self.set_optional_property(
            "accent-color",
            Color::from_rgb_f32(rgb.0, rgb.1, rgb.2).into(),
        );
    }

    pub fn apply_kit_tokens(&mut self, tokens: &TokenSet) {
        self.set_color_scheme(&tokens.mode);
        let _ = self.component.set_global_property(
            "Theme",
            "mode",
            Value::String(tokens.mode.as_str().into()),
        );
        for (name, color) in slint_kit::kit_color_bindings(tokens) {
            let _ = self
                .component
                .set_global_property("Theme", name, color.into());
        }
        self.set_optional_property("accent-color", tokens.get("primary").into());
        let tone = |name| match self.component.get_global_property("Theme", name) {
            Ok(Value::Brush(brush)) => Some(brush.color()),
            _ => None,
        };
        if let (Some(fg), Some(bg)) = (tone("fg"), tone("page-bg")) {
            let _ = self.component.set_global_property(
                "FocusTones",
                "fallback",
                Value::Bool(contrast(fg, bg) < 9.0),
            );
        }
        self.touch();
    }

    fn set_property(&self, name: &str, value: Value) {
        let result = self.component.set_property(name, value);
        debug_assert!(result.is_ok());
        self.touch();
    }

    fn set_optional_property(&self, name: &str, value: Value) {
        let _ = self.component.set_property(name, value);
        self.touch();
    }

    fn touch(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        self.component.window().request_redraw();
    }

    pub fn slint_window(&self) -> &slint::Window {
        self.component.window()
    }
}

pub fn apply_kit_tokens_from_disk(window: &mut OutputWindow, mode: &str) {
    window.apply_kit_tokens(&slint_kit::load_tokens_preferring(mode));
}

pub fn apply_kit_tokens_from_system(window: &mut OutputWindow, mode: &str) {
    window.apply_kit_tokens(&slint_kit::load_tokens_system(mode));
}

impl AuthUi for OutputWindow {
    fn show_prompt(&mut self, text: &str, secret: bool) {
        self.hide_password();
        self.set_property("prompt-text", Value::String(text.into()));
        self.set_property("prompt-is-secret", Value::Bool(secret));
        self.set_property("info-message", Value::String("".into()));
        self.set_property("error-message", Value::String("".into()));
        self.set_property("auth-state", Value::String("prompting".into()));
    }
    fn reset_input(&mut self) {
        self.hide_password();
        if let Ok(Value::Number(serial)) = self.component.get_property("prompt-serial") {
            self.set_optional_property("prompt-serial", Value::Number(serial + 1.0));
        }
    }
    fn show_info(&mut self, text: &str) {
        self.set_property("info-message", Value::String(text.into()));
    }
    fn show_error(&mut self, text: &str) {
        self.hide_password();
        self.set_property("error-message", Value::String(text.into()));
        self.set_property("auth-state", Value::String("error".into()));
        if let Ok(Value::Number(serial)) = self.component.get_property("error-serial") {
            self.set_optional_property("error-serial", Value::Number(serial + 1.0));
        }
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

#[derive(Clone, Copy, Default)]
#[repr(transparent)]
struct Argb8888(u32);

unsafe impl bytemuck::Zeroable for Argb8888 {}
unsafe impl bytemuck::Pod for Argb8888 {}

impl TargetPixel for Argb8888 {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let inverse_alpha = u16::from(u8::MAX - color.alpha);
        let red = color.red as u16 + ((self.0 >> 16) & 0xff) as u16 * inverse_alpha / 255;
        let green = color.green as u16 + ((self.0 >> 8) & 0xff) as u16 * inverse_alpha / 255;
        let blue = color.blue as u16 + (self.0 & 0xff) as u16 * inverse_alpha / 255;
        let alpha = color.alpha as u16 + ((self.0 >> 24) & 0xff) as u16 * inverse_alpha / 255;
        self.0 = u32::from(blue as u8)
            | (u32::from(green as u8) << 8)
            | (u32::from(red as u8) << 16)
            | (u32::from(alpha as u8) << 24);
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(u32::from(blue) | (u32::from(green) << 8) | (u32::from(red) << 16) | 0xff00_0000)
    }

    fn background() -> Self {
        Self(0)
    }
}

fn contrast(a: Color, b: Color) -> f64 {
    let luminance = |color: Color| {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.red())
            + 0.7152 * linear(color.green())
            + 0.0722 * linear(color.blue())
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
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

fn printable(text: &str) -> bool {
    !text.is_empty() && !text.chars().all(char::is_control)
}

fn key_text(keysym: u32, utf8: Option<String>) -> Option<slint::SharedString> {
    let key = match keysym {
        0xffe1 | 0xffe2 => Key::Shift,
        0xffe3 | 0xffe4 => Key::Control,
        0xffe9 | 0xffea => Key::Alt,
        0xffeb | 0xffec => Key::Meta,
        0xff0d | 0xff8d => Key::Return,
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
        _ => return utf8.filter(|text| printable(text)).map(Into::into),
    };
    Some(key.into())
}

// Slint maps Ctrl plus a letter to Undo, Redo, Copy, Cut and Paste, and its
// undo history outlives a field the theme clears, so only Select All passes.
fn control_text(keysym: u32) -> Option<slint::SharedString> {
    match keysym {
        0x61 => Some("a".into()),
        _ => key_text(keysym, None),
    }
}

pub struct Looks {
    pub cli_background: Option<std::path::PathBuf>,
    pub fallback_background: Option<std::path::PathBuf>,
    pub cli_fit: Option<BackgroundFit>,
    pub config: vigil_config::Config,
}

impl Looks {
    pub fn for_connector(&self, connector: &str) -> (Option<std::path::PathBuf>, BackgroundFit) {
        self.for_connector_with_fallback(connector, None, None)
    }

    pub fn for_connector_with_fallback(
        &self,
        connector: &str,
        background_fallback: Option<std::path::PathBuf>,
        fit_fallback: Option<BackgroundFit>,
    ) -> (Option<std::path::PathBuf>, BackgroundFit) {
        let over = self.config.output.get(connector);
        let background = self
            .cli_background
            .clone()
            .or_else(|| over.and_then(|o| o.background.clone()))
            .or_else(|| self.config.look.background.clone())
            .or(background_fallback)
            .or_else(|| self.fallback_background.clone());
        let fit = self
            .cli_fit
            .or_else(|| over.and_then(|o| parse_fit(o.fit.as_deref(), connector)))
            .or_else(|| parse_fit(self.config.look.fit.as_deref(), "look"))
            .or(fit_fallback)
            .unwrap_or_default();
        (background, fit)
    }
}

fn parse_fit(value: Option<&str>, context: &str) -> Option<BackgroundFit> {
    let value = value?;
    let parsed = BackgroundFit::parse(value);
    if parsed.is_none() {
        eprintln!("vigil-ui: config: unknown fit `{value}` ({context})");
    }
    parsed
}

pub fn background(
    path: &Path,
    fit: BackgroundFit,
    out_w: u32,
    out_h: u32,
) -> Result<Vec<u8>, String> {
    if out_w == 0 || out_h == 0 {
        return Err("background dimensions must be non-zero".to_owned());
    }
    let source = load_background(path)?;
    render_background(&source, fit, out_w, out_h)
}

pub struct BackgroundImage(RgbaImage);

pub fn load_background(path: &Path) -> Result<BackgroundImage, String> {
    image::open(path)
        .map(|image| BackgroundImage(image.into_rgba8()))
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub fn render_background(
    source: &BackgroundImage,
    fit: BackgroundFit,
    out_w: u32,
    out_h: u32,
) -> Result<Vec<u8>, String> {
    if out_w == 0 || out_h == 0 {
        return Err("background dimensions must be non-zero".to_owned());
    }
    Ok(fit_rgba(&source.0, fit, out_w, out_h).into_raw())
}

#[cfg(test)]
fn fit_background(source: &DynamicImage, fit: BackgroundFit, out_w: u32, out_h: u32) -> RgbaImage {
    fit_rgba(&source.to_rgba8(), fit, out_w, out_h)
}

fn fit_rgba(source: &RgbaImage, fit: BackgroundFit, out_w: u32, out_h: u32) -> RgbaImage {
    let (in_w, in_h) = source.dimensions();
    let mut output = RgbaImage::from_pixel(out_w, out_h, image::Rgba([0, 0, 0, 255]));
    match fit {
        BackgroundFit::Stretch => imageops::resize(source, out_w, out_h, imageops::Lanczos3),
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
            let resized = imageops::resize(source, width, height, imageops::Lanczos3);
            let x = (i64::from(out_w) - i64::from(width)).div_euclid(2);
            let y = (i64::from(out_h) - i64::from(height)).div_euclid(2);
            imageops::overlay(&mut output, &resized, x, y);
            output
        }
        BackgroundFit::Center => {
            let x = (i64::from(out_w) - i64::from(in_w)).div_euclid(2);
            let y = (i64::from(out_h) - i64::from(in_h)).div_euclid(2);
            imageops::overlay(&mut output, source, x, y);
            output
        }
        BackgroundFit::Tile => {
            for y in (0..out_h).step_by(in_h as usize) {
                for x in (0..out_w).step_by(in_w as usize) {
                    imageops::overlay(&mut output, source, i64::from(x), i64::from(y));
                }
            }
            output
        }
    }
}

pub mod bgcache {
    use super::BackgroundFit;
    use std::path::{Path, PathBuf};

    const KEEP: usize = 12;

    fn fnv(bytes: &[u8], mut hash: u64) -> u64 {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    pub fn entry_path(
        dir: &Path,
        source: &Path,
        fit: BackgroundFit,
        width: u32,
        height: u32,
    ) -> Option<PathBuf> {
        let meta = std::fs::metadata(source).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        h = fnv(source.as_os_str().as_encoded_bytes(), h);
        h = fnv(&meta.len().to_le_bytes(), h);
        h = fnv(&mtime.as_nanos().to_le_bytes(), h);
        h = fnv(format!("{fit:?}").as_bytes(), h);
        h = fnv(&width.to_le_bytes(), h);
        h = fnv(&height.to_le_bytes(), h);
        Some(dir.join(format!("{h:016x}-{width}x{height}.rgba")))
    }

    pub fn read(path: &Path, width: u32, height: u32) -> Option<Vec<u8>> {
        let bytes = std::fs::read(path).ok()?;
        (bytes.len() == width as usize * height as usize * 4).then_some(bytes)
    }

    pub fn write(path: &Path, bytes: &[u8]) {
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "rgba"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        files.sort();
        let excess = files.len().saturating_sub(KEEP);
        for (_, old) in files.into_iter().take(excess) {
            if old != path {
                let _ = std::fs::remove_file(old);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let d =
                std::env::temp_dir().join(format!("vigil-bgcache-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        #[test]
        fn round_trip_and_size_check() {
            let d = scratch("rt");
            let src = d.join("src.png");
            std::fs::write(&src, b"x").unwrap();
            let p = entry_path(&d.join("c"), &src, BackgroundFit::Fill, 2, 1).unwrap();
            write(&p, &[1, 2, 3, 4, 5, 6, 7, 8]);
            assert_eq!(read(&p, 2, 1).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
            assert!(read(&p, 3, 1).is_none(), "wrong size must miss");
            let _ = std::fs::remove_dir_all(&d);
        }

        #[test]
        fn key_changes_with_fit_size_and_source() {
            let d = scratch("key");
            let src = d.join("src.png");
            std::fs::write(&src, b"x").unwrap();
            let a = entry_path(&d, &src, BackgroundFit::Fill, 4, 4).unwrap();
            assert_ne!(a, entry_path(&d, &src, BackgroundFit::Fit, 4, 4).unwrap());
            assert_ne!(a, entry_path(&d, &src, BackgroundFit::Fill, 4, 5).unwrap());
            std::fs::write(&src, b"longer").unwrap();
            assert_ne!(a, entry_path(&d, &src, BackgroundFit::Fill, 4, 4).unwrap());
            assert!(entry_path(&d, &d.join("missing"), BackgroundFit::Fill, 4, 4).is_none());
            let _ = std::fs::remove_dir_all(&d);
        }

        #[test]
        fn trims_to_keep_newest() {
            let d = scratch("trim");
            for i in 0..(KEEP + 3) {
                std::thread::sleep(std::time::Duration::from_millis(15));
                write(&d.join(format!("{i:02}.rgba")), &[0]);
            }
            let n = std::fs::read_dir(&d).unwrap().count();
            assert_eq!(n, KEEP);
            assert!(d.join(format!("{:02}.rgba", KEEP + 2)).exists());
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}

#[derive(Default)]
pub struct UiSnapshot {
    pub prompt: (String, bool),
    pub info: String,
    pub error: String,
    pub busy: bool,
}

impl UiSnapshot {
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

pub fn advance_timers() {
    slint::platform::update_timers_and_animations();
}

pub fn duration_until_next_timer_update() -> Option<Duration> {
    slint::platform::duration_until_next_timer_update()
}

impl SoftwareBackend {
    fn draw_into_shadow(&mut self, view: &SceneView) -> bool {
        let (scene_w, scene_h) = view.scene_size;
        let shadow_stride = scene_w as usize;
        let native_background = self.native_background.clone();
        let full = self.full_repaint;
        if self.adapter.draw_if_needed(|renderer| {
            renderer.set_repaint_buffer_type(if full {
                RepaintBufferType::NewBuffer
            } else {
                RepaintBufferType::ReusedBuffer
            });
            let region = if let Some(background) = &native_background {
                let overlay = bytemuck::cast_slice_mut::<u8, Argb8888>(&mut self.native_overlay);
                let region = renderer.render(overlay, shadow_stride);
                if full {
                    composite_native_background(background, overlay, &mut self.shadow);
                } else {
                    for (origin, size) in region.iter() {
                        composite_native_rect(
                            background,
                            overlay,
                            &mut self.shadow,
                            shadow_stride,
                            (origin.x.max(0) as usize, origin.y.max(0) as usize),
                            (size.width as usize, size.height as usize),
                        );
                    }
                }
                region
            } else {
                let shadow_pixels = bytemuck::cast_slice_mut::<u8, Xrgb8888>(&mut self.shadow);
                renderer.render(shadow_pixels, shadow_stride)
            };
            if full {
                self.unpresented = Damage::Full;
            } else {
                for (origin, size) in region.iter() {
                    let drawn = Rect::new(
                        origin.x.max(0) as u32,
                        origin.y.max(0) as u32,
                        size.width,
                        size.height,
                    );
                    self.unpresented.add(drawn.clipped(scene_w, scene_h));
                }
            }
        }) {
            self.full_repaint = false;
            self.needs_present = true;
        }
        self.needs_present
    }
}

impl RenderBackend for SoftwareBackend {
    fn scene_needs_present(&mut self, view: &SceneView) -> bool {
        self.draw_into_shadow(view)
    }

    fn request_present(&mut self) {
        self.needs_present = true;
    }

    fn set_native_background(
        &mut self,
        pixels: std::sync::Arc<[u8]>,
        width: u32,
        height: u32,
    ) -> bool {
        if width == 0
            || height == 0
            || width as usize * height as usize * 4 != self.shadow.len()
            || pixels.len() != self.shadow.len()
        {
            return false;
        }
        self.native_background = Some(pixels);
        self.native_overlay.resize(self.shadow.len(), 0);
        self.full_repaint = true;
        self.adapter.request_redraw();
        self.needs_present = true;
        true
    }

    fn supports_native_background(&self) -> bool {
        true
    }

    fn clear_native_background(&mut self) {
        self.native_background = None;
        self.native_overlay.clear();
        self.full_repaint = true;
        self.adapter.request_redraw();
        self.needs_present = true;
    }

    fn render(&mut self, view: &SceneView, canvas: CoreCanvas<'_>) -> bool {
        let CoreCanvas::Cpu(target) = canvas else {
            eprintln!("vigil-ui: software backend given a GL canvas");
            return false;
        };
        let (scene_w, scene_h) = view.scene_size;
        let debug = std::env::var_os("VIGIL_DEBUG_FRAMES").is_some();
        if target.width != self.panel_width
            || target.height != self.panel_height
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
                    scene_w,
                    scene_h
                );
            }
            return false;
        }
        self.draw_into_shadow(view);
        if !self.needs_present {
            return false;
        }
        let layout = scanout::Layout {
            transform: self.transform,
            scene: view.scene_size,
        };
        let cursor = view
            .cursor_visible
            .then(|| scanout::cursor_rect(layout, view.scale, view.pointer));
        let mut changed = std::mem::take(&mut self.unpresented);
        for rect in self.cursor_drawn.iter().chain(&cursor) {
            changed.add(*rect);
        }
        let changed = changed.map(|rect| layout.to_panel(rect));
        let frame = vigil_core::next_frame_serial();
        let repaint = match target.damage {
            Some(damage) => damage.repaint(self.presented, frame, changed),
            None => Damage::Full,
        };
        self.presented = Some(frame);
        let shadow = bytemuck::cast_slice::<u8, u32>(&self.shadow);
        for rect in repaint.rects(self.panel_width, self.panel_height) {
            scanout::copy(shadow, layout, target.buffer, target.stride, rect);
        }
        if let Some(at) = cursor {
            scanout::blit_cursor(layout, view.scale, at, target.buffer, target.stride);
        }
        self.cursor_drawn = cursor;
        self.needs_present = false;
        true
    }
}

fn composite_native_background(background: &[u8], overlay: &[Argb8888], output: &mut [u8]) {
    composite_native_span(
        bytemuck::cast_slice::<u8, Xrgb8888>(background),
        overlay,
        bytemuck::cast_slice_mut::<u8, Xrgb8888>(output),
    );
}

fn composite_native_rect(
    background: &[u8],
    overlay: &[Argb8888],
    output: &mut [u8],
    stride: usize,
    (x, y): (usize, usize),
    (width, height): (usize, usize),
) {
    let background = bytemuck::cast_slice::<u8, Xrgb8888>(background);
    let output = bytemuck::cast_slice_mut::<u8, Xrgb8888>(output);
    if stride == 0 || x >= stride {
        return;
    }
    let width = width.min(stride - x);
    let rows = output.len() / stride;
    for row in y..(y + height).min(rows) {
        let span = row * stride + x..row * stride + x + width;
        composite_native_span(
            &background[span.clone()],
            &overlay[span.clone()],
            &mut output[span],
        );
    }
}

fn composite_native_span(background: &[Xrgb8888], overlay: &[Argb8888], output: &mut [Xrgb8888]) {
    for ((background, overlay), output) in background.iter().zip(overlay).zip(output) {
        let alpha = overlay.0 >> 24;
        output.0 = if alpha == 0 {
            background.0
        } else if alpha == 255 {
            overlay.0 & 0x00ff_ffff
        } else {
            let inverse = 255 - alpha;
            let br = (background.0 >> 16) & 0xff;
            let bg = (background.0 >> 8) & 0xff;
            let bb = background.0 & 0xff;
            let red = ((overlay.0 >> 16) & 0xff) + br * inverse / 255;
            let green = ((overlay.0 >> 8) & 0xff) + bg * inverse / 255;
            let blue = (overlay.0 & 0xff) + bb * inverse / 255;
            (red << 16) | (green << 8) | blue
        };
    }
}

#[cfg(test)]
mod tests {
    use vigil_core::scene_to_panel;

    #[test]
    fn every_transform_is_a_bijection_onto_the_panel() {
        let (sw, sh) = (7, 5);
        for transform in 0..4u8 {
            let (pw, ph) = if transform % 2 == 1 {
                (sh, sw)
            } else {
                (sw, sh)
            };
            let mut seen = vec![false; pw * ph];
            for sy in 0..sh {
                for sx in 0..sw {
                    let (px, py) = scene_to_panel(transform, sw, sh, sx, sy);
                    assert!(px < pw && py < ph, "transform {transform} left the panel");
                    let slot = &mut seen[py * pw + px];
                    assert!(!*slot, "transform {transform} maps two pixels to one");
                    *slot = true;
                }
            }
            assert!(seen.iter().all(|&x| x), "transform {transform} left holes");
        }
    }

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
            fallback_background: None,
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

    #[test]
    fn caller_background_is_the_last_fallback() {
        let mut value = looks("", None, None);
        value.fallback_background = Some("/current-wallpaper.png".into());
        assert_eq!(
            value.for_connector("DP-1"),
            (Some("/current-wallpaper.png".into()), BackgroundFit::Fill)
        );

        value.config = vigil_config::parse("[look]\nbackground = \"/configured.png\"").unwrap();
        assert_eq!(
            value.for_connector("DP-1").0,
            Some("/configured.png".into())
        );
    }

    #[test]
    fn dynamic_registry_fallback_is_replaceable_and_keeps_config_precedence() {
        let value = looks("", None, None);
        assert_eq!(
            value.for_connector_with_fallback(
                "DP-1",
                Some("/system.png".into()),
                Some(BackgroundFit::Center),
            ),
            (Some("/system.png".into()), BackgroundFit::Center)
        );
        assert_eq!(
            value.for_connector_with_fallback("DP-1", None, None),
            (None, BackgroundFit::Fill)
        );

        let configured = looks("[look]\nbackground = \"/operator.png\"", None, None);
        assert_eq!(
            configured
                .for_connector_with_fallback(
                    "DP-1",
                    Some("/system.png".into()),
                    Some(BackgroundFit::Center),
                )
                .0,
            Some("/operator.png".into())
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
        let dirty = Arc::new(DirtySet::new());
        let metrics = Arc::new(Metrics::new());
        let wake_edges = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_edges_for_callback = wake_edges.clone();
        let wake = WakeHandle::new(move || {
            wake_edges_for_callback.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        platform.set_runtime(wake.clone(), dirty.clone(), metrics.clone());
        platform.set_next_output(OutputId(1));
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
        platform.clear_next_output();
        let adapter = platform.claim_last_adapter().unwrap();
        let mut window = OutputWindow::new(OutputId(1), 2, 2, 1.0, adapter, component).unwrap();
        assert_eq!(dirty.take_all(), vec![OutputId(1)]);
        wake.acknowledge();
        let mut buffer = vec![0_u8; 16];
        assert!(window.render_if_needed(FrameTarget {
            buffer: &mut buffer,
            width: 2,
            height: 2,
            stride: 8,
            damage: None,
        }));
        assert_eq!(
            buffer,
            [
                0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0
            ]
        );
        assert!(dirty.take_all().is_empty());

        assert!(
            !window.scene_needs_present(),
            "a settled scene must not ask to be presented"
        );
        window.request_present();
        assert!(
            window.scene_needs_present(),
            "a scene asked to repaint must still report it owes a present"
        );

        assert_eq!(window.panel_size(), (2, 2));
        assert_eq!(window.scene_size(), (2, 2));
        let mut wrong = vec![0_u8; 36];
        assert!(
            !window.render_if_needed(FrameTarget {
                buffer: &mut wrong,
                width: 3,
                height: 3,
                stride: 12,
                damage: None,
            }),
            "a mismatched target must be refused, not partially painted"
        );
        assert_eq!(wrong, vec![0_u8; 36], "a refused render must not write");
        assert!(
            window.scene_needs_present(),
            "a refused render must leave the present still owed"
        );
        assert!(window.render_if_needed(FrameTarget {
            buffer: &mut buffer,
            width: 2,
            height: 2,
            stride: 8,
            damage: None,
        }));

        let source = r#"
            export component Native inherits Window {
                in property <bool> native-background: false;
                in property <bool> show-box: true;
                background: native-background ? transparent : #000000;
                Rectangle {
                    visible: show-box;
                    x: 0px; y: 0px; width: 1px; height: 1px; background: #ff0000;
                }
            }
        "#;
        let result = block_on(
            slint_interpreter::Compiler::default()
                .build_from_source(source.to_owned(), "native.slint".into()),
        );
        assert!(!result.has_errors());
        platform.set_next_output(OutputId(2));
        let component = result.component("Native").unwrap().create().unwrap();
        platform.clear_next_output();
        let adapter = platform.claim_last_adapter().unwrap();
        let mut window = OutputWindow::new(OutputId(2), 2, 2, 1.0, adapter, component).unwrap();
        assert!(window.supports_native_background());
        assert!(window.set_native_background_xrgb([0xff, 0, 0, 0].repeat(4).into(), 2, 2));
        let mut buffer = vec![0_u8; 16];
        assert!(window.render_if_needed(FrameTarget {
            buffer: &mut buffer,
            width: 2,
            height: 2,
            stride: 8,
            damage: None,
        }));
        assert_eq!(
            buffer,
            [0, 0, 0xff, 0, 0xff, 0, 0, 0, 0xff, 0, 0, 0, 0xff, 0, 0, 0]
        );
        window.set_optional_property("show-box", Value::Bool(false));
        assert_eq!(dirty.take_all(), vec![OutputId(2)]);
        assert!(wake_edges.load(std::sync::atomic::Ordering::Relaxed) >= 2);
        assert!(window.render_if_needed(FrameTarget {
            buffer: &mut buffer,
            width: 2,
            height: 2,
            stride: 8,
            damage: None,
        }));
        assert_eq!(buffer, [0xff, 0, 0, 0].repeat(4));

        let source = r#"
            export component Rotated inherits Window {
                background: #204060;
            }
        "#;
        let result = block_on(
            slint_interpreter::Compiler::default()
                .build_from_source(source.to_owned(), "rotated.slint".into()),
        );
        assert!(!result.has_errors());
        platform.set_next_output(OutputId(3));
        let component = result.component("Rotated").unwrap().create().unwrap();
        platform.clear_next_output();
        let adapter = platform.claim_last_adapter().unwrap();
        let rotated =
            OutputWindow::with_transform(OutputId(3), 4, 2, 1.0, 1, adapter, component).unwrap();
        assert_eq!(
            rotated.panel_size(),
            (4, 2),
            "panel_size is the scanout size the compositor configured"
        );
        assert_eq!(
            rotated.scene_size(),
            (2, 4),
            "a quarter turn lays the scene out transposed"
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
