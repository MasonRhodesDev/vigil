//! Safe, windowed development harness for Vigil.
//!
//! This binary deliberately has no dependency on PAM, greetd, logind, DRM,
//! libinput, or the Wayland session-lock implementation. UI actions append to
//! an in-memory trace and can never affect the host session.

use std::cell::RefCell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use font8x8::{BASIC_FONTS, UnicodeFonts};
use softbuffer::{Context, Surface};
use vigil_core::{AuthUi, FrameTarget, InputEvent, OutputId};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const CONTROL_WIDTH: u32 = 420;
const CONTROL_HEIGHT: u32 = 640;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Login,
    Lock,
    Warning,
}

impl Mode {
    fn parse() -> Self {
        match std::env::args().nth(1).as_deref() {
            Some("login") | None => Self::Login,
            Some("lock") => Self::Lock,
            Some("warning") => Self::Warning,
            Some(other) => {
                eprintln!("usage: vigil-sim [login|lock|warning] (got {other:?})");
                std::process::exit(2);
            }
        }
    }
}

struct Simulator {
    mode: Mode,
    platform: VigilPlatform,
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    control_window: Option<Arc<Window>>,
    control_context: Option<Context<Arc<Window>>>,
    control_surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    control_pointer: PhysicalPosition<f64>,
    scene: Option<OutputWindow>,
    pixels: Vec<u8>,
    trace: Rc<RefCell<Vec<String>>>,
    started: Instant,
}

impl Simulator {
    fn new(mode: Mode, platform: VigilPlatform) -> Self {
        Self {
            mode,
            platform,
            window: None,
            context: None,
            surface: None,
            control_window: None,
            control_context: None,
            control_surface: None,
            control_pointer: PhysicalPosition::new(0.0, 0.0),
            scene: None,
            pixels: vec![0; (WIDTH * HEIGHT * 4) as usize],
            trace: Rc::new(RefCell::new(Vec::new())),
            started: Instant::now(),
        }
    }

    fn create_scene(&mut self) {
        let theme = Theme::load_or_default(None);
        let component = theme.instantiate().expect("instantiate embedded theme");
        let adapter = self.platform.claim_last_adapter().expect("theme adapter");
        let mut scene = OutputWindow::new(OutputId(1), WIDTH, HEIGHT, 1.0, adapter, component)
            .expect("create simulated output");
        scene.set_cursor_visible(true);
        scene.set_clock("13:37");
        scene.set_user_name("mason");
        vigil_ui::apply_kit_tokens_from_disk(&mut scene, "dark");
        match self.mode {
            Mode::Login => {
                scene.set_panel_visible(true);
                scene.set_power_visible(true);
                scene.set_users(&["mason".into(), "Manual entry".into()]);
                scene.set_user_index(0);
                scene.set_sessions(&["Hyprland".into(), "Test session".into()]);
                scene.set_session_index(0);
                scene.show_prompt("Password:", true);
            }
            Mode::Lock => {
                scene.set_panel_visible(true);
                scene.set_power_visible(true);
                scene.set_users(&["mason".into()]);
                scene.set_sessions(&[]);
                scene.show_prompt("Password:", true);
                scene.set_status_banner("SIMULATED LOCK — host session unaffected");
            }
            Mode::Warning => {
                scene.set_panel_visible(false);
                scene.set_power_visible(false);
                scene.set_users(&[]);
                scene.set_sessions(&[]);
                scene.show_info("SIMULATED WARNING — press any key to cancel");
                scene.set_status_banner("Frost stage (tint-only preview)");
            }
        }
        let trace = self.trace.clone();
        scene.on_ui_message(Rc::new(move |message| {
            trace.borrow_mut().push(format!("ui: {message:?}"));
        }));
        self.scene = Some(scene);
        if let Some(window) = self.window.as_ref() {
            window.set_title(&format!(
                "Vigil Simulation — {:?} — HOST SESSION UNAFFECTED",
                self.mode
            ));
            window.request_redraw();
        }
    }

