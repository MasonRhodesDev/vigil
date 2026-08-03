//! The vigil binary: calloop wiring ONLY (DESIGN.md §5). Every subsystem
//! lives in its crate; this file assembles them behind vigil-core seams.
//!
//! Config file support is M2; for M1 everything is CLI flags (greetd's
//! `command =` line carries them).

mod layout;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use vigil_auth::AuthMachine;
use vigil_core::{
    AuthUi, BackgroundFit, InputEvent, OutputEvent, OutputId, PresentError, Presenter,
    SessionEvent, UiMessage,
};
use vigil_input::InputSystem;
use vigil_outputs::OutputManager;
use vigil_present_dumb::DumbBufferPresenter;
use vigil_session::SessionManager;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct Cli {
    user: String,
    socket: Option<String>,
    theme: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: BackgroundFit,
    cmd: Vec<String>,
}

fn parse_cli() -> Result<Cli, String> {
    let mut args = std::env::args().skip(1);
    let mut cli = Cli {
        user: String::new(),
        socket: None,
        theme: None,
        background: None,
        bg_mode: BackgroundFit::default(),
        cmd: Vec::new(),
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--user" => cli.user = value("--user")?,
            "--socket" => cli.socket = Some(value("--socket")?),
            "--theme" => cli.theme = Some(PathBuf::from(value("--theme")?)),
            "--background" => cli.background = Some(PathBuf::from(value("--background")?)),
            "--bg-mode" => {
                let v = value("--bg-mode")?;
                cli.bg_mode = BackgroundFit::parse(&v).ok_or(format!("unknown bg-mode {v}"))?;
            }
            "--cmd" => {
                cli.cmd = args.by_ref().collect();
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if cli.user.is_empty() {
        return Err("--user is required".into());
    }
    if cli.cmd.is_empty() {
        cli.cmd = vec!["/bin/sh".into(), "-l".into()];
    }
    Ok(cli)
}

/// One live output: its swapchain and its scene.
struct Entry {
    id: OutputId,
    width: u32,
    height: u32,
    presenter: DumbBufferPresenter,
    window: OutputWindow,
}

/// Fan-out AuthUi: every monitor mirrors the auth state.
struct FanUi<'a>(&'a mut [Entry]);

impl AuthUi for FanUi<'_> {
    fn show_prompt(&mut self, text: &str, secret: bool) {
        for e in self.0.iter_mut() {
            e.window.show_prompt(text, secret);
        }
    }
    fn show_info(&mut self, text: &str) {
        for e in self.0.iter_mut() {
            e.window.show_info(text);
        }
    }
    fn show_error(&mut self, text: &str) {
        for e in self.0.iter_mut() {
            e.window.show_error(text);
        }
    }
    fn set_busy(&mut self, busy: bool) {
        for e in self.0.iter_mut() {
            e.window.set_busy(busy);
        }
    }
}

struct App {
    outputs: OutputManager,
    input: InputSystem,
    auth: AuthMachine,
    theme: Theme,
    platform: VigilPlatform,
    entries: Vec<Entry>,
    row: layout::Row,
    cursor: (f64, f64),
    panel: usize,
    queue: Rc<RefCell<VecDeque<UiMessage>>>,
    background: Option<PathBuf>,
    bg_mode: BackgroundFit,
    caps_lock: bool,
    last_clock: (Instant, String),
    signal: LoopSignal,
    exit_code: i32,
}

