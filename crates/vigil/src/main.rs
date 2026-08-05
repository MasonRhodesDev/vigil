//! The vigil binary: calloop wiring ONLY (DESIGN.md §5). Every subsystem
//! lives in its crate; this file assembles them behind vigil-core seams.
//!
//! Config file support is M2; for M1 everything is CLI flags (greetd's
//! `command =` line carries them).

mod layout;
mod sessions;

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
use vigil_config::Config;
use vigil_core::{
    AuthUi, BackgroundFit, FrameTarget, InputEvent, OutputEvent, OutputId, PowerAction,
    PresentError, Presenter, SessionEvent, UiMessage,
};
use vigil_input::InputSystem;
use vigil_outputs::OutputManager;
use vigil_present_dumb::DumbBufferPresenter;
use vigil_session::SessionManager;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, UiSnapshot, VigilPlatform};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct Cli {
    user: Option<String>,
    socket: Option<String>,
    config: Option<PathBuf>,
    theme: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: Option<BackgroundFit>,
    cmd: Vec<String>,
}

fn parse_cli() -> Result<Cli, String> {
    let mut args = std::env::args().skip(1);
    let mut cli = Cli {
        user: None,
        socket: None,
        config: None,
        theme: None,
        background: None,
        bg_mode: None,
        cmd: Vec::new(),
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--user" => cli.user = Some(value("--user")?),
            "--socket" => cli.socket = Some(value("--socket")?),
            "--config" => cli.config = Some(PathBuf::from(value("--config")?)),
            "--theme" => cli.theme = Some(PathBuf::from(value("--theme")?)),
            "--background" => cli.background = Some(PathBuf::from(value("--background")?)),
            "--bg-mode" => {
                let v = value("--bg-mode")?;
                cli.bg_mode = Some(BackgroundFit::parse(&v).ok_or(format!("unknown bg-mode {v}"))?);
            }
            "--cmd" => {
                cli.cmd = args.by_ref().collect();
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(cli)
}

/// Effective settings after CLI-over-config-over-default merge.
struct Resolved {
    user: Option<String>,
    theme: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: BackgroundFit,
    cmd: Vec<String>,
    power_enabled: bool,
    clock_format: String,
}

fn resolve(cli: &Cli, config: &Config) -> Resolved {
    let fit = config.look.fit.as_deref().and_then(|s| {
        let parsed = BackgroundFit::parse(s);
        if parsed.is_none() {
            eprintln!("vigil: config: unknown fit `{s}`");
        }
        parsed
    });
    Resolved {
        user: cli
            .user
            .clone()
            .or_else(|| (!config.greeter.user.is_empty()).then(|| config.greeter.user.clone())),
        theme: cli.theme.clone().or_else(|| config.look.theme.clone()),
        background: cli
            .background
            .clone()
            .or_else(|| config.look.background.clone()),
        bg_mode: cli.bg_mode.or(fit).unwrap_or_default(),
        cmd: if cli.cmd.is_empty() {
            config.greeter.cmd.clone()
        } else {
            cli.cmd.clone()
        },
        power_enabled: config.power.enabled && std::env::var_os("VIGIL_POWER_INHIBIT").is_none(),
        clock_format: config.look.clock_format.clone(),
    }
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
struct FanUi<'a> {
    entries: &'a mut [Entry],
    snapshot: &'a mut UiSnapshot,
}

impl AuthUi for FanUi<'_> {
    fn show_prompt(&mut self, text: &str, secret: bool) {
        self.snapshot.on_prompt(text, secret);
        for e in self.entries.iter_mut() {
            e.window.show_prompt(text, secret);
        }
    }
    fn show_info(&mut self, text: &str) {
        self.snapshot.info = text.to_owned();
        for e in self.entries.iter_mut() {
            e.window.show_info(text);
        }
    }
    fn show_error(&mut self, text: &str) {
        self.snapshot.error = text.to_owned();
        for e in self.entries.iter_mut() {
            e.window.show_error(text);
        }
    }
    fn set_busy(&mut self, busy: bool) {
        self.snapshot.busy = busy;
        for e in self.entries.iter_mut() {
            e.window.set_busy(busy);
        }
    }
}

struct App {
    session: SessionManager,
    /// One manager per GPU (outputs can span cards); index == the
    /// `OutputId` namespace.
    outputs: Vec<OutputManager>,
    input: InputSystem,
    auth: AuthMachine,
    sessions: Vec<sessions::SessionEntry>,
    selected_session: usize,
    theme: Theme,
    platform: VigilPlatform,
    entries: Vec<Entry>,
    row: layout::Row,
    cursor: (f64, f64),
    panel: usize,
    queue: Rc<RefCell<VecDeque<UiMessage>>>,
    background: Option<PathBuf>,
    bg_mode: BackgroundFit,
    power_enabled: bool,
    clock_format: String,
    caps_lock: bool,
    last_clock: (Instant, String),
    snapshot: UiSnapshot,
    /// False while VT-switched away: DRM is paused, rendering must stop.
    active: bool,
    signal: LoopSignal,
    exit_code: i32,
}

impl App {
    fn on_session(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Pause => {
                self.active = false;
                self.input.suspend();
                for gpu in self.outputs.iter_mut() {
                    gpu.pause();
                }
            }
            SessionEvent::Activate => {
                self.active = true;
                if let Err(e) = self.input.resume() {
                    eprintln!("vigil: {e}");
                }
                for gpu in self.outputs.iter_mut() {
                    let _ = gpu.activate();
                }
                // A render racing the pause can hit DeviceInactive and drop
                // its entry, and no udev event replays a VT switch — so any
                // output a manager still knows but we lost gets rebuilt.
                let known: Vec<OutputId> = self.outputs.iter().flat_map(|gpu| gpu.ids()).collect();
                for id in known {
                    if !self.entries.iter().any(|e| e.id == id)
                        && let Err(e) = self.add_output(id)
                    {
                        eprintln!("vigil: rebuilding output {id:?}: {e}");
                    }
                }
                self.rebuild_row();
                // Presenters re-commit on the next drawn frame; force one.
                for e in self.entries.iter_mut() {
                    e.window.set_panel_visible(false);
                }
                self.apply_panel();
            }
        }
    }

    fn rescan(&mut self) {
        let mut events = Vec::new();
        for gpu in self.outputs.iter_mut() {
            match gpu.scan() {
                Ok(batch) => events.extend(batch),
                Err(e) => eprintln!("vigil: hotplug scan failed: {e}"),
            }
        }
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

    /// The manager owning `id` (its namespace is the vec index).
    fn gpu_for(&mut self, id: OutputId) -> Result<&mut OutputManager, String> {
        let index = (id.0 >> 24) as usize;
        self.outputs
            .get_mut(index)
            .ok_or_else(|| format!("no GPU {index} for output {id:?}"))
    }

    fn add_output(&mut self, id: OutputId) -> Result<(), String> {
        let gpu = self.gpu_for(id)?;
        let info = gpu.info(id).cloned().ok_or("no info for output")?;
        let surface = gpu.create_surface(id).map_err(|e| e.to_string())?;
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
        let names: Vec<String> = self.sessions.iter().map(|s| s.name.clone()).collect();
        window.set_sessions(&names);
        window.set_session_index(self.selected_session);
        self.snapshot.apply(&mut window);
        let queue = self.queue.clone();
        window.on_ui_message(Rc::new(move |m| queue.borrow_mut().push_back(m)));

        eprintln!(
            "vigil: output {} {}x{} (gpu {})",
            info.connector,
            info.width,
            info.height,
            id.0 >> 24
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
            // The panel output is by construction the one under the pointer.
            e.window.set_cursor_visible(i == self.panel);
        }
    }

    fn route(&mut self, events: Vec<InputEvent>) {
        // XF86Switch_VT_1..=XF86Switch_VT_12: the greeter owns Ctrl+Alt+Fn
        // because taking libinput disabled the kernel's handling. Without
        // this the greeter VT is a roach motel.
        const VT_FIRST: u32 = 0x1008_FE01;
        const VT_LAST: u32 = 0x1008_FE0C;
        const ESCAPE: u32 = 0xff1b;
        for event in events {
            match event {
                InputEvent::Key {
                    keysym, pressed, ..
                } => {
                    if pressed && (VT_FIRST..=VT_LAST).contains(&keysym) {
                        let vt = (keysym - VT_FIRST + 1) as i32;
                        if let Err(e) = self.session.change_vt(vt) {
                            eprintln!("vigil: change vt {vt}: {e}");
                        }
                        continue;
                    }
                    if pressed && keysym == ESCAPE {
                        self.queue.borrow_mut().push_back(UiMessage::Cancel);
                    }
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
            if let UiMessage::SelectSession(index) = msg {
                self.select_session(index);
                continue;
            }
            if let UiMessage::Power(action) = msg {
                self.power(action);
                continue;
            }
            let mut fan = FanUi {
                entries: &mut self.entries,
                snapshot: &mut self.snapshot,
            };
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

    /// Power actions go through logind (the greeter's session is active on
    /// the seat, so polkit's allow_active rule applies — no root needed).
    /// Config and `VIGIL_POWER_INHIBIT` can turn them into log lines.
    fn power(&mut self, action: PowerAction) {
        let arg = match action {
            PowerAction::Reboot => "reboot",
            PowerAction::Poweroff => "poweroff",
        };
        if !self.power_enabled {
            eprintln!("vigil: power action inhibited: systemctl {arg}");
            return;
        }
        eprintln!("vigil: power: systemctl {arg}");
        match std::process::Command::new("systemctl").arg(arg).status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("vigil: systemctl {arg} exited {status}"),
            Err(e) => eprintln!("vigil: systemctl {arg}: {e}"),
        }
    }

    /// Session choice is the binary's product logic: remember it, hand the
    /// command line to the auth machine, and mirror it on every output.
    fn select_session(&mut self, index: usize) {
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        self.selected_session = index;
        self.auth
            .set_session(session.cmd.clone(), session.env.clone());
        for e in self.entries.iter_mut() {
            e.window.set_session_index(index);
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
            let text = clock_text(&self.clock_format);
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
        if !self.active {
            return;
        }
        let mut dead = Vec::new();
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let Entry {
                presenter, window, ..
            } = entry;
            let debug_frames = std::env::var_os("VIGIL_DEBUG_FRAMES").is_some();
            match presenter.with_frame(&mut |target| {
                let (mid, row_len, stride) = (
                    target.stride * (target.height as usize / 2),
                    target.width as usize * 4,
                    target.stride,
                );
                let _ = stride;
                let drew = window.render_if_needed(FrameTarget {
                    buffer: target.buffer,
                    width: target.width,
                    height: target.height,
                    stride: target.stride,
                });
                if drew && debug_frames {
                    let row = &target.buffer[mid..mid + row_len];
                    let sum: u64 = row.iter().map(|&b| b as u64).sum();
                    eprintln!("vigil: frame drawn, mid-row byte sum {sum}");
                }
                drew
            }) {
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
fn clock_text(format: &str) -> String {
    std::process::Command::new("date")
        .arg(format!("+{format}"))
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn run() -> Result<i32, String> {
    let cli = parse_cli()?;
    let config = Config::load(cli.config.as_deref());
    let resolved = resolve(&cli, &config);

    let (session, notifier) = SessionManager::new().map_err(|e| e.to_string())?;
    let seat = session.seat_name();
    let mut session = session;

    // One manager per GPU on the seat, primary first — outputs can span
    // cards (found on the founding machine: the dock's DP hangs off the
    // dGPU while boot_vga drives the panel). A card that fails to open or
    // init is skipped with a log line, not fatal: a greeter with fewer
    // monitors beats no greeter.
    let mut outputs = Vec::new();
    for path in vigil_outputs::all_gpu_paths(&seat).map_err(|e| e.to_string())? {
        let namespace = outputs.len() as u32;
        let manager = session
            .open_device(&path)
            .map_err(|e| e.to_string())
            .and_then(|fd| OutputManager::new(fd, namespace).map_err(|e| e.to_string()));
        match manager {
            Ok((manager, _drm_notifier)) => outputs.push(manager),
            Err(e) => eprintln!("vigil: skipping GPU {}: {e}", path.display()),
        }
    }
    if outputs.is_empty() {
        return Err("no usable GPU on the seat".into());
    }
    let udev = vigil_outputs::udev_monitor(&seat).map_err(|e| e.to_string())?;

    let platform = VigilPlatform::install().map_err(|e| e.to_string())?;
    let theme = Theme::load_or_default(resolved.theme.as_deref());

    let input =
        InputSystem::new(&seat, Box::new(session.device_opener())).map_err(|e| e.to_string())?;
    let input_fd = input
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup input fd: {e}"))?;

    // `--cmd` pins a single fixed session (kiosk/test mode); otherwise the
    // installed wayland-/x-session entries are offered, defaulting to the
    // first. The list is never empty (login-shell fallback).
    let session_list = if resolved.cmd.is_empty() {
        sessions::enumerate(&config.sessions.dirs)
    } else {
        vec![sessions::SessionEntry {
            name: "Custom".into(),
            cmd: resolved.cmd.clone(),
            env: Vec::new(),
        }]
    };

    let mut auth = AuthMachine::connect(cli.socket.as_deref()).map_err(|e| e.to_string())?;
    auth.set_session(session_list[0].cmd.clone(), session_list[0].env.clone());

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| format!("event loop: {e}"))?;
    let handle = event_loop.handle();

    let mut app = App {
        session,
        outputs,
        input,
        auth,
        sessions: session_list,
        selected_session: 0,
        theme,
        platform,
        entries: Vec::new(),
        row: layout::Row::default(),
        cursor: (0.0, 0.0),
        panel: 0,
        queue: Rc::new(RefCell::new(VecDeque::new())),
        background: resolved.background.clone(),
        bg_mode: resolved.bg_mode,
        power_enabled: resolved.power_enabled,
        clock_format: resolved.clock_format.clone(),
        caps_lock: false,
        last_clock: (Instant::now(), clock_text(&resolved.clock_format)),
        snapshot: UiSnapshot::default(),
        active: true,
        signal: event_loop.get_signal(),
        exit_code: 1,
    };

    app.rescan();
    {
        let mut fan = FanUi {
            entries: &mut app.entries,
            snapshot: &mut app.snapshot,
        };
        app.auth
            .begin(resolved.user.as_deref(), &mut fan)
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
                // Never log event contents: keystrokes include the password.
                if std::env::var_os("VIGIL_DEBUG_FRAMES").is_some() && !events.is_empty() {
                    eprintln!("vigil: input ready: {} event(s)", events.len());
                }
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

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn cli_overrides_config() {
        let cli = Cli {
            user: Some("kiosk".into()),
            socket: None,
            config: None,
            theme: Some("/cli.slint".into()),
            background: None,
            bg_mode: None,
            cmd: Vec::new(),
        };
        let config = vigil_config::parse(
            "[look]\ntheme=\"/cfg.slint\"\nbackground=\"/cfg.png\"\nfit=\"tile\"\n\
             [greeter]\nuser=\"other\"\ncmd=[\"x\"]",
        )
        .unwrap();
        let resolved = resolve(&cli, &config);
        assert_eq!(resolved.theme, Some(PathBuf::from("/cli.slint")));
        assert_eq!(resolved.background, Some(PathBuf::from("/cfg.png")));
        assert_eq!(resolved.bg_mode, BackgroundFit::Tile);
        assert_eq!(resolved.user.as_deref(), Some("kiosk"));
        assert_eq!(resolved.cmd, ["x"]);
    }

    #[test]
    fn config_fills_when_cli_absent() {
        let cli = Cli {
            user: None,
            socket: None,
            config: None,
            theme: None,
            background: None,
            bg_mode: None,
            cmd: Vec::new(),
        };
        let config = vigil_config::parse(
            "[look]\ntheme=\"/cfg.slint\"\nbackground=\"/cfg.png\"\nfit=\"tile\"\n\
             [greeter]\nuser=\"other\"\ncmd=[\"x\"]",
        )
        .unwrap();
        let resolved = resolve(&cli, &config);
        assert_eq!(resolved.theme, Some(PathBuf::from("/cfg.slint")));
        assert_eq!(resolved.user.as_deref(), Some("other"));
    }

    #[test]
    fn defaults_when_both_absent() {
        let cli = Cli {
            user: None,
            socket: None,
            config: None,
            theme: None,
            background: None,
            bg_mode: None,
            cmd: Vec::new(),
        };
        let config = Config::default();
        let resolved = resolve(&cli, &config);
        assert_eq!(resolved.bg_mode, BackgroundFit::Fill);
        assert!(resolved.power_enabled);
        assert_eq!(resolved.clock_format, "%H:%M");
        assert_eq!(resolved.user, None);
    }
}
