//! Safe, windowed development harness for Vigil.
//!
//! This binary deliberately has no dependency on PAM, greetd, logind, DRM,
//! libinput, or the Wayland session-lock implementation. UI actions append to
//! an in-memory trace and can never affect the host session.

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use softbuffer::{Context, Surface};
use vigil_core::{AuthUi, FrameTarget, InputEvent, OutputId};
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};
use vigil_warning::Phase;
use vigil_warning::Timeline;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const DRAWER_WIDTH: u32 = 420;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Login,
    Lock,
    Warning,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "login" => Some(Self::Login),
            "lock" => Some(Self::Lock),
            "warning" => Some(Self::Warning),
            _ => None,
        }
    }
}

struct SimArgs {
    mode: Mode,
    paused: bool,
    at: Duration,
    state_file: Option<std::path::PathBuf>,
    control_socket: Option<std::path::PathBuf>,
}

impl SimArgs {
    fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let mut parsed = Self {
            mode: Mode::Login,
            paused: false,
            at: Duration::ZERO,
            state_file: None,
            control_socket: None,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "login" | "lock" | "warning" => parsed.mode = Mode::parse(&arg).unwrap(),
                "--paused" => parsed.paused = true,
                "--at-ms" => {
                    let value = args
                        .next()
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or_else(|| usage("--at-ms requires an integer"));
                    parsed.at = Duration::from_millis(value);
                }
                "--state-file" => {
                    parsed.state_file = Some(
                        args.next()
                            .map(Into::into)
                            .unwrap_or_else(|| usage("--state-file requires a path")),
                    )
                }
                "--control-socket" => {
                    parsed.control_socket = Some(
                        args.next()
                            .map(Into::into)
                            .unwrap_or_else(|| usage("--control-socket requires a path")),
                    );
                }
                "-h" | "--help" => usage(""),
                _ => usage(&format!("unknown argument {arg:?}")),
            }
        }
        if parsed.at != Duration::ZERO {
            parsed.paused = true;
        }
        parsed
    }
}

fn usage(error: &str) -> ! {
    if !error.is_empty() {
        eprintln!("error: {error}");
    }
    eprintln!(
        "usage: vigil-sim [login|lock|warning] [--paused] [--at-ms MS] [--state-file PATH] [--control-socket PATH]"
    );
    std::process::exit(if error.is_empty() { 0 } else { 2 });
}

struct Simulator {
    mode: Mode,
    platform: VigilPlatform,
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    /// Pointer position in the simulator's fixed 1280x800 canvas.
    pointer: PhysicalPosition<f64>,
    drawer_open: bool,
    scene: Option<OutputWindow>,
    pixels: Vec<u8>,
    trace: Rc<RefCell<Vec<String>>>,
    started: Instant,
    last_tick: Instant,
    sim_now: Duration,
    warning: Option<Timeline>,
    paused: bool,
    blur_enabled: bool,
    desktop_sharp: Arc<Vec<u8>>,
    desktop_blurred: Arc<Vec<u8>>,
    lock_wallpaper: Arc<Vec<u8>>,
    warning_wait: Option<Duration>,
    warning_phase: Option<Phase>,
    state_file: Option<std::path::PathBuf>,
    accept_warning_input: bool,
    warning_pointer_origin: Option<PhysicalPosition<f64>>,
    warning_input_after: Instant,
    last_state: RefCell<String>,
    warning_visual_key: Option<(u128, bool)>,
    pending_acks: Vec<std::sync::mpsc::SyncSender<Result<(), String>>>,
}

