//! UI subsystem (DESIGN.md §5): the custom Slint `Platform` (one full-output
//! `Window` per output — validated by spike M0b), per-output background
//! bitmaps, the software cursor as a scene element, and the AuthUi
//! implementation that binds theme contract properties.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use image::{DynamicImage, RgbaImage, imageops};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{
    Key, Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
};
use slint::{
    Color, ComponentHandle, Image, LogicalPosition, PhysicalSize, Rgba8Pixel, SharedPixelBuffer,
    WindowSize,
};
use slint_idle_runtime::{DirtySet, Metrics, RedrawHandle, WakeHandle};
use slint_interpreter::{ComponentInstance, Value};
use slint_kit::TokenSet;
use vigil_core::{
    AuthUi, BackgroundFit, CURSOR, FrameTarget, InputEvent, OutputId, PowerAction, UiMessage,
    scene_to_panel,
};
use vigil_core::{Canvas as CoreCanvas, RenderBackend, SceneView};

/// The custom Slint platform. Window adapters are created one at a time and
/// captured per output (M0b's adapter-capture pattern).
#[derive(Clone, Default)]
pub struct VigilPlatform {
    adapters: Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>,
    /// Adapter to vend for the next instantiation instead of a software one.
    ///
    /// A component is bound to whatever adapter existed when it was created,
    /// so a GL scene has to be built against its own GL window -- which only
    /// exists after that output's surface does. The binary sets this, then
    /// instantiates.
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
    /// Install as the process-wide Slint platform. Call once, before any
    /// component is instantiated.
    pub fn install() -> Result<Self, slint::platform::SetPlatformError> {
        let platform = Self::default();
        slint::platform::set_platform(Box::new(platform.clone()))?;
        Ok(platform)
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

    /// Associate the next theme instantiation with one physical output.
    pub fn set_next_output(&self, output: OutputId) {
        *self.next_output.borrow_mut() = Some(output);
    }

    pub fn clear_next_output(&self) {
        *self.next_output.borrow_mut() = None;
    }

    /// Claim the adapter created by the most recently instantiated component.
    pub fn claim_last_adapter(&self) -> Option<Rc<MinimalSoftwareWindow>> {
        self.adapters.borrow_mut().pop()
    }

    /// Vend `adapter` for components instantiated from now until
    /// [`Self::clear_adapter_override`]. Wrap exactly one instantiation.
    pub fn use_next_adapter(&self, adapter: Rc<dyn WindowAdapter>) {
        *self.next.borrow_mut() = Some(adapter);
    }

    /// Go back to vending software adapters.
    pub fn clear_adapter_override(&self) {
        *self.next.borrow_mut() = None;
    }

    /// How many software adapters have been created and not yet claimed.
    ///
    /// Compare across an instantiation to tell whether it got the override:
    /// the absolute count is not a signal, because earlier outputs leave
    /// adapters behind.
    pub fn adapters_created(&self) -> usize {
        self.adapters.borrow().len()
    }
}

impl Platform for VigilPlatform {
    fn cursor_flash_cycle(&self) -> Duration {
        // Login/lock prompts may remain focused for hours. A blinking caret
        // must not keep every fullscreen output's animation clock alive.
        Duration::ZERO
    }

    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        // A one-shot override: taken, so the next instantiation goes back to
        // the software default rather than silently reusing this adapter.
        // Sticky, not one-shot: instantiating one component asks for an
        // adapter more than once, and handing out the override only the first
        // time leaves the component bound to a software window that the GL
        // renderer then draws nothing into -- a black screen with every log
        // line reporting success.
        if let Some(adapter) = self.next.borrow().clone() {
            return Ok(adapter);
        }
        // Partial repaints into the window's persistent shadow buffer; each
        // present then copies the shadow to the (possibly alternating)
        // output buffer. Software-rendering a full 4K scene per frame is
        // what made the first on-metal run drop keystrokes.
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
        // Showing a new scene always owes its output one initial frame.
        redraw.request_redraw();
        Ok(Rc::new(TrackedSoftwareWindow {
            inner: adapter,
            redraw,
        }))
    }
}

