//! The vigil binary: calloop wiring ONLY (DESIGN.md §5). Every subsystem
//! lives in its crate; this file assembles them behind vigil-core seams.
//!
//! Config file support is M2; for M1 everything is CLI flags (greetd's
//! `command =` line carries them).

mod layout;
mod sessions;
mod users;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use monitor_profiles::{ConnectedOutput, Profile, ResolvedOutput};
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
/// How often the banner file is re-read. Host integrations update it at
/// human timescale; a 1s poll costs one small read and needs no watcher.
const BANNER_POLL: Duration = Duration::from_secs(1);
/// Cap so a runaway file cannot break the theme's layout.
const BANNER_MAX: usize = 200;

/// Cycler slot that returns the panel to typing a name.
const OTHER_USER: &str = "Other…";

struct Cli {
    user: Option<String>,
    socket: Option<String>,
    config: Option<PathBuf>,
    theme: Option<PathBuf>,
    theme_check: Option<PathBuf>,
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
        theme_check: None,
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
            "--theme-check" => cli.theme_check = Some(PathBuf::from(value("--theme-check")?)),
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
    cmd: Vec<String>,
    power_enabled: bool,
    clock_format: String,
}

fn resolve(cli: &Cli, config: &Config) -> Resolved {
    Resolved {
        user: cli
            .user
            .clone()
            .or_else(|| (!config.greeter.user.is_empty()).then(|| config.greeter.user.clone())),
        theme: cli.theme.clone().or_else(|| config.look.theme.clone()),
        cmd: if cli.cmd.is_empty() {
            config.greeter.cmd.clone()
        } else {
            cli.cmd.clone()
        },
        power_enabled: config.power.enabled && std::env::var_os("VIGIL_POWER_INHIBIT").is_none(),
        clock_format: config.look.clock_format.clone(),
    }
}