#[derive(Debug)]
struct UserCommand(String, std::sync::mpsc::SyncSender<Result<(), String>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlCommand {
    State(Mode),
    LockWait,
    Pause,
    Resume,
    Advance(u64),
    Commit,
    Cancel,
    Hotplug,
    Blur(Option<bool>),
    Drawer(Option<bool>),
}

fn parse_control_command(command: &str) -> Result<ControlCommand, String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        ["state", mode] => Mode::parse(mode)
            .map(ControlCommand::State)
            .ok_or_else(|| format!("unknown state {mode:?}")),
        ["lock", "--wait"] => Ok(ControlCommand::LockWait),
        ["pause"] => Ok(ControlCommand::Pause),
        ["resume"] => Ok(ControlCommand::Resume),
        ["advance", ms] => ms
            .parse::<u64>()
            .map(ControlCommand::Advance)
            .map_err(|_| format!("invalid millisecond value {ms:?}")),
        ["commit"] => Ok(ControlCommand::Commit),
        ["cancel"] => Ok(ControlCommand::Cancel),
        ["hotplug"] => Ok(ControlCommand::Hotplug),
        ["blur", "on"] => Ok(ControlCommand::Blur(Some(true))),
        ["blur", "off"] => Ok(ControlCommand::Blur(Some(false))),
        ["blur", "toggle"] => Ok(ControlCommand::Blur(None)),
        ["drawer", "open"] => Ok(ControlCommand::Drawer(Some(true))),
        ["drawer", "close"] => Ok(ControlCommand::Drawer(Some(false))),
        ["drawer", "toggle"] => Ok(ControlCommand::Drawer(None)),
        [] => Err("empty command".into()),
        _ => Err(format!("unknown command {command:?}")),
    }
}

impl Simulator {
    fn new(
        mode: Mode,
        platform: VigilPlatform,
        paused: bool,
        at: Duration,
        state_file: Option<std::path::PathBuf>,
    ) -> Self {
        let desktop_sharp = Arc::new(fake_desktop());
        let desktop_blurred = Arc::new(blur_rgba(&desktop_sharp, WIDTH, HEIGHT));
        let lock_wallpaper = Arc::new(fake_lock_wallpaper());
        Self {
            mode,
            platform,
            window: None,
            context: None,
            surface: None,
            pointer: PhysicalPosition::new(0.0, 0.0),
            drawer_open: false,
            scene: None,
            pixels: vec![0; (WIDTH * HEIGHT * 4) as usize],
            trace: Rc::new(RefCell::new(Vec::new())),
            started: Instant::now(),
            last_tick: Instant::now(),
            sim_now: at,
            warning: None,
            paused,
            blur_enabled: true,
            desktop_sharp,
            desktop_blurred,
            lock_wallpaper,
            warning_wait: None,
            warning_phase: None,
            state_file,
            accept_warning_input: false,
            warning_pointer_origin: None,
            warning_input_after: Instant::now(),
            last_state: RefCell::new(String::new()),
            warning_visual_key: None,
            pending_acks: Vec::new(),
        }
    }

    fn record(&self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("[vigil-sim] {message}");
        self.trace.borrow_mut().push(message);
        self.write_state();
    }

    fn write_state(&self) {
        let Some(path) = self.state_file.as_ref() else {
            return;
        };
        let phase = self.warning_phase.map(|phase| format!("{phase:?}"));
        let body = format!(
            "{{\n  \"mode\": \"{:?}\",\n  \"warning_phase\": {},\n  \"time_ms\": {},\n  \"paused\": {},\n  \"blur_enabled\": {},\n  \"drawer_open\": {}\n}}\n",
            self.mode,
            phase.map_or_else(|| "null".into(), |value| format!("\"{value}\"")),
            self.sim_now.as_millis(),
            self.paused,
            self.blur_enabled,
            self.drawer_open
        );
        if *self.last_state.borrow() == body {
            return;
        }
        if let Err(error) = fs::write(path, &body) {
            eprintln!("[vigil-sim] state write failed: {error}");
        } else {
            *self.last_state.borrow_mut() = body;
        }
    }