    fn dispatch(&mut self, event: InputEvent) {
        self.trace.borrow_mut().push(format!("input: {event:?}"));
        if let Some(scene) = self.scene.as_mut() {
            scene.dispatch(event);
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn pointer_position(&self, position: PhysicalPosition<f64>) -> (f64, f64) {
        let Some(window) = self.window.as_ref() else {
            return (position.x, position.y);
        };
        let size = window.inner_size();
        let x = position.x * f64::from(WIDTH) / f64::from(size.width.max(1));
        let y = position.y * f64::from(HEIGHT) / f64::from(size.height.max(1));
        (x, y)
    }

    fn render(&mut self) {
        let Some(scene) = self.scene.as_mut() else {
            return;
        };
        vigil_ui::advance_timers();
        let drew = scene.render_if_needed(FrameTarget {
            buffer: &mut self.pixels,
            width: WIDTH,
            height: HEIGHT,
            stride: (WIDTH * 4) as usize,
        });
        if !drew && self.started.elapsed().as_millis() > 100 {
            return;
        }
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        surface
            .resize(
                NonZeroU32::new(WIDTH).unwrap(),
                NonZeroU32::new(HEIGHT).unwrap(),
            )
            .expect("resize surface");
        let mut buffer = surface.buffer_mut().expect("acquire window buffer");
        for (out, px) in buffer.iter_mut().zip(self.pixels.chunks_exact(4)) {
            *out = u32::from_le_bytes([px[0], px[1], px[2], 0]);
        }
        buffer.present().expect("present simulated output");
    }

    fn select_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.trace.borrow_mut().push(format!("state: {mode:?}"));
        self.create_scene();
        if let Some(window) = self.control_window.as_ref() {
            window.request_redraw();
        }
    }

    fn render_controls(&mut self) {
        let mut pixels = vec![0x00110e0c_u32; (CONTROL_WIDTH * CONTROL_HEIGHT) as usize];
        draw_text(&mut pixels, 24, 22, "VIGIL SIMULATION", 0x00ffb77a, 2);
        draw_text(
            &mut pixels,
            24,
            52,
            "HOST SESSION UNAFFECTED",
            0x00b7ada6,
            1,
        );
        draw_text(&mut pixels, 24, 88, "STATE", 0x00f4eee9, 1);
        let modes = [
            (Mode::Login, "LOGIN"),
            (Mode::Lock, "LOCK"),
            (Mode::Warning, "IDLE WARNING"),
        ];
        for (idx, (mode, label)) in modes.iter().enumerate() {
            let y = 112 + idx as u32 * 54;
            let color = if *mode == self.mode {
                0x00a95f2c
            } else {
                0x00352b26
            };
            fill_rect(&mut pixels, 24, y, 372, 42, color);
            draw_text(&mut pixels, 40, y + 13, label, 0x00fff5ee, 1);
        }
        draw_text(&mut pixels, 24, 292, "WARNING CONTROLS", 0x00f4eee9, 1);
        for (idx, label) in [
            "RESTART TIMELINE",
            "PAUSE / RESUME",
            "+ 1 SECOND",
            "COMMIT NOW",
            "CANCEL INPUT",
            "HOTPLUG",
            "TOGGLE BLUR",
        ]
        .iter()
        .enumerate()
        {
            let y = 316 + idx as u32 * 42;
            fill_rect(&mut pixels, 24, y, 372, 32, 0x00251f1c);
            draw_text(&mut pixels, 40, y + 9, label, 0x00ddd3cc, 1);
        }
        let Some(surface) = self.control_surface.as_mut() else {
            return;
        };
        surface
            .resize(
                NonZeroU32::new(CONTROL_WIDTH).unwrap(),
                NonZeroU32::new(CONTROL_HEIGHT).unwrap(),
            )
            .expect("resize controls");
        let mut buffer = surface.buffer_mut().expect("control buffer");
        buffer.copy_from_slice(&pixels);
        buffer.present().expect("present controls");
    }

    fn control_click(&mut self) {
        let y = self.control_pointer.y.max(0.0) as u32;
        if (112..154).contains(&y) {
            self.select_mode(Mode::Login);
        } else if (166..208).contains(&y) {
            self.select_mode(Mode::Lock);
        } else if (220..262).contains(&y) {
            self.select_mode(Mode::Warning);
        } else if (316..348).contains(&y) {
            self.select_mode(Mode::Warning);
            self.trace.borrow_mut().push("warning: restart".into());
        } else if (358..390).contains(&y) {
            self.trace.borrow_mut().push("warning: pause/resume".into());
        } else if (400..432).contains(&y) {
            self.trace.borrow_mut().push("warning: advance 1s".into());
        } else if (442..474).contains(&y) {
            self.trace.borrow_mut().push("warning: commit".into());
        } else if (484..516).contains(&y) {
            self.trace.borrow_mut().push("warning: cancel".into());
        } else if (526..558).contains(&y) {
            self.trace.borrow_mut().push("warning: hotplug".into());
        } else if (568..600).contains(&y) {
            self.trace.borrow_mut().push("warning: toggle blur".into());
        }
    }
}

impl ApplicationHandler for Simulator {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let title = format!(
            "Vigil Simulation — {:?} — HOST SESSION UNAFFECTED",
            self.mode
        );
        let attributes = WindowAttributes::default()
            .with_title(title)
            .with_resizable(false)
            .with_inner_size(PhysicalSize::new(WIDTH, HEIGHT));
        let window = Arc::new(event_loop.create_window(attributes).expect("create window"));
        let context = Context::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&context, window.clone()).expect("softbuffer surface");
        self.window = Some(window.clone());
        self.context = Some(context);
        self.surface = Some(surface);
        let control = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("Vigil Simulation Controls")
                        .with_resizable(false)
                        .with_inner_size(PhysicalSize::new(CONTROL_WIDTH, CONTROL_HEIGHT)),
                )
                .expect("create controls"),
        );
        let control_context = Context::new(control.clone()).expect("control context");
        let control_surface =
            Surface::new(&control_context, control.clone()).expect("control surface");
        self.control_window = Some(control.clone());
        self.control_context = Some(control_context);
        self.control_surface = Some(control_surface);
        self.create_scene();
        window.request_redraw();
        control.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let is_control = self
            .control_window
            .as_ref()
            .is_some_and(|w| w.id() == window_id);
        if is_control {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::RedrawRequested => self.render_controls(),
                WindowEvent::CursorMoved { position, .. } => self.control_pointer = position,
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => self.control_click(),
                _ => {}
            }
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.pointer_position(position);
                self.dispatch(InputEvent::PointerAbsolute { x, y });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    MouseButton::Back => 0x116,
                    MouseButton::Forward => 0x115,
                    MouseButton::Other(value) => u32::from(value),
                };
                self.dispatch(InputEvent::PointerButton {
                    button,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let (keysym, utf8) = key_event(&event.logical_key);
                self.dispatch(InputEvent::Key {
                    keysym,
                    utf8,
                    pressed,
                });
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        vigil_ui::advance_timers();
        if let Some(delay) = vigil_ui::duration_until_next_timer_update() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + delay));
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