/// Session preselected at startup. Precedence: the user's own last
/// successful session (when it still exists) > the operator's
/// `[sessions] default` > first. A remembered name that no longer matches
/// any entry falls through instead of stranding the user (issue #22).
fn initial_session(
    sessions: &[sessions::SessionEntry],
    state: Option<&vigil_config::State>,
    configured_default: &str,
) -> usize {
    state
        .and_then(|s| sessions.iter().position(|e| e.name == s.session))
        .or_else(|| {
            (!configured_default.is_empty())
                .then(|| sessions.iter().position(|e| e.name == configured_default))
                .flatten()
        })
        .unwrap_or(0)
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
    profiles: Vec<Profile>,
    layout: Vec<ResolvedOutput>,
    input: InputSystem,
    auth: AuthMachine,
    sessions: Vec<sessions::SessionEntry>,
    selected_session: usize,
    users: Vec<String>,
    selected_user: usize,
    remember: bool,
    state_file: PathBuf,
    remembered_user: Option<String>,
    theme: Theme,
    platform: VigilPlatform,
    entries: Vec<Entry>,
    row: layout::Row,
    cursor: (f64, f64),
    panel: usize,
    queue: Rc<RefCell<VecDeque<UiMessage>>>,
    looks: vigil_ui::Looks,
    power_enabled: bool,
    clock_format: String,
    banner_file: Option<PathBuf>,
    banner: String,
    last_banner: Instant,
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

    /// Match a profile against the connected outputs and cache its resolved
    /// geometry. Returns the outputs to skip (profile says disabled).
    fn resolve_profile(&mut self) -> Vec<OutputId> {
        self.layout.clear();
        if self.profiles.is_empty() {
            return Vec::new();
        }
        let mut connected = Vec::new();
        for gpu in &self.outputs {
            for id in gpu.ids() {
                if let Some(info) = gpu.info(id) {
                    connected.push((
                        id,
                        ConnectedOutput {
                            name: info.connector.clone(),
                            description: match (&info.make, &info.model) {
                                (Some(make), Some(model)) => format!("{make} {model}"),
                                _ => info.connector.clone(),
                            },
                        },
                    ));
                }
            }
        }
        let signature: Vec<String> = connected
            .iter()
            .map(|(_, c)| c.description.clone())
            .collect();
        let outputs: Vec<ConnectedOutput> = connected.iter().map(|(_, c)| c.clone()).collect();
        let Some(profile) = monitor_profiles::select(&signature, &self.profiles) else {
            eprintln!("vigil: no monitor profile matches {signature:?}");
            return Vec::new();
        };
        let resolved = monitor_profiles::resolve(profile, &outputs);
        for w in &resolved.warnings {
            eprintln!("vigil: profile {}: {w}", profile.name);
        }
        for u in &resolved.unmatched {
            eprintln!("vigil: profile {}: no output for {u}", profile.name);
        }
        eprintln!("vigil: monitor profile {}", profile.name);
        let mut disabled = Vec::new();
        for out in &resolved.outputs {
            let Some((id, _)) = connected.iter().find(|(_, c)| c.name == out.name) else {
                continue;
            };
            if !out.enabled {
                disabled.push(*id);
                continue;
            }
            if let Some(mode) = out.mode {
                let want = (mode.width, mode.height, mode.refresh.round() as u32);
                if let Some(gpu) = self.outputs.get_mut((id.0 >> 24) as usize)
                    && let Err(e) = gpu.set_mode(*id, want)
                {
                    eprintln!("vigil: profile {}: {e}; using preferred mode", profile.name);
                }
            }
        }
        self.layout = resolved.outputs;
        disabled
    }

    fn rescan(&mut self) {
        let mut events = Vec::new();
        for gpu in self.outputs.iter_mut() {
            match gpu.scan() {
                Ok(batch) => events.extend(batch),
                Err(e) => eprintln!("vigil: hotplug scan failed: {e}"),
            }
        }
        let disabled = self.resolve_profile();
        for event in events {
            match event {
                OutputEvent::Added(id, _) => {
                    if disabled.contains(&id) {
                        eprintln!("vigil: output {id:?} disabled by profile");
                        continue;
                    }
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
        // Precedence: [output."NAME"] > monitor profile > EDID-derived default.
        let profile_scale = self
            .layout
            .iter()
            .find(|o| o.name == info.connector && o.scale.is_finite() && o.scale > 0.0)
            .map(|o| o.scale as f32);
        let scale = self
            .looks
            .config
            .output
            .get(&info.connector)
            .and_then(|o| o.scale)
            .filter(|s| s.is_finite() && *s > 0.0)
            .or(profile_scale)
            .unwrap_or(info.scale);
        // A rotated output renders an upright scene at swapped dimensions;
        // the presenter still scans out the panel's own geometry.
        let transform = self
            .layout
            .iter()
            .find(|o| o.name == info.connector)
            .map_or(0, |o| o.transform);
        // 4..=7 are the flipped (mirrored) variants. Rotating without the
        // flip is wrong, but it is legibly wrong and still lets someone log
        // in, which beats refusing to drive the output at all.
        if transform > 3 {
            eprintln!(
                "vigil: {}: flipped transform {transform} not supported,                  rotating without the flip",
                info.connector
            );
        }
        let mut window = OutputWindow::with_transform(
            id,
            info.width,
            info.height,
            scale,
            transform,
            adapter,
            component,
        )
        .map_err(|e| e.to_string())?;
        let (scene_width, scene_height) = window.scene_size();

        let (background, fit) = self.looks.for_connector(&info.connector);
        if let Some(path) = &background {
            match vigil_ui::background(path, fit, scene_width, scene_height) {
                Ok(rgba) => window.set_background(rgba, scene_width, scene_height),
                Err(e) => eprintln!("vigil: background: {e}"),
            }
        }
        window.set_clock(&self.last_clock.1);
        window.set_caps_lock(self.caps_lock);
        window.set_status_banner(&self.banner);
        window.set_panel_visible(false);
        let names: Vec<String> = self.sessions.iter().map(|s| s.name.clone()).collect();
        window.set_sessions(&names);
        window.set_session_index(self.selected_session);
        window.set_users(&self.users);
        window.set_user_index(self.selected_user);
        match self.users.get(self.selected_user) {
            Some(name) if name != OTHER_USER => window.set_user_name(name),
            Some(_) => window.set_user_name(""),
            None => {
                if let Some(user) = &self.remembered_user {
                    window.set_user_name(user);
                }
            }
        }
        self.snapshot.apply(&mut window);
        let queue = self.queue.clone();
        window.on_ui_message(Rc::new(move |m| queue.borrow_mut().push_back(m)));

        eprintln!(
            "vigil: output {} {}x{} scale {scale} (gpu {}){}",
            info.connector,
            info.width,
            info.height,
            id.0 >> 24,
            match (&info.make, &info.model) {
                (Some(make), Some(model)) => format!(" [{make} {model}]"),
                _ => String::new(),
            }
        );
        self.entries.push(Entry {
            id,
            // Scene dimensions: pointer routing works in the space the user
            // sees, so a portrait monitor must present as portrait here.
            width: scene_width,
            height: scene_height,
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
        if self.layout.is_empty() {
            let spans: Vec<_> = self
                .entries
                .iter()
                .map(|e| (e.id, e.width, e.height))
                .collect();
            self.row.rebuild_scan_order(&spans);
        } else {
            let spans: Vec<_> = self
                .entries
                .iter()
                .map(|e| {
                    let connector = self
                        .outputs
                        .get((e.id.0 >> 24) as usize)
                        .and_then(|gpu| gpu.info(e.id))
                        .map(|i| i.connector.clone())
                        .unwrap_or_default();
                    let (x, y) = self
                        .layout
                        .iter()
                        .find(|o| o.name == connector)
                        .map_or((0, 0), |o| o.position);
                    (e.id, x, y, e.width, e.height)
                })
                .collect();
            self.row.rebuild(&spans);
        }
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
            if let UiMessage::SelectUser(index) = msg {
                self.select_user(index);
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
                if self.remember
                    && let Some(user) = self.auth.user()
                {
                    vigil_config::State {
                        user: user.to_owned(),
                        session: self.sessions[self.selected_session].name.clone(),
                    }
                    .store(&self.state_file);
                }
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

    /// User choice is product logic: it only changes which name an empty
    /// submit uses — `Other…` clears it so the field is typed into instead.
    fn select_user(&mut self, index: usize) {
        let Some(name) = self.users.get(index).cloned() else {
            return;
        };
        self.selected_user = index;
        let default = (name != OTHER_USER).then(|| name.clone());
        self.auth.set_default_user(default.clone());
        let label = default.unwrap_or_default();
        for e in self.entries.iter_mut() {
            e.window.set_user_index(index);
            e.window.set_user_name(&label);
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
        if self.last_banner.elapsed() >= BANNER_POLL {
            self.last_banner = Instant::now();
            self.refresh_banner();
        }
        self.pump_messages();
        self.render();
    }

    /// Re-read the banner file and push any change to every output. A
    /// missing or unreadable file means no banner: a host-integration
    /// channel must never be able to break the login screen.
    fn refresh_banner(&mut self) {
        let Some(path) = &self.banner_file else {
            return;
        };
        let text = std::fs::read_to_string(path)
            .map(|raw| banner_text(&raw))
            .unwrap_or_default();
        if text != self.banner {
            self.banner = text;
            for e in self.entries.iter_mut() {
                e.window.set_status_banner(&self.banner);
            }
        }
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

/// Normalize banner-file contents into one display line: non-whitespace
/// control characters are dropped (an escape sequence must not reach the
/// scene), whitespace runs — newlines included — collapse to single
/// spaces, and the result is capped.
fn banner_text(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    let line = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(BANNER_MAX).collect()
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

    // Author/CI tool: validate a theme and exit without touching the seat.
    if let Some(path) = &cli.theme_check {
        // The interpreter needs a platform to instantiate the probe component.
        VigilPlatform::install().map_err(|e| e.to_string())?;
        return match vigil_theme::check(path) {
            Ok(()) => {
                println!("{}: ok", path.display());
                Ok(0)
            }
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                Ok(1)
            }
        };
    }

    let config = Config::load(cli.config.as_deref());
    let resolved = resolve(&cli, &config);

    // Profiles are optional. A missing dir, unreadable files, or no matching
    // profile all degrade to today's scan-order layout — this is the login
    // screen; a layout file must never keep someone from logging in.
    let profiles = match &config.profiles.dir {
        Some(dir) => {
            let (profiles, diags) = monitor_profiles::load_dir(dir);
            for d in diags {
                eprintln!("vigil: profile {}: {}", d.source, d.message);
            }
            profiles
        }
        None => Vec::new(),
    };

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

    let keymap = vigil_core::KeymapSettings {
        rules: config.keyboard.rules.clone(),
        model: config.keyboard.model.clone(),
        layout: config.keyboard.layout.clone(),
        variant: config.keyboard.variant.clone(),
        options: config.keyboard.options.clone(),
    };
    let input = InputSystem::new(&seat, Box::new(session.device_opener()), &keymap)
        .map_err(|e| e.to_string())?;
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

    // Kiosk --cmd mode never remembers; otherwise preselect last session and
    // let an empty username submit the last user.
    let remember = resolved.cmd.is_empty() && config.sessions.remember;
    let remembered = remember
        .then(|| vigil_config::State::load(&config.sessions.state_file))
        .flatten();
    // A pinned --user/[greeter] user is kiosk mode: no list, no choosing.
    let user_list = if resolved.user.is_none() && config.users.show_list {
        let mut list = users::enumerate();
        if !list.is_empty() {
            list.push(OTHER_USER.to_owned());
        }
        list
    } else {
        Vec::new()
    };
    let remembered_user = remembered
        .as_ref()
        .map(|s| s.user.clone())
        .filter(|u| !u.is_empty());
    // Preselect the remembered user when it is still a real account.
    let selected_user = remembered_user
        .as_ref()
        .and_then(|u| user_list.iter().position(|name| name == u))
        .unwrap_or(0);
    let initial_session =
        initial_session(&session_list, remembered.as_ref(), &config.sessions.default);

    let mut auth = AuthMachine::connect(cli.socket.as_deref()).map_err(|e| e.to_string())?;
    // With a list, the selected entry is the default an empty submit uses;
    // without one, the remembered name still is (G2).
    auth.set_default_user(match user_list.get(selected_user) {
        Some(name) if name != OTHER_USER => Some(name.clone()),
        Some(_) => None,
        None => remembered_user.clone(),
    });
    auth.set_session(
        session_list[initial_session].cmd.clone(),
        session_list[initial_session].env.clone(),
    );

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| format!("event loop: {e}"))?;
    let handle = event_loop.handle();

    let mut app = App {
        session,
        outputs,
        profiles,
        layout: Vec::new(),
        input,
        auth,
        sessions: session_list,
        selected_session: initial_session,
        users: user_list,
        selected_user,
        remember,
        state_file: config.sessions.state_file.clone(),
        remembered_user,
        theme,
        platform,
        entries: Vec::new(),
        row: layout::Row::default(),
        cursor: (0.0, 0.0),
        panel: 0,
        queue: Rc::new(RefCell::new(VecDeque::new())),
        looks: vigil_ui::Looks {
            cli_background: cli.background.clone(),
            cli_fit: cli.bg_mode,
            config: config.clone(),
        },
        power_enabled: resolved.power_enabled,
        clock_format: resolved.clock_format.clone(),
        banner_file: config.greeter.banner_file.clone(),
        banner: String::new(),
        last_banner: Instant::now(),
        caps_lock: false,
        last_clock: (Instant::now(), clock_text(&resolved.clock_format)),
        snapshot: UiSnapshot::default(),
        active: true,
        signal: event_loop.get_signal(),
        exit_code: 1,
    };

    app.rescan();
    app.refresh_banner();
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
    fn banner_text_collapses_to_one_line() {
        assert_eq!(
            banner_text("  Approval sent\nto your phone  "),
            "Approval sent to your phone"
        );
    }

    #[test]
    fn banner_text_drops_escapes() {
        assert_eq!(banner_text("bell\u{7}here"), "bellhere");
    }

    #[test]
    fn banner_text_caps_length() {
        assert_eq!(banner_text(&"x".repeat(500)).chars().count(), 200);
    }

    #[test]
    fn banner_text_blank_is_empty() {
        assert_eq!(banner_text("   \n\t "), "");
    }

    #[test]
    fn initial_session_prefers_remembered() {
        let sessions = vec![
            sessions::SessionEntry {
                name: "A".into(),
                cmd: vec!["a".into()],
                env: Vec::new(),
            },
            sessions::SessionEntry {
                name: "B".into(),
                cmd: vec!["b".into()],
                env: Vec::new(),
            },
        ];
        let state = vigil_config::State {
            user: String::new(),
            session: "B".into(),
        };
        assert_eq!(initial_session(&sessions, Some(&state), "A"), 1);
    }

    #[test]
    fn initial_session_uses_configured_default() {
        let sessions = test_sessions();
        assert_eq!(initial_session(&sessions, None, "B"), 1);
    }

    #[test]
    fn initial_session_ignores_stale_remembered() {
        let sessions = test_sessions();
        let state = vigil_config::State {
            user: String::new(),
            session: "Gone".into(),
        };
        assert_eq!(initial_session(&sessions, Some(&state), "B"), 1);
    }

    #[test]
    fn initial_session_falls_back_to_first() {
        assert_eq!(initial_session(&test_sessions(), None, ""), 0);
    }

    fn test_sessions() -> Vec<sessions::SessionEntry> {
        vec![
            sessions::SessionEntry {
                name: "A".into(),
                cmd: vec!["a".into()],
                env: Vec::new(),
            },
            sessions::SessionEntry {
                name: "B".into(),
                cmd: vec!["b".into()],
                env: Vec::new(),
            },
        ]
    }

    #[test]
    fn cli_overrides_config() {
        let cli = Cli {
            user: Some("kiosk".into()),
            socket: None,
            config: None,
            theme: Some("/cli.slint".into()),
            theme_check: None,
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
            theme_check: None,
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
            theme_check: None,
            background: None,
            bg_mode: None,
            cmd: Vec::new(),
        };
        let config = Config::default();
        let resolved = resolve(&cli, &config);
        assert!(resolved.power_enabled);
        assert_eq!(resolved.clock_format, "%H:%M");
        assert_eq!(resolved.user, None);
    }
}