impl App {
    fn on_session(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Pause => self.outputs.pause(),
            SessionEvent::Activate => {
                let _ = self.outputs.activate();
                // Presenters re-commit on the next drawn frame; force one.
                for e in self.entries.iter_mut() {
                    e.window.set_panel_visible(false);
                }
                self.apply_panel();
            }
        }
    }

    fn rescan(&mut self) {
        let events = match self.outputs.scan() {
            Ok(events) => events,
            Err(e) => {
                eprintln!("vigil: hotplug scan failed: {e}");
                return;
            }
        };
        for event in events {
            match event {
                OutputEvent::Added(id, _) => {
                    if let Err(e) = self.add_output(id) {
                        eprintln!("vigil: skipping output {id:?}: {e}");
                    }
                }
                OutputEvent::Removed(id) => self.remove_output(id),
                OutputEvent::NeedsRedraw(_) => {}
            }
        }
        self.rebuild_row();
    }

    fn add_output(&mut self, id: OutputId) -> Result<(), String> {
        let info = self.outputs.info(id).cloned().ok_or("no info for output")?;
        let surface = self.outputs.create_surface(id).map_err(|e| e.to_string())?;
        let presenter = DumbBufferPresenter::new(surface).map_err(|e| e.to_string())?;
        let component = self.theme.instantiate().map_err(|e| e.to_string())?;
        let adapter = self
            .platform
            .claim_last_adapter()
            .ok_or("no window adapter captured for theme instance")?;
        let mut window =
            OutputWindow::new(id, info.width, info.height, info.scale, adapter, component)
                .map_err(|e| e.to_string())?;

        if let Some(path) = &self.background {
            match vigil_ui::background(path, self.bg_mode, info.width, info.height) {
                Ok(rgba) => window.set_background(rgba, info.width, info.height),
                Err(e) => eprintln!("vigil: background: {e}"),
            }
        }
        window.set_clock(&self.last_clock.1);
        window.set_caps_lock(self.caps_lock);
        window.set_panel_visible(false);
        let queue = self.queue.clone();
        window.on_ui_message(Rc::new(move |m| queue.borrow_mut().push_back(m)));

        eprintln!(
            "vigil: output {} {}x{}",
            info.connector, info.width, info.height
        );
        self.entries.push(Entry {
            id,
            width: info.width,
            height: info.height,
            presenter,
            window,
        });
        Ok(())
    }

    fn remove_output(&mut self, id: OutputId) {
        self.entries.retain(|e| e.id != id);
        if self.panel >= self.entries.len() {
            self.panel = 0;
        }
    }

    fn rebuild_row(&mut self) {
        let spans: Vec<_> = self
            .entries
            .iter()
            .map(|e| (e.id, e.width, e.height))
            .collect();
        self.row.rebuild(&spans);
        let (cx, cy) = self.row.clamp(self.cursor.0, self.cursor.1);
        self.cursor = (cx, cy);
        self.apply_panel();
    }

    fn apply_panel(&mut self) {
        for (i, e) in self.entries.iter_mut().enumerate() {
            e.window.set_panel_visible(i == self.panel);
        }
    }

    fn route(&mut self, events: Vec<InputEvent>) {
        for event in events {
            match event {
                InputEvent::Key { .. } => {
                    if let Some(e) = self.entries.get_mut(self.panel) {
                        e.window.dispatch(event);
                    }
                }
                InputEvent::PointerMotion { dx, dy } => {
                    self.move_cursor(self.cursor.0 + dx, self.cursor.1 + dy);
                }
                InputEvent::PointerAbsolute { x, y } => {
                    let (gx, gy) = self.row.denormalize(x, y);
                    self.move_cursor(gx, gy);
                }
                InputEvent::PointerButton { .. } => {
                    if let Some(e) = self.entries.get_mut(self.panel) {
                        e.window.dispatch(event);
                    }
                }
            }
        }
    }

    fn move_cursor(&mut self, x: f64, y: f64) {
        let (cx, cy) = self.row.clamp(x, y);
        self.cursor = (cx, cy);
        let Some((idx, lx, ly)) = self.row.locate(cx, cy) else {
            return;
        };
        if idx != self.panel {
            self.panel = idx;
            self.apply_panel();
        }
        if let Some(e) = self.entries.get_mut(idx) {
            e.window
                .dispatch(InputEvent::PointerAbsolute { x: lx, y: ly });
        }
    }

    fn pump_messages(&mut self) {
        loop {
            let msg = self.queue.borrow_mut().pop_front();
            let Some(msg) = msg else { break };
            let mut fan = FanUi(&mut self.entries);
            if let Err(e) = self.auth.handle(msg, &mut fan) {
                eprintln!("vigil: auth: {e}");
            }
            if self.auth.is_complete() {
                self.exit_code = 0;
                self.signal.stop();
                return;
            }
        }
    }

    fn tick(&mut self) {
        vigil_ui::advance_timers();
        let repeats = self.input.tick_repeat(Instant::now());
        if !repeats.is_empty() {
            self.route(repeats);
        }
        let caps = self.input.caps_lock();
        if caps != self.caps_lock {
            self.caps_lock = caps;
            for e in self.entries.iter_mut() {
                e.window.set_caps_lock(caps);
            }
        }
        if self.last_clock.0.elapsed() >= Duration::from_secs(1) {
            let text = clock_text();
            if text != self.last_clock.1 {
                for e in self.entries.iter_mut() {
                    e.window.set_clock(&text);
                }
            }
            self.last_clock = (Instant::now(), text);
        }
        self.pump_messages();
        self.render();
    }

    fn render(&mut self) {
        let mut dead = Vec::new();
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let Entry {
                presenter, window, ..
            } = entry;
            match presenter.with_frame(&mut |target| window.render_if_needed(target)) {
                Ok(_) => {}
                Err(PresentError::DeviceLost) => dead.push(i),
                Err(e) => eprintln!("vigil: present: {e}"),
            }
        }
        for i in dead.into_iter().rev() {
            let id = self.entries[i].id;
            eprintln!("vigil: output {id:?} lost; awaiting rescan");
            self.entries.remove(i);
        }
        if self.panel >= self.entries.len() {
            self.panel = 0;
        }
    }
}