    fn create_scene(&mut self) {
        let theme = Theme::load_or_default(None);
        let component = theme.instantiate().expect("instantiate embedded theme");
        let adapter = self.platform.claim_last_adapter().expect("theme adapter");
        let mut scene = OutputWindow::new(OutputId(1), WIDTH, HEIGHT, 1.0, adapter, component)
            .expect("create simulated output");
        // The host compositor already supplies a cursor for this ordinary
        // window. Drawing Vigil's DRM/session cursor too creates a misleading
        // duplicate whose coordinates diverge under output scaling.
        scene.set_cursor_visible(false);
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
                scene.set_background((*self.lock_wallpaper).clone(), WIDTH, HEIGHT);
                scene.set_panel_visible(true);
                scene.set_power_visible(true);
                scene.set_users(&["mason".into()]);
                scene.set_sessions(&[]);
                scene.show_prompt("Password:", true);
                scene.set_status_banner("SIMULATED LOCK — host session unaffected");
            }
            Mode::Warning => {
                // The fake desktop is exclusively the pre-lock plane used to
                // verify capture-free frost. It must never back Login or the
                // committed Lock state, where the resolved lock wallpaper is
                // opaque before authentication becomes visible.
                scene.set_background((*self.desktop_sharp).clone(), WIDTH, HEIGHT);
                scene.set_panel_visible(false);
                scene.set_power_visible(false);
                scene.set_users(&[]);
                scene.set_sessions(&[]);
                scene.show_info("SIMULATED WARNING — press any key to cancel");
                scene.set_status_banner("Frost stage (simulated compositor blur)");
                let config = vigil_config::LockWarning {
                    duration_ms: 10_000,
                    ..Default::default()
                };
                let mut warning = Timeline::new(config);
                warning.start(Duration::ZERO);
                self.last_tick = Instant::now();
                self.warning = Some(warning);
                self.warning_phase = Some(Phase::Mapped);
                self.accept_warning_input = false;
                self.warning_pointer_origin = None;
                self.warning_visual_key = None;
                // Winit/compositors emit synthetic enter/motion events while
                // mapping a new window. They are not user activity.
                self.warning_input_after = Instant::now() + Duration::from_secs(1);
            }
        }
        let trace = self.trace.clone();
        scene.on_ui_message(Rc::new(move |message| {
            trace.borrow_mut().push(format!("ui: {message:?}"));
        }));
        self.scene = Some(scene);
        self.write_state();
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
        if self.mode == Mode::Warning
            && !self.paused
            && !self.drawer_open
            && self.accept_warning_input
            && let Some(warning) = self.warning.as_mut()
        {
            warning.input(&event);
        }
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
        if !drew
            && self.started.elapsed().as_millis() > 100
            && !self.drawer_open
            && self.pending_acks.is_empty()
        {
            return;
        }
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let size = self.window.as_ref().expect("window").inner_size();
        surface
            .resize(
                NonZeroU32::new(size.width).unwrap(),
                NonZeroU32::new(size.height).unwrap(),
            )
            .expect("resize surface");
        let mut buffer = surface.buffer_mut().expect("acquire window buffer");
        let mut canvas: Vec<u32> = self
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| u32::from_le_bytes([px[0], px[1], px[2], 0]))
            .collect();
        draw_hamburger(&mut canvas);
        if self.drawer_open {
            draw_drawer(&mut canvas, self.mode);
        }
        for y in 0..size.height {
            let source_y = y * HEIGHT / size.height;
            for x in 0..size.width {
                let source_x = x * WIDTH / size.width;
                buffer[(y * size.width + x) as usize] =
                    canvas[(source_y * WIDTH + source_x) as usize];
            }
        }
        buffer.present().expect("present simulated output");
        for ack in self.pending_acks.drain(..) {
            let _ = ack.send(Ok(()));
        }
    }

    fn update_warning(&mut self) {
        if self.mode != Mode::Warning {
            return;
        }
        let now = Instant::now();
        if !self.paused {
            self.sim_now += now.saturating_duration_since(self.last_tick);
        }
        self.last_tick = now;
        let Some(timeline) = self.warning.as_mut() else {
            return;
        };
        let sample = timeline.sample(self.sim_now);
        let elements = timeline.element_samples(self.sim_now);
        let visual_key = (self.sim_now.as_millis(), self.blur_enabled);
        let visual_changed = self.warning_visual_key != Some(visual_key);
        self.warning_visual_key = Some(visual_key);
        self.warning_phase = Some(sample.phase);
        self.write_state();
        self.warning_wait = sample.next_frame;
        if visual_changed && let Some(scene) = self.scene.as_mut() {
            let panel_visible = elements.iter().any(|element| {
                matches!(element.selector.as_str(), "user_selector" | "password")
                    && element.progress > 0.001
            });
            let power_visible = elements
                .iter()
                .any(|element| element.selector == "power" && element.progress > 0.001);
            scene.set_panel_visible(panel_visible);
            scene.set_power_visible(power_visible);
            let frost = if self.blur_enabled { sample.frost } else { 0.0 };
            let frame = blend_warning_frame(
                &self.desktop_sharp,
                &self.desktop_blurred,
                &self.lock_wallpaper,
                frost,
                sample.wallpaper,
            );
            scene.set_background(frame, WIDTH, HEIGHT);
            // The production frost plane is supplied by the compositor. The
            // simulator models that plane in pixels, so disable the old tint
            // placeholder rather than stacking it over the preview.
            scene.set_warning_progress(0.0, 0.0);
            for element in elements {
                scene.set_warning_element(&element.selector, element.progress);
            }
            scene.set_status_banner(&format!(
                "Warning {:.1}s · frost {:.0}% · wallpaper {:.0}%",
                self.sim_now.as_secs_f32(),
                sample.frost * 100.0,
                sample.wallpaper * 100.0
            ));
        }
        if visual_changed && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
        if sample.phase == Phase::Cancelled {
            self.record("warning: cancelled");
            self.select_mode(Mode::Login);
            return;
        }
        if sample.should_commit {
            self.select_mode(Mode::Lock);
        }
    }

    fn select_mode(&mut self, mode: Mode) {
        if mode == Mode::Warning {
            self.sim_now = Duration::ZERO;
            self.last_tick = Instant::now();
        } else {
            self.warning_phase = None;
        }
        self.mode = mode;
        self.record(format!("state: {mode:?}"));
        self.create_scene();
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn control_click(&mut self) {
        let y = self.pointer.y.max(0.0) as u32;
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
            self.paused = !self.paused;
            self.record("warning: pause/resume");
        } else if (400..432).contains(&y) {
            self.sim_now += Duration::from_secs(1);
            self.record("warning: advance 1s");
        } else if (442..474).contains(&y) {
            if let Some(warning) = self.warning.as_mut() {
                warning.request_commit();
            }
            self.trace.borrow_mut().push("warning: commit".into());
        } else if (484..516).contains(&y) {
            if let Some(warning) = self.warning.as_mut() {
                warning.input(&InputEvent::Key {
                    keysym: 1,
                    utf8: None,
                    pressed: true,
                });
            }
            self.trace.borrow_mut().push("warning: cancel".into());
        } else if (526..558).contains(&y) {
            if let Some(warning) = self.warning.as_mut() {
                warning.hotplug();
            }
            self.trace.borrow_mut().push("warning: hotplug".into());
        } else if (568..600).contains(&y) {
            self.blur_enabled = !self.blur_enabled;
            self.trace.borrow_mut().push("warning: toggle blur".into());
        }
    }

    fn handle_shortcut(&mut self, key: &Key) -> bool {
        match key {
            Key::Named(NamedKey::F1) => self.select_mode(Mode::Login),
            Key::Named(NamedKey::F2) => self.select_mode(Mode::Lock),
            Key::Named(NamedKey::F3) => self.select_mode(Mode::Warning),
            Key::Named(NamedKey::Space) if self.mode == Mode::Warning => {
                self.paused = !self.paused;
                self.record("warning: pause/resume (keyboard)");
            }
            Key::Named(NamedKey::ArrowRight) if self.mode == Mode::Warning && self.paused => {
                self.sim_now += Duration::from_secs(1);
                self.record("warning: advance 1s (keyboard)");
            }
            Key::Character(key) if key.eq_ignore_ascii_case("d") => {
                self.drawer_open = !self.drawer_open;
                self.record("drawer: toggle (keyboard)");
            }
            Key::Character(key) if key.eq_ignore_ascii_case("b") && self.mode == Mode::Warning => {
                self.blur_enabled = !self.blur_enabled;
                self.record("warning: toggle blur (keyboard)");
            }
            _ => return false,
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
        true
    }

    fn apply_command(&mut self, command: &str) -> Result<(), String> {
        match parse_control_command(command)? {
            // Mirrors `vigil-lock --wait`: the socket response is emitted
            // only after the resulting lock frame has been presented.
            ControlCommand::LockWait => self.select_mode(Mode::Lock),
            ControlCommand::State(mode) => self.select_mode(mode),
            ControlCommand::Pause => {
                self.paused = true;
                self.record("warning: pause (socket)");
            }
            ControlCommand::Resume => {
                self.paused = false;
                self.last_tick = Instant::now();
                self.record("warning: resume (socket)");
            }
            ControlCommand::Advance(ms) if self.mode == Mode::Warning => {
                self.sim_now += Duration::from_millis(ms);
                self.record(format!("warning: advance {ms}ms (socket)"));
            }
            ControlCommand::Commit if self.mode == Mode::Warning => {
                if let Some(warning) = self.warning.as_mut() {
                    warning.request_commit();
                }
                self.record("warning: commit (socket)");
            }
            ControlCommand::Cancel if self.mode == Mode::Warning => {
                if let Some(warning) = self.warning.as_mut() {
                    warning.input(&InputEvent::Key {
                        keysym: 1,
                        utf8: None,
                        pressed: true,
                    });
                }
                self.record("warning: cancel (socket)");
            }
            ControlCommand::Hotplug if self.mode == Mode::Warning => {
                if let Some(warning) = self.warning.as_mut() {
                    warning.hotplug();
                }
                self.record("warning: hotplug (socket)");
            }
            ControlCommand::Blur(value) if self.mode == Mode::Warning => {
                self.blur_enabled = value.unwrap_or(!self.blur_enabled);
                self.record("warning: blur changed (socket)");
            }
            ControlCommand::Drawer(value) => {
                self.drawer_open = value.unwrap_or(!self.drawer_open);
                self.record("drawer: changed (socket)");
            }
            _ => return Err(format!("command is unavailable in {:?} mode", self.mode)),
        }
        self.update_warning();
        self.write_state();
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
        Ok(())
    }
}