/// One output's scene: Slint window + theme instance + per-output state.
pub struct OutputWindow {
    _id: OutputId,
    /// How this scene becomes pixels. Everything above is what the scene
    /// *is*, and is identical whichever renderer draws it.
    backend: Box<dyn RenderBackend>,
    /// Scene dimensions: what Slint renders and what every coordinate in
    /// this type is expressed in. For a rotated output these are the
    /// *rotated* dimensions, so the whole UI, pointer and cursor stay in one
    /// upright coordinate space and only the final copy-out knows better.
    width: u32,
    height: u32,
    scale: f32,
    pointer_x: f64,
    pointer_y: f64,
    cursor_visible: bool,
    /// Bumped whenever the scene changes. Slint's own redraw request fires
    /// once for a custom adapter and never re-arms, so a backend that has no
    /// partial-repaint bookkeeping of its own (GL) needs a signal it can
    /// trust. Every mutation funnels through `set_property`, so one counter
    /// there covers the lot.
    revision: std::cell::Cell<u64>,
    component: ComponentInstance,
    /// Scanout (panel) dimensions. Equal to the scene except on a
    /// quarter-turn transform, which swaps them - so this is the quantity a
    /// compositor's configure is in, and the scene is not.
    panel: (u32, u32),
}

/// The software baseline: Slint's `SoftwareRenderer` into a persistent
/// shadow buffer, copied out (rotating if the output is transformed) with
/// the cursor composited on top.
pub struct SoftwareBackend {
    /// Persistent scene buffer (tightly packed XRGB8888). Slint partial-
    /// repaints into it; presents copy it out. Keeps cursor motion at
    /// memcpy cost instead of a full software re-render.
    shadow: Vec<u8>,
    /// The next present must copy out even if the scene didn't change
    /// (cursor moved/toggled, or a fresh swapchain buffer needs filling).
    needs_present: bool,
    adapter: Rc<MinimalSoftwareWindow>,
    /// Panel (scanout) dimensions; differ from the scene on a quarter turn.
    panel_width: u32,
    panel_height: u32,
    /// wl_output/Hyprland transform: 0, 1, 2, 3 = 0, 90, 180, 270 degrees.
    transform: u8,
    /// Scanout-ready scene pixels copied below a transparent Slint overlay.
    native_background: Option<std::sync::Arc<[u8]>>,
    native_overlay: Vec<u8>,
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
        Self::with_transform(id, width, height, scale, 0, adapter, component)
    }

    /// Bind a component to a backend built elsewhere -- the GL path, which
    /// this crate cannot name without depending on it.
    ///
    /// `width`/`height` are *scene* dimensions here: a caller with its own
    /// adapter has already sized it, so there is nothing left to swap. Such
    /// a caller also owns its own transform, so [`Self::panel_size`] reports
    /// the scene size for these windows - this type is not told otherwise.
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
        Ok(Self {
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
        })
    }

    /// `width`/`height` are the panel's scanout dimensions; a quarter-turn
    /// transform swaps them to get the scene the theme is laid out in.
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
        // Size before show: the first layout happens on show, and a wrong
        // size there means a first frame laid out for the wrong output.
        adapter.set_size(PhysicalSize::new(width, height));
        component.show()?;
        Ok(Self {
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
        })
    }

    /// Whether this output draws the software cursor (the one under the
    /// pointer). The cursor is composited at present time, never rendered
    /// by Slint, so toggling or moving it costs a copy, not a re-render.
    /// Force the next present to copy out even if the scene is unchanged.
    /// After a resume the scanout buffers hold whatever survived suspend, so
    /// there is nothing to be gained by trusting them.
    pub fn request_present(&mut self) {
        self.backend.request_present();
        self.component.window().request_redraw();
    }

    /// Re-arm the copy-out without requesting a redraw.
    ///
    /// The difference from [`Self::request_present`] is the missing
    /// `request_redraw`: this says "the buffer is stale", not "the scene is
    /// stale". A settled scene therefore stays settled and the next render
    /// copies the shadow out as it stands.
    ///
    /// Two callers, both of which want exactly that. A presenter retrying
    /// after a device error, on a bounded deadline, must not wake the loop
    /// to do it. And the warn→lock handoff (vigil#86) arms this from inside
    /// the lock surface's configure callback, where the retained scene is
    /// already the picture on screen and asking Slint to draw would be the
    /// scene work vigil#37 forbids there.
    pub fn request_present_deferred(&mut self) {
        self.backend.request_present();
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        if self.cursor_visible != visible {
            self.cursor_visible = visible;
            self.request_present();
        }
    }

    /// Set the pre-fit background bitmap (from `background` below).
    pub fn set_background(&mut self, rgba: Vec<u8>, width: u32, height: u32) {
        self.set_background_pixels(&rgba, width, height);
    }

    /// Set a pre-fit background without requiring ownership of its backing
    /// cache entry. Slint takes its own pixel-buffer copy here, so workers can
    /// share one rendered bitmap between equal-sized outputs.
    pub fn set_background_pixels(&mut self, rgba: &[u8], width: u32, height: u32) {
        if rgba.len() != width as usize * height as usize * 4 {
            return;
        }
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba, width, height);
        self.set_property("background-source", Value::Image(Image::from_rgba8(buffer)));
    }

    /// Use a prepared XRGB frame as the renderer's immutable base layer.
    /// Themes predating the optional `native-background` property stay on
    /// the image path so custom themes remain compatible.
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

    /// Remove a previously rendered bitmap and reveal the theme background.
    pub fn clear_background(&mut self) {
        self.backend.clear_native_background();
        let _ = self
            .component
            .set_property("native-background", Value::Bool(false));
        self.set_property("background-source", Value::Image(Image::default()));
    }

    /// Whether this output hosts the login panel (theme `panel-visible`).
    pub fn set_panel_visible(&mut self, visible: bool) {
        self.set_property("panel-visible", Value::Bool(visible));
    }

    /// Whether the theme's host power actions are visible.
    pub fn set_power_visible(&mut self, visible: bool) {
        self.set_optional_property("power-visible", Value::Bool(visible));
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

    /// Route a normalized input event into this window.
    pub fn dispatch(&mut self, event: InputEvent) {
        // Keys and buttons change what the scene shows through paths
        // Slint's one-shot redraw request cannot be trusted to re-arm for,
        // so they bump the revision. Pointer motion does not: its only
        // scene-visible effect (hover) does re-arm via the adapter's
        // request_redraw, and a per-motion revision bump would make a
        // hardware-cursor GL output re-render the scene for every pixel
        // the pointer travels (#25).
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

    /// Scene dimensions — what the theme is laid out in, and what pointer
    /// coordinates and backgrounds for this output must be sized to. Differs
    /// from the panel's scanout size on a quarter-turn transform.
    pub fn scene_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Scanout dimensions — what a compositor's configure and every
    /// [`FrameTarget`] for this output are expressed in. Same as
    /// [`Self::scene_size`] except on a quarter turn, which swaps them.
    ///
    /// Compare a configure against this one, never against the scene: they
    /// agree only while the transform is 0, so a comparison written against
    /// the scene is correct by coincidence and silently wrong the day an
    /// output is rotated.
    pub fn panel_size(&self) -> (u32, u32) {
        self.panel
    }

    /// Current pointer position in scene pixels.
    pub fn pointer(&self) -> (f64, f64) {
        (self.pointer_x, self.pointer_y)
    }

    /// A description of the scene for the backend to draw.
    fn view(&self) -> SceneView {
        SceneView {
            scene_size: (self.width, self.height),
            scale: self.scale,
            pointer: (self.pointer_x, self.pointer_y),
            cursor_visible: self.cursor_visible,
            revision: self.revision.get(),
        }
    }

    /// Draw into whatever canvas the presenter handed out; returns whether
    /// anything was drawn. The one render entry point both paths share.
    pub fn render(&mut self, canvas: CoreCanvas<'_>) -> bool {
        let view = self.view();
        self.backend.render(&view, canvas)
    }

    /// Render into a CPU target if dirty; returns whether pixels changed.
    pub fn render_if_needed(&mut self, target: FrameTarget<'_>) -> bool {
        self.render(CoreCanvas::Cpu(target))
    }

    /// Whether this window owes a present, without needing a buffer to
    /// answer. Draws the scene into the backend's shadow, so a following
    /// `render_if_needed` only copies out. See `RenderBackend::
    /// scene_needs_present` for why a presenter must be able to ask.
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
        self.set_optional_property("user-name", Value::String(name.into()));
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
        self.set_optional_property(
            "users",
            Value::Model(slint::ModelRc::new(slint::VecModel::from(model))),
        );
    }

    /// Optional theme property `selected-user` (contract v2).
    pub fn set_user_index(&mut self, index: usize) {
        self.set_optional_property("selected-user", Value::Number(index as f64));
    }

    /// Optional theme property `color-scheme` ("dark" | "light" | "").
    pub fn set_color_scheme(&mut self, scheme: &str) {
        self.set_optional_property("color-scheme", Value::String(scheme.into()));
    }

    /// Optional theme property `accent-color`. Portal sRGB is already
    /// [0,1] floats, which is exactly what `from_rgb_f32` wants.
    pub fn set_accent_color(&mut self, rgb: (f32, f32, f32)) {
        self.set_optional_property(
            "accent-color",
            Color::from_rgb_f32(rgb.0, rgb.1, rgb.2).into(),
        );
    }

    /// Paint slint-kit `Theme` (if the theme exports it) plus contract
    /// `color-scheme` / `accent-color` from an LMTT [`TokenSet`].
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
        self.touch();
    }

    fn set_property(&self, name: &str, value: Value) {
        let result = self.component.set_property(name, value);
        debug_assert!(result.is_ok());
        self.touch();
    }

    /// As [`Self::set_property`] but for optional contract properties a theme
    /// may legitimately not declare.
    fn set_optional_property(&self, name: &str, value: Value) {
        let _ = self.component.set_property(name, value);
        self.touch();
    }

    /// Mark the scene changed.
    fn touch(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        self.component.window().request_redraw();
    }

    pub fn slint_window(&self) -> &slint::Window {
        self.component.window()
    }
}

