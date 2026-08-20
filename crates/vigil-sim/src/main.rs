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
        scene.set_panel_visible(true);
        scene.set_clock("13:37");
        scene.set_users(&["mason".into(), "Manual entry".into()]);
        scene.set_user_index(0);
        scene.set_sessions(&["Hyprland".into(), "Test session".into()]);
        scene.set_session_index(0);
        scene.set_user_name("mason");
        vigil_ui::apply_kit_tokens_from_disk(&mut scene, "dark");
        match self.mode {
            Mode::Login => scene.show_prompt("Password:", true),
            Mode::Lock => {
                scene.show_prompt("Password:", true);
                scene.set_status_banner("SIMULATED LOCK — host session unaffected");
            }
            Mode::Warning => {
                scene.show_info("SIMULATED WARNING — press any key to cancel");
                scene.set_status_banner("Frost stage (tint-only preview)");
            }
        }
        let trace = self.trace.clone();
        scene.on_ui_message(Rc::new(move |message| {
            trace.borrow_mut().push(format!("ui: {message:?}"));
        }));
        self.scene = Some(scene);
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
        self.create_scene();
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
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