fn fill_rect(pixels: &mut [u32], x: u32, y: u32, width: u32, height: u32, color: u32) {
    for row in y..(y + height).min(CONTROL_HEIGHT) {
        let start = (row * CONTROL_WIDTH + x) as usize;
        let end = (row * CONTROL_WIDTH + (x + width).min(CONTROL_WIDTH)) as usize;
        pixels[start..end].fill(color);
    }
}

fn draw_text(pixels: &mut [u32], x: u32, y: u32, text: &str, color: u32, scale: u32) {
    let mut pen_x = x;
    for ch in text.chars() {
        if let Some(glyph) = BASIC_FONTS.get(ch) {
            for (gy, bits) in glyph.iter().enumerate() {
                for gx in 0..8 {
                    if bits & (1 << gx) != 0 {
                        fill_rect(
                            pixels,
                            pen_x + gx * scale,
                            y + gy as u32 * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
        }
        pen_x += 9 * scale;
    }
}

fn key_event(key: &Key) -> (u32, Option<String>) {
    match key {
        Key::Character(text) => {
            let text = text.to_string();
            let keysym = text.chars().next().map_or(0, u32::from);
            (keysym, Some(text))
        }
        Key::Named(NamedKey::Enter) => (0xff0d, Some("\r".into())),
        Key::Named(NamedKey::Tab) => (0xff09, Some("\t".into())),
        Key::Named(NamedKey::Backspace) => (0xff08, None),
        Key::Named(NamedKey::Escape) => (0xff1b, None),
        Key::Named(NamedKey::ArrowLeft) => (0xff51, None),
        Key::Named(NamedKey::ArrowUp) => (0xff52, None),
        Key::Named(NamedKey::ArrowRight) => (0xff53, None),
        Key::Named(NamedKey::ArrowDown) => (0xff54, None),
        _ => (0, None),
    }
}

fn main() {
    let mode = Mode::parse();
    let platform = VigilPlatform::install().expect("install Vigil Slint platform");
    let event_loop = EventLoop::new().expect("create event loop");
    let mut simulator = Simulator::new(mode, platform);
    event_loop.run_app(&mut simulator).expect("run simulator");
}