impl ApplicationHandler<UserCommand> for Simulator {
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
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let _ = window_id;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.pointer_position(position);
                self.pointer = PhysicalPosition::new(x, y);
                self.accept_warning_input = Instant::now() >= self.warning_input_after
                    && self.warning_pointer_origin.is_some_and(|origin| {
                        let dx = origin.x - x;
                        let dy = origin.y - y;
                        dx * dx + dy * dy >= 64.0
                    });
                if Instant::now() < self.warning_input_after {
                    self.warning_pointer_origin = Some(self.pointer);
                } else {
                    self.warning_pointer_origin.get_or_insert(self.pointer);
                }
                self.dispatch(InputEvent::PointerAbsolute { x, y });
                self.accept_warning_input = false;
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left && state == ElementState::Pressed {
                    let x = self.pointer.x.max(0.0) as u32;
                    let y = self.pointer.y.max(0.0) as u32;
                    if (!self.drawer_open && x < 56 && y < 56)
                        || (self.drawer_open && x < DRAWER_WIDTH)
                    {
                        if !self.drawer_open {
                            self.drawer_open = true;
                        } else if x < 56 && y < 56 {
                            self.drawer_open = false;
                        } else {
                            self.control_click();
                        }
                        if let Some(window) = self.window.as_ref() {
                            window.request_redraw();
                        }
                        return;
                    }
                }
                let button = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    MouseButton::Back => 0x116,
                    MouseButton::Forward => 0x115,
                    MouseButton::Other(value) => u32::from(value),
                };
                self.accept_warning_input = true;
                self.dispatch(InputEvent::PointerButton {
                    button,
                    pressed: state == ElementState::Pressed,
                });
                self.accept_warning_input = false;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if pressed && self.handle_shortcut(&event.logical_key) {
                    return;
                }
                let (keysym, utf8) = key_event(&event.logical_key);
                self.accept_warning_input = true;
                self.dispatch(InputEvent::Key {
                    keysym,
                    utf8,
                    pressed,
                });
                self.accept_warning_input = false;
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        vigil_ui::advance_timers();
        self.update_warning();
        if self.mode == Mode::Warning {
            if !self.paused
                && let Some(delay) = self.warning_wait
            {
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + delay));
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            return;
        }
        if let Some(delay) = vigil_ui::duration_until_next_timer_update() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + delay));
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserCommand) {
        match self.apply_command(&event.0) {
            Ok(()) => {
                self.pending_acks.push(event.1);
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            Err(error) => {
                self.record(format!("command rejected: {error}"));
                let _ = event.1.send(Err(error));
            }
        }
    }
}