/// HH:MM via `date`; std has no local-time formatting and a chrono dependency
/// is not worth it for a clock string (M2 revisits with proper config).
fn clock_text() -> String {
    std::process::Command::new("date")
        .arg("+%H:%M")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn run() -> Result<i32, String> {
    let cli = parse_cli()?;

    let (session, notifier) = SessionManager::new().map_err(|e| e.to_string())?;
    let seat = session.seat_name();
    let gpu = vigil_outputs::primary_gpu_path(&seat).map_err(|e| e.to_string())?;
    let mut session = session;
    let fd = session.open_device(&gpu).map_err(|e| e.to_string())?;
    let (outputs, _drm_notifier) = OutputManager::new(fd).map_err(|e| e.to_string())?;
    let udev = vigil_outputs::udev_monitor(&seat).map_err(|e| e.to_string())?;

    let platform = VigilPlatform::install().map_err(|e| e.to_string())?;
    let theme = Theme::load_or_default(cli.theme.as_deref());

    let input =
        InputSystem::new(&seat, Box::new(session.device_opener())).map_err(|e| e.to_string())?;
    let input_fd = input
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup input fd: {e}"))?;

    let mut auth = AuthMachine::connect(cli.socket.as_deref()).map_err(|e| e.to_string())?;
    auth.set_session(cli.cmd.clone(), Vec::new());

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| format!("event loop: {e}"))?;
    let handle = event_loop.handle();

    let mut app = App {
        outputs,
        input,
        auth,
        theme,
        platform,
        entries: Vec::new(),
        row: layout::Row::default(),
        cursor: (0.0, 0.0),
        panel: 0,
        queue: Rc::new(RefCell::new(VecDeque::new())),
        background: cli.background.clone(),
        bg_mode: cli.bg_mode,
        caps_lock: false,
        last_clock: (Instant::now(), clock_text()),
        signal: event_loop.get_signal(),
        exit_code: 1,
    };

    app.rescan();
    {
        let mut fan = FanUi(&mut app.entries);
        app.auth
            .start(&cli.user, &mut fan)
            .map_err(|e| e.to_string())?;
    }

    handle
        .insert_source(notifier, |event, _, app: &mut App| {
            app.on_session(vigil_session::translate(event));
        })
        .map_err(|e| format!("session source: {e}"))?;

    handle
        .insert_source(udev, |_event, _, app: &mut App| {
            app.rescan();
        })
        .map_err(|e| format!("udev source: {e}"))?;

    handle
        .insert_source(
            Generic::new(input_fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                let events = app.input.dispatch();
                app.route(events);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("input source: {e}"))?;

    handle
        .insert_source(
            Timer::from_duration(FRAME_INTERVAL),
            |_deadline, _, app: &mut App| {
                app.tick();
                TimeoutAction::ToDuration(FRAME_INTERVAL)
            },
        )
        .map_err(|e| format!("timer source: {e}"))?;

    event_loop
        .run(None, &mut app, |_| {})
        .map_err(|e| format!("event loop run: {e}"))?;

    Ok(app.exit_code)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("vigil: {e}");
            std::process::exit(1);
        }
    }
}