/// Load LMTT tokens through slint-kit / lmtt-core. For vigil-lock.
/// The greeter must not call this.
pub fn apply_kit_tokens_from_disk(window: &mut OutputWindow, mode: &str) {
    window.apply_kit_tokens(&slint_kit::load_tokens_preferring(mode));
}

/// Load LMTT tokens from system/packaged/embedded layers. No user tree.
pub fn apply_kit_tokens_from_system(window: &mut OutputWindow, mode: &str) {
    window.apply_kit_tokens(&slint_kit::load_tokens_system(mode));
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

/// Premultiplied ARGB8888 overlay pixel (little-endian B, G, R, A).
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
/// CLI > `[output."NAME"]` override > `[look]` > caller fallback > default. An override keys
/// on the exact connector name; with two GPUs exposing the same name
/// (e.g. two DP-1s) the override applies to every one of them.
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

    /// Resolve application overrides above a dynamic registry fallback.
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
    let source = load_background(path)?;
    render_background(&source, fit, out_w, out_h)
}

/// A source image decoded once and reusable for several output sizes.
pub struct BackgroundImage(DynamicImage);

pub fn load_background(path: &Path) -> Result<BackgroundImage, String> {
    image::open(path)
        .map(BackgroundImage)
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
    Ok(fit_background(&source.0, fit, out_w, out_h).into_raw())
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

impl SoftwareBackend {
    /// Partial-repaint the scene into the persistent shadow and report
    /// whether a present is owed.
    ///
    /// This needs no target, which is the whole point: a presenter can ask
    /// "is there anything to show?" without first acquiring a `wl_buffer`.
    /// Acquiring one and dropping it un-attached on a clean scene is what
    /// span the locked-idle loop -- the compositor answers every
    /// `wl_buffer.destroy` with `delete_id`, which wakes the event loop,
    /// which churns another buffer (#65).
    ///
    /// Dirtiness cannot be predicted without doing this: Slint sets it
    /// *during* the render (`draw_if_needed` reports whether it drew), and
    /// core calls `request_redraw` on the inner window rather than any
    /// wrapper we could observe -- which is why the DirtySet is only
    /// advisory.
    fn draw_into_shadow(&mut self, view: &SceneView) -> bool {
        // Slint partial-repaints into the persistent shadow (ReusedBuffer
        // contract: same buffer, contents preserved between renders).
        let shadow_stride = view.scene_size.0 as usize;
        let native_background = self.native_background.clone();
        if self.adapter.draw_if_needed(|renderer| {
            if let Some(background) = &native_background {
                renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
                let overlay = bytemuck::cast_slice_mut::<u8, Argb8888>(&mut self.native_overlay);
                renderer.render(overlay, shadow_stride);
                composite_native_background(background, overlay, &mut self.shadow);
            } else {
                renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
                let shadow_pixels = bytemuck::cast_slice_mut::<u8, Xrgb8888>(&mut self.shadow);
                renderer.render(shadow_pixels, shadow_stride);
            }
        }) {
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
        self.needs_present = true;
        true
    }

    fn supports_native_background(&self) -> bool {
        true
    }

    fn clear_native_background(&mut self) {
        self.native_background = None;
        self.native_overlay.clear();
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
        // Copy out (the target may be an alternating swapchain buffer with a
        // wider stride), then composite the cursor on top — the shadow itself
        // never contains it.
        if self.transform == 0 {
            let row_bytes = scene_w as usize * 4;
            for y in 0..scene_h as usize {
                target.buffer[y * target.stride..y * target.stride + row_bytes]
                    .copy_from_slice(&self.shadow[y * row_bytes..(y + 1) * row_bytes]);
            }
        } else {
            let Ok(pixels) = bytemuck::try_cast_slice_mut::<u8, Xrgb8888>(target.buffer) else {
                if debug {
                    eprintln!("vigil-ui: buffer not 4-byte aligned");
                }
                return true;
            };
            self.rotate_out(view.scene_size, pixels, target.stride / 4);
        }
        if view.cursor_visible {
            let Ok(pixels) = bytemuck::try_cast_slice_mut::<u8, Xrgb8888>(target.buffer) else {
                if debug {
                    eprintln!("vigil-ui: buffer not 4-byte aligned");
                }
                return true;
            };
            self.blit_cursor(view, pixels, target.stride / 4);
        }
        self.needs_present = false;
        true
    }
}

fn composite_native_background(background: &[u8], overlay: &[Argb8888], output: &mut [u8]) {
    let background = bytemuck::cast_slice::<u8, Xrgb8888>(background);
    let output = bytemuck::cast_slice_mut::<u8, Xrgb8888>(output);
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

impl SoftwareBackend {
    /// Scene pixel -> panel pixel for this output's transform.
    ///
    /// Convention (wl_output / Hyprland): the transform names how the panel
    /// is mounted, so the content is rotated the opposite way to come out
    /// upright. `1` (90) rotates the scene a quarter turn *counter-clockwise*
    /// into the panel, `3` (270) clockwise — pinned on metal against Hyprland.
    #[inline]
    fn to_panel(&self, scene: (u32, u32), sx: usize, sy: usize) -> (usize, usize) {
        scene_to_panel(self.transform, scene.0 as usize, scene.1 as usize, sx, sy)
    }

    /// Rotate the shadow into the scanout buffer.
    ///
    /// Tiled rather than scanline: a quarter turn writes down a column for
    /// every row it reads, so the naive loop misses cache on nearly every
    /// pixel. On a 4K panel that is 8.3M scattered writes per present, and
    /// per-frame work on that scale is precisely what starved the event loop
    /// and dropped keystrokes once already (see the shadow buffer above).
    ///
    /// Measured on the reference machine, rotating a 2160x3840 scene into a
    /// 3840x2160 panel: naive 15.8 ms/frame, tiled 7.5 ms. For scale, the
    /// unrotated row memcpy is 2.2 ms. A tile of 16 pixels is one 64-byte
    /// cache line of XRGB8888 and measured fastest; 8 and 32 both lose ~1 ms.
    fn rotate_out(&self, scene: (u32, u32), target: &mut [Xrgb8888], target_stride: usize) {
        const TILE: usize = 16;
        let (sw, sh) = (scene.0 as usize, scene.1 as usize);
        let shadow = bytemuck::cast_slice::<u8, Xrgb8888>(&self.shadow);
        for tile_y in (0..sh).step_by(TILE) {
            let y_end = (tile_y + TILE).min(sh);
            for tile_x in (0..sw).step_by(TILE) {
                let x_end = (tile_x + TILE).min(sw);
                for sy in tile_y..y_end {
                    let row = &shadow[sy * sw + tile_x..sy * sw + x_end];
                    for (sx, pixel) in (tile_x..x_end).zip(row) {
                        let (px, py) = self.to_panel(scene, sx, sy);
                        target[py * target_stride + px] = *pixel;
                    }
                }
            }
        }
    }

    /// Overlay the software cursor into the just-rendered frame, scaled to
    /// the output's HiDPI factor (nearest neighbor — it is a pointer).
    fn blit_cursor(&self, view: &SceneView, pixels: &mut [Xrgb8888], pixel_stride: usize) {
        let scale = f64::from(view.scale.max(1.0));
        let out_w = (CURSOR[0].len() as f64 * scale) as usize;
        let out_h = (CURSOR.len() as f64 * scale) as usize;
        let (base_x, base_y) = (view.pointer.0 as usize, view.pointer.1 as usize);
        for oy in 0..out_h {
            let py = base_y + oy;
            if py >= view.scene_size.1 as usize {
                break;
            }
            let row = CURSOR[((oy as f64 / scale) as usize).min(CURSOR.len() - 1)];
            for ox in 0..out_w {
                let px = base_x + ox;
                if px >= view.scene_size.0 as usize {
                    break;
                }
                // The cursor is composited after the rotation, so its scene
                // coordinates have to make the same trip the frame just did
                // — otherwise the pointer sits at right angles to the UI it
                // is pointing at.
                let (tx, ty) = self.to_panel(view.scene_size, px, py);
                match row[((ox as f64 / scale) as usize).min(row.len() - 1)] {
                    b'X' => pixels[ty * pixel_stride + tx] = Xrgb8888(0),
                    b'#' => pixels[ty * pixel_stride + tx] = Xrgb8888(0x00ff_ffff),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scene_to_panel;

    // The corner-pinning rotation tests live in vigil-core next to
    // scene_to_panel itself; the bijection property stays here with the
    // renderer that relies on it.
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
        }));
        assert_eq!(
            buffer,
            [
                0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0, 0x60, 0x40, 0x20, 0
            ]
        );
        // A static visible frame does not re-arm itself.
        assert!(dirty.take_all().is_empty());

        // ...and reports that it owes nothing, WITHOUT a target. A presenter
        // must be able to ask before acquiring a buffer: acquiring one and
        // dropping it un-attached on a clean scene makes the compositor
        // reply delete_id, which wakes the loop, which acquires another
        // (#65). A probe that just says "true" restores that loop.
        assert!(
            !window.scene_needs_present(),
            "a settled scene must not ask to be presented"
        );
        window.request_present();
        assert!(
            window.scene_needs_present(),
            "a scene asked to repaint must still report it owes a present"
        );

        // A target that disagrees with the panel renders NOTHING, and asking
        // again never helps — the window has to be rebuilt at the new size.
        // This is why a warning→lock rebind at a changed pixel size is not a
        // rebind: vigil-lock's `rebound_needs_resize` exists because keeping
        // the retained window here would leave that output black for the
        // whole locked session (issue #40 geometry, issue #86 handoff). The
        // integration itself only shows up on a compositor that configures
        // the lock surface differently from the layer surface, so the seam
        // is asserted from both sides instead.
        //
        // The quantity being compared is the PANEL size, which is what a
        // configure carries and what a FrameTarget is measured in. It
        // coincides with the scene here because this window is untransformed;
        // the rotated case below pins that they are different quantities.
        assert_eq!(window.panel_size(), (2, 2));
        assert_eq!(window.scene_size(), (2, 2));
        let mut wrong = vec![0_u8; 36];
        assert!(
            !window.render_if_needed(FrameTarget {
                buffer: &mut wrong,
                width: 3,
                height: 3,
                stride: 12,
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
        }));
        assert_eq!(buffer, [0xff, 0, 0, 0].repeat(4));

        // A quarter turn separates the panel from the scene, and a
        // FrameTarget follows the panel. vigil-lock compares a compositor's
        // configure against `panel_size` for exactly this reason: written
        // against `scene_size` it is correct only until an output is
        // rotated, and would then call every unchanged configure a resize
        // and rebuild the scene on every rebind. Last in this test because
        // instantiating a scene arms its output's dirty flag, and the
        // assertions above read the dirty set.
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