fn start_control_socket(
    path: std::path::PathBuf,
    proxy: winit::event_loop::EventLoopProxy<UserCommand>,
) {
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            usage(&format!("control path is not a socket: {}", path.display()));
        }
        fs::remove_file(&path)
            .unwrap_or_else(|error| usage(&format!("cannot replace control socket: {error}")));
    }
    let listener = UnixListener::bind(&path)
        .unwrap_or_else(|error| usage(&format!("cannot bind control socket: {error}")));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut command = String::new();
            if stream.read_to_string(&mut command).is_ok() && !command.trim().is_empty() {
                let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
                if proxy
                    .send_event(UserCommand(command.trim().to_owned(), done_tx))
                    .is_ok()
                {
                    match done_rx.recv_timeout(Duration::from_secs(2)) {
                        Ok(Ok(())) => {
                            let _ = stream.write_all(b"applied\n");
                        }
                        Ok(Err(error)) => {
                            let _ = writeln!(stream, "rejected: {error}");
                        }
                        Err(_) => {
                            let _ = stream.write_all(b"failed: presentation timeout\n");
                        }
                    }
                } else {
                    let _ = stream.write_all(b"failed: simulator unavailable\n");
                }
            }
        }
    });
}

fn draw_hamburger(pixels: &mut [u32]) {
    fill_rect(pixels, (WIDTH, HEIGHT), 12, 12, 44, 40, 0x00_251f1c);
    for y in [21, 27, 33] {
        fill_rect(pixels, (WIDTH, HEIGHT), 22, y, 20, 2, 0x00_fff5ee);
    }
}

fn draw_drawer(pixels: &mut [u32], mode: Mode) {
    fill_rect(
        pixels,
        (WIDTH, HEIGHT),
        0,
        0,
        DRAWER_WIDTH,
        HEIGHT,
        0x00_110e0c,
    );
    draw_text(pixels, 24, 22, "VIGIL SIMULATION", 0x00_ffb77a, 2);
    draw_text(pixels, 24, 52, "HOST SESSION UNAFFECTED", 0x00_b7ada6, 1);
    draw_text(pixels, 24, 88, "STATE", 0x00_f4eee9, 1);
    for (idx, (state, label)) in [
        (Mode::Login, "LOGIN"),
        (Mode::Lock, "LOCK"),
        (Mode::Warning, "IDLE WARNING"),
    ]
    .iter()
    .enumerate()
    {
        let y = 112 + idx as u32 * 54;
        fill_rect(
            pixels,
            (WIDTH, HEIGHT),
            24,
            y,
            372,
            42,
            if *state == mode {
                0x00_a95f2c
            } else {
                0x00_352b26
            },
        );
        draw_text(pixels, 40, y + 13, label, 0x00_fff5ee, 1);
    }
    draw_text(pixels, 24, 292, "WARNING CONTROLS", 0x00_f4eee9, 1);
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
        fill_rect(pixels, (WIDTH, HEIGHT), 24, y, 372, 32, 0x00_251f1c);
        draw_text(pixels, 40, y + 9, label, 0x00_ddd3cc, 1);
    }
}

fn fill_rect(
    pixels: &mut [u32],
    canvas: (u32, u32),
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: u32,
) {
    let (canvas_width, canvas_height) = canvas;
    for row in y..(y + height).min(canvas_height) {
        let start = (row * canvas_width + x) as usize;
        let end = (row * canvas_width + (x + width).min(canvas_width)) as usize;
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
                            (WIDTH, HEIGHT),
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

fn fake_desktop() -> Vec<u8> {
    let mut rgba = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let i = ((y * WIDTH + x) * 4) as usize;
            rgba[i] = (38 + x * 34 / WIDTH) as u8;
            rgba[i + 1] = (52 + y * 42 / HEIGHT) as u8;
            rgba[i + 2] = (76 + (x + y) * 44 / (WIDTH + HEIGHT)) as u8;
            rgba[i + 3] = 255;
        }
    }
    for (x, y, w, h, c) in [
        (0, 0, WIDTH, 32, [24, 27, 36, 255]),
        (90, 105, 720, 470, [238, 235, 226, 255]),
        (850, 150, 330, 390, [35, 39, 52, 255]),
        (120, 140, 660, 44, [66, 91, 135, 255]),
    ] {
        for row in y..y + h {
            for col in x..x + w {
                let i = ((row * WIDTH + col) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&c);
            }
        }
    }
    rgba
}

/// CPU blur exists only in the safe simulator. Production uses the Wayland
/// background-effect protocol and never captures or reads desktop pixels.
fn blur_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .expect("simulator desktop dimensions");
    // A full-resolution Gaussian is several seconds in an unoptimized dev
    // build and delays the very harness intended to accelerate UI work.
    // Downsample/upsample is a deterministic, sub-frame approximation of the
    // compositor blur plane and is deliberately simulator-only.
    let small = image::imageops::resize(
        &image,
        (width / 16).max(1),
        (height / 16).max(1),
        image::imageops::FilterType::Triangle,
    );
    image::imageops::resize(
        &small,
        width,
        height,
        image::imageops::FilterType::CatmullRom,
    )
    .into_raw()
}

fn fake_lock_wallpaper() -> Vec<u8> {
    let mut rgba = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let i = ((y * WIDTH + x) * 4) as usize;
            let glow = ((x as f32 / WIDTH as f32 * std::f32::consts::PI).sin() * 30.0) as u8;
            rgba[i] = 18 + glow / 3;
            rgba[i + 1] = 28 + glow / 2;
            rgba[i + 2] = 44 + glow;
            rgba[i + 3] = 255;
        }
    }
    rgba
}

fn blend_warning_frame(
    sharp: &[u8],
    blurred: &[u8],
    wallpaper: &[u8],
    frost: f32,
    wallpaper_alpha: f32,
) -> Vec<u8> {
    sharp
        .iter()
        .zip(blurred)
        .zip(wallpaper)
        .map(|((&sharp, &blurred), &wallpaper)| {
            let frosted = sharp as f32 + (blurred as f32 - sharp as f32) * frost;
            (frosted + (wallpaper as f32 - frosted) * wallpaper_alpha)
                .round()
                .clamp(0.0, 255.0) as u8
        })
        .collect()
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
    let args = SimArgs::parse();
    let platform = VigilPlatform::install().expect("install Vigil Slint platform");
    let event_loop = EventLoop::<UserCommand>::with_user_event()
        .build()
        .expect("create event loop");
    if let Some(path) = args.control_socket.clone() {
        start_control_socket(path, event_loop.create_proxy());
    }
    let mut simulator = Simulator::new(args.mode, platform, args.paused, args.at, args.state_file);
    event_loop.run_app(&mut simulator).expect("run simulator");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_parser_accepts_wait_and_rejects_malformed_commands() {
        assert_eq!(
            parse_control_command("lock --wait").unwrap(),
            ControlCommand::LockWait
        );
        assert_eq!(
            parse_control_command("advance 125").unwrap(),
            ControlCommand::Advance(125)
        );
        assert!(parse_control_command("advance tomorrow").is_err());
        assert!(parse_control_command("lock --maybe").is_err());
        assert!(parse_control_command("state locked-ish").is_err());
    }

    #[test]
    fn warning_blend_has_exact_endpoints() {
        let sharp = [0, 10, 20, 255];
        let blurred = [100, 110, 120, 255];
        let wallpaper = [200, 210, 220, 255];
        assert_eq!(
            blend_warning_frame(&sharp, &blurred, &wallpaper, 0.0, 0.0),
            sharp
        );
        assert_eq!(
            blend_warning_frame(&sharp, &blurred, &wallpaper, 1.0, 0.0),
            blurred
        );
        assert_eq!(
            blend_warning_frame(&sharp, &blurred, &wallpaper, 1.0, 1.0),
            wallpaper
        );
    }
}
