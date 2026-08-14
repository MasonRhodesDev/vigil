//! vigil-lock: session lockscreen sharing vigil's theme and auth seams
//! (DESIGN.md §12). Policy layer only: vigil-wayland owns the protocol,
//! vigil-pam owns authentication, vigil-ui/-theme own the scene.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vigil_config::Config;
use vigil_core::{
    AppearanceEvent, AuthEvent, AuthUi, BackgroundFit, ColorScheme, FrameTarget, InputEvent,
    LoginEvent, OutputId, OutputInfo, UiMessage,
};
use vigil_login::{AppearanceWatcher, LoginSession};
use vigil_pam::PamAttempt;
use vigil_theme::Theme;
use vigil_ui::{Looks, OutputWindow, UiSnapshot, VigilPlatform};
use vigil_wayland::{LockOutcome, LockSession};

struct Cli {
    user: String,
    config: Option<PathBuf>,
    theme: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: Option<BackgroundFit>,
    grace: Option<u64>,
    /// Write one byte here once the compositor confirms the lock (systemd/
    /// hypridle readiness protocol).
    ready_fd: Option<i32>,
    /// Re-exec as a background child and exit 0 only once it is locked —
    /// the blocking form a `before_sleep_cmd` needs: the caller's sleep
    /// inhibitor is not released until the screen is already secured.
    daemonize: bool,
}

fn parse_cli() -> Result<Cli, String> {
    let mut cli = Cli {
        user: whoami()?,
        config: None,
        theme: None,
        background: None,
        bg_mode: None,
        grace: None,
        ready_fd: None,
        daemonize: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--user" => cli.user = value("--user")?,
            "--config" => cli.config = Some(PathBuf::from(value("--config")?)),
            "--theme" => cli.theme = Some(PathBuf::from(value("--theme")?)),
            "--background" => cli.background = Some(PathBuf::from(value("--background")?)),
            "--bg-mode" => {
                let v = value("--bg-mode")?;
                cli.bg_mode = Some(BackgroundFit::parse(&v).ok_or(format!("unknown bg-mode {v}"))?);
            }
            "--grace" => {
                let v = value("--grace")?;
                cli.grace = Some(v.parse().map_err(|_| format!("bad --grace {v}"))?);
            }
            "--ready-fd" => {
                let v = value("--ready-fd")?;
                cli.ready_fd = Some(v.parse().map_err(|_| format!("bad --ready-fd {v}"))?);
            }
            "--daemonize" => cli.daemonize = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(cli)
}

/// The user to authenticate: the session owner, not a CLI guess.
fn whoami() -> Result<String, String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .map_err(|_| "cannot determine user (USER/LOGNAME unset); pass --user".into())
}

fn output_description(info: &OutputInfo) -> Option<String> {
    let value = [info.make.as_deref(), info.model.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!value.is_empty()).then_some(value)
}

fn appearance_fit(fit: appearance_profiles::Fit) -> BackgroundFit {
    match fit {
        appearance_profiles::Fit::Fill => BackgroundFit::Fill,
        appearance_profiles::Fit::Fit => BackgroundFit::Fit,
        appearance_profiles::Fit::Stretch => BackgroundFit::Stretch,
        appearance_profiles::Fit::Center => BackgroundFit::Center,
        appearance_profiles::Fit::Tile => BackgroundFit::Tile,
    }
}

struct Entry {
    id: OutputId,
    window: OutputWindow,
}

struct Locker {
    platform: VigilPlatform,
    theme: Theme,
    entries: Vec<Entry>,
    panel: usize,
    user: String,
    looks: Looks,
    appearance_registry: appearance_profiles::Registry,
    clock_format: String,
    caps_lock: bool,
    queue: Rc<std::cell::RefCell<VecDeque<UiMessage>>>,
    auth_rx: mpsc::Receiver<AuthEvent>,
    auth_tx: mpsc::Sender<AuthEvent>,
    attempt: Option<PamAttempt>,
    login: Option<LoginSession>,
    login_rx: mpsc::Receiver<LoginEvent>,
    login_tx: mpsc::Sender<LoginEvent>,
    appearance: Option<AppearanceWatcher>,
    appearance_rx: mpsc::Receiver<AppearanceEvent>,
    appearance_tx: mpsc::Sender<AppearanceEvent>,
    scheme: ColorScheme,
    accent: Option<(f32, f32, f32)>,
    unlocked: bool,
    last_clock: (Instant, String),
    ready_fd: Option<i32>,
    grace_secs: u64,
    grace: Option<Grace>,
    /// Auth state to replay onto scenes rebuilt mid-lock (resume/resize
    /// recreates outputs; a fresh theme instance starts blank).
    snapshot: UiSnapshot,
}

/// Grace window: unlock without auth shortly after locking. Two deadlines
/// on two clocks: Instant freezes during suspend while SystemTime does
/// not, so requiring BOTH keeps a pre-suspend grace from surviving into
/// the resume — the lock-before-sleep guarantee stays intact without
/// logind integration.
struct Grace {
    deadline_mono: Instant,
    deadline_wall: std::time::SystemTime,
}

impl Grace {
    fn new(secs: u64) -> Self {
        let secs = Duration::from_secs(secs);
        Self {
            deadline_mono: Instant::now() + secs,
            deadline_wall: std::time::SystemTime::now() + secs,
        }
    }

    /// Presses dismiss; motion and releases never do.
    fn dismisses(
        &self,
        event: &InputEvent,
        now_mono: Instant,
        now_wall: std::time::SystemTime,
    ) -> bool {
        let live = now_mono < self.deadline_mono && now_wall < self.deadline_wall;
        live && matches!(
            event,
            InputEvent::Key { pressed: true, .. } | InputEvent::PointerButton { pressed: true, .. }
        )
    }
}

/// What a [`LoginEvent`] means for the locker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginAction {
    /// Release the screen without authentication.
    Unlock,
    Ignore,
}

/// logind event → locker action. `Unlock` is honored without auth on
/// purpose: logind only accepts `UnlockSession` from root or the session's
/// own user, it is the escape hatch every locker implements, and a working
/// `loginctl unlock-session` is worth having when a locker misbehaves.
fn handle_login_event(event: LoginEvent, grace: &mut Option<Grace>) -> LoginAction {
    match event {
        LoginEvent::Unlock => LoginAction::Unlock,
        // We ARE the locker; the session is already locked.
        LoginEvent::Lock => LoginAction::Ignore,
        LoginEvent::PrepareForSleep(true) => {
            // A grace window must never survive a suspend (#9 residual).
            *grace = None;
            LoginAction::Ignore
        }
        LoginEvent::PrepareForSleep(false) => LoginAction::Ignore,
    }
}

impl Locker {
    fn new(cli: Cli, config: Config, grace_secs: u64) -> Result<Self, String> {
        let platform = VigilPlatform::install().map_err(|e| e.to_string())?;
        let theme = Theme::load_or_default(cli.theme.as_deref());
        let clock_format = config.look.clock_format.clone();
        let (auth_tx, auth_rx) = mpsc::channel();
        let (login_tx, login_rx) = mpsc::channel();
        let (appearance_tx, appearance_rx) = mpsc::channel();
        let locker = Self {
            platform,
            theme,
            entries: Vec::new(),
            panel: 0,
            user: cli.user,
            looks: Looks {
                cli_background: cli.background.clone(),
                fallback_background: std::env::var_os("WALLPAPER_PATH")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from),
                cli_fit: cli.bg_mode,
                config,
            },
            appearance_registry: appearance_profiles::Registry::load_current_user().unwrap_or_else(
                |e| {
                    eprintln!("vigil-lock: appearance registry: {e}");
                    Default::default()
                },
            ),
            clock_format: clock_format.clone(),
            caps_lock: false,
            queue: Rc::default(),
            auth_rx,
            auth_tx,
            attempt: None,
            login: LoginSession::connect(),
            login_rx,
            login_tx,
            appearance: AppearanceWatcher::connect(),
            appearance_rx,
            appearance_tx,
            scheme: ColorScheme::default(),
            accent: None,
            unlocked: false,
            last_clock: (Instant::now(), clock_text(&clock_format)),
            ready_fd: cli.ready_fd,
            grace_secs,
            grace: None,
            snapshot: {
                // Show a usable password prompt from the first frame: with
                // pam_fprintd in the stack the real prompt only arrives
                // after fingerprint resolves, and typed responses buffer
                // until PAM asks — the card must not sit there blank.
                let mut snapshot = UiSnapshot::default();
                snapshot.on_prompt("Password", true);
                snapshot
            },
        };
        if let Some(login) = &locker.login {
            login.spawn_signals(locker.login_tx.clone());
            login.spawn_sleep_signals(locker.login_tx.clone());
        }
        if let Some(appearance) = &locker.appearance {
            appearance.read_initial(&locker.appearance_tx);
            appearance.spawn_signals(locker.appearance_tx.clone());
        }
        Ok(locker)
    }

    fn start_attempt(&mut self) {
        let tx = self.auth_tx.clone();
        self.attempt = Some(PamAttempt::start(&self.user, move |event| {
            let _ = tx.send(event);
        }));
    }

    fn apply_panel(&mut self) {
        for (i, e) in self.entries.iter_mut().enumerate() {
            e.window.set_panel_visible(i == self.panel);
            // Hyprland does not draw a cursor over lock surfaces a client
            // that sets no cursor image — the software cursor covers it.
            e.window.set_cursor_visible(i == self.panel);
        }
    }

    fn each_window(&mut self, f: impl Fn(&mut OutputWindow)) {
        for e in self.entries.iter_mut() {
            f(&mut e.window);
        }
    }

    fn pump_auth(&mut self) {
        while let Ok(event) = self.auth_rx.try_recv() {
            match event {
                AuthEvent::Prompt { text, secret } => {
                    self.snapshot.on_prompt(&text, secret);
                    self.each_window(|w| w.show_prompt(&text, secret));
                }
                AuthEvent::Info(text) => {
                    self.snapshot.info = text.clone();
                    self.each_window(|w| w.show_info(&text));
                }
                AuthEvent::Error(text) => {
                    self.snapshot.error = text.clone();
                    self.each_window(|w| w.show_error(&text));
                }
                AuthEvent::Done(Ok(())) => {
                    self.unlock_now();
                    return;
                }
                AuthEvent::Done(Err(message)) => {
                    self.snapshot.error = message.clone();
                    self.snapshot.busy = false;
                    self.each_window(|w| {
                        w.show_error(&message);
                        w.set_busy(false);
                    });
                    // A fresh PAM transaction per attempt (hyprlock's model):
                    // the new conversation re-prompts.
                    self.start_attempt();
                }
            }
        }
    }

    fn pump_ui(&mut self) {
        loop {
            let msg = self.queue.borrow_mut().pop_front();
            let Some(msg) = msg else { break };
            match msg {
                UiMessage::Respond(text) => {
                    if self.attempt.is_some() {
                        self.snapshot.busy = true;
                        self.each_window(|w| w.set_busy(true));
                    }
                    if let Some(attempt) = &self.attempt {
                        attempt.respond(text);
                    }
                }
                UiMessage::Cancel => {
                    if let Some(attempt) = &mut self.attempt {
                        attempt.cancel();
                    }
                }
                // A locker has no session picker; power actions are policy
                // for L2.
                // A locker has no session or user picker; power actions are
                // policy for L2.
                UiMessage::SelectSession(_) | UiMessage::SelectUser(_) | UiMessage::Power(_) => {}
            }
        }
    }

    fn pump_login(&mut self) {
        while let Ok(event) = self.login_rx.try_recv() {
            if handle_login_event(event, &mut self.grace) == LoginAction::Unlock {
                self.unlock_now();
            }
        }
    }

    /// Portal appearance → theme properties, cached so scenes rebuilt later
    /// (resume, hotplug, resize) come up with the same look.
    fn pump_appearance(&mut self) {
        while let Ok(event) = self.appearance_rx.try_recv() {
            match event {
                AppearanceEvent::Scheme(scheme) => {
                    self.scheme = scheme;
                    let text = scheme.as_theme_str();
                    self.each_window(|w| w.set_color_scheme(text));
                }
                AppearanceEvent::Accent(accent) => {
                    self.accent = accent;
                    // Unset leaves the theme's own default binding intact.
                    if let Some(rgb) = accent {
                        self.each_window(|w| w.set_accent_color(rgb));
                    }
                }
            }
        }
    }

    /// Single exit path: clear the logind hint, then release the screen.
    fn unlock_now(&mut self) {
        if let Some(login) = &self.login {
            login.set_locked_hint(false);
        }
        self.unlocked = true;
    }
}

impl LockSession for Locker {
    fn output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        let build = || -> Result<OutputWindow, String> {
            let component = self.theme.instantiate().map_err(|e| e.to_string())?;
            let adapter = self
                .platform
                .claim_last_adapter()
                .ok_or("no adapter captured")?;
            let mut window =
                OutputWindow::new(id, info.width, info.height, info.scale, adapter, component)
                    .map_err(|e| e.to_string())?;
            let resolved = self.appearance_registry.resolve(
                &appearance_profiles::OutputIdentity::new(
                    &info.connector,
                    output_description(info),
                ),
                None,
            );
            let (background, fit) = self.looks.for_connector_with_fallback(
                &info.connector,
                resolved.path,
                Some(appearance_fit(resolved.fit)),
            );
            if let Some(path) = &background {
                match vigil_ui::background(path, fit, info.width, info.height) {
                    Ok(rgba) => window.set_background(rgba, info.width, info.height),
                    Err(e) => eprintln!("vigil-lock: background: {e}"),
                }
            }
            window.set_clock(&self.last_clock.1);
            window.set_caps_lock(self.caps_lock);
            window.set_panel_visible(false);
            window.set_user_name(&self.user);
            window.set_color_scheme(self.scheme.as_theme_str());
            if let Some(rgb) = self.accent {
                window.set_accent_color(rgb);
            }
            self.snapshot.apply(&mut window);
            let queue = self.queue.clone();
            window.on_ui_message(Rc::new(move |m| queue.borrow_mut().push_back(m)));
            Ok(window)
        };
        match build() {
            Ok(window) => {
                eprintln!(
                    "vigil-lock: output {} {}x{}@{:.2}",
                    info.connector, info.width, info.height, info.scale
                );
                self.entries.push(Entry { id, window });
                self.apply_panel();
            }
            Err(e) => eprintln!("vigil-lock: skipping output {id:?}: {e}"),
        }
    }

    fn output_resized(&mut self, id: OutputId, info: &OutputInfo) {
        // Simplest correct handling: rebuild the scene at the new geometry.
        self.entries.retain(|e| e.id != id);
        self.output_ready(id, info);
    }

    fn output_gone(&mut self, id: OutputId) {
        self.entries.retain(|e| e.id != id);
        if self.panel >= self.entries.len() {
            self.panel = 0;
        }
        self.apply_panel();
    }

    fn focus_output(&mut self, id: OutputId) {
        if let Some(idx) = self.entries.iter().position(|e| e.id == id)
            && idx != self.panel
        {
            self.panel = idx;
            self.apply_panel();
        }
    }

    fn input(&mut self, event: InputEvent) {
        if let Some(grace) = &self.grace
            && grace.dismisses(&event, Instant::now(), std::time::SystemTime::now())
        {
            // Dismissed inside the grace window: unlock and swallow the
            // event so the keystroke never reaches a PAM response.
            self.unlock_now();
            return;
        }
        if let Some(e) = self.entries.get_mut(self.panel) {
            e.window.dispatch(event);
        }
    }

    fn caps_lock(&mut self, on: bool) {
        if self.caps_lock != on {
            self.caps_lock = on;
            self.each_window(|w| w.set_caps_lock(on));
        }
    }

    fn locked(&mut self) {
        eprintln!("vigil-lock: session locked");
        // Readiness: the compositor holds the lock from this moment — it
        // will never reveal the session again without unlock_and_destroy,
        // even if we have not painted yet. Safe to let a suspend proceed.
        if let Some(fd) = self.ready_fd.take() {
            use std::io::Write;
            use std::os::fd::FromRawFd;
            let mut ready = unsafe { std::fs::File::from_raw_fd(fd) };
            let _ = ready.write_all(b"1");
            // Dropping closes the fd; a --daemonize parent unblocks on
            // either the byte or the EOF.
        }
        if self.grace_secs > 0 {
            self.grace = Some(Grace::new(self.grace_secs));
        }
        if let Some(login) = &self.login {
            login.set_locked_hint(true);
        }
    }

    fn tick(&mut self) {
        vigil_ui::advance_timers();
        if self.last_clock.0.elapsed() >= Duration::from_secs(1) {
            let text = clock_text(&self.clock_format);
            if text != self.last_clock.1 {
                self.each_window(|w| w.set_clock(&text));
            }
            self.last_clock = (Instant::now(), text);
        }
        self.pump_auth();
        self.pump_login();
        self.pump_appearance();
        self.pump_ui();
    }

    fn render(&mut self, id: OutputId, target: FrameTarget<'_>) -> bool {
        self.entries
            .iter_mut()
            .find(|e| e.id == id)
            .map(|e| e.window.render_if_needed(target))
            .unwrap_or(false)
    }

    fn wants_unlock(&self) -> bool {
        self.unlocked
    }
}

fn clock_text(format: &str) -> String {
    std::process::Command::new("date")
        .arg(format!("+{format}"))
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The blocking half of `--daemonize`: spawn the real locker as a child
/// whose stdout is our socketpair, and return only when it reports locked
/// (one byte) or dies first (EOF). Exit 0 here == the session IS locked.
fn daemonize() -> ! {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let result = (|| -> Result<i32, String> {
        let (mut parent_end, child_end) =
            UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut child = std::process::Command::new(exe)
            .args(std::env::args().skip(1).filter(|a| a != "--daemonize"))
            .args(["--ready-fd", "1"])
            .stdout(std::process::Stdio::from(OwnedFd::from(child_end)))
            .spawn()
            .map_err(|e| format!("spawn locker: {e}"))?;
        let mut byte = [0u8; 1];
        match parent_end.read_exact(&mut byte) {
            Ok(()) => Ok(0),
            Err(_) => {
                // EOF without the ready byte: the locker died before the
                // compositor confirmed the lock. Propagate its exit code so
                // the caller knows the screen is NOT secured.
                let status = child.wait().map_err(|e| format!("wait locker: {e}"))?;
                Ok(status.code().unwrap_or(1).max(1))
            }
        }
    })();
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("vigil-lock: daemonize: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let mut cli = match parse_cli() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            std::process::exit(2);
        }
    };
    if cli.daemonize {
        daemonize();
    }
    let config = Config::load_layered(cli.config.as_deref());
    cli.theme = cli.theme.or(config.look.theme.clone());
    let grace_secs = cli.grace.unwrap_or(config.lock.grace_secs);
    let mut locker = match Locker::new(cli, config, grace_secs) {
        Ok(locker) => locker,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            std::process::exit(1);
        }
    };
    locker.start_attempt();
    match vigil_wayland::run(locker) {
        Ok(LockOutcome::Unlocked) => std::process::exit(0),
        Ok(LockOutcome::Denied) => {
            eprintln!("vigil-lock: lock denied (another locker running?)");
            std::process::exit(2);
        }
        Ok(LockOutcome::Invalidated) => {
            eprintln!("vigil-lock: lock invalidated by the compositor");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn key() -> InputEvent {
        InputEvent::Key {
            keysym: 'a' as u32,
            utf8: Some("a".into()),
            pressed: true,
        }
    }

    #[test]
    fn press_inside_window_dismisses() {
        assert!(Grace::new(5).dismisses(&key(), Instant::now(), SystemTime::now()));
        assert!(Grace::new(5).dismisses(
            &InputEvent::PointerButton {
                button: 0x110,
                pressed: true,
            },
            Instant::now(),
            SystemTime::now(),
        ));
    }

    #[test]
    fn expired_window_never_dismisses() {
        assert!(!Grace::new(5).dismisses(
            &key(),
            Instant::now() + Duration::from_secs(6),
            SystemTime::now() + Duration::from_secs(6),
        ));
    }

    #[test]
    fn wall_clock_jump_kills_grace() {
        assert!(!Grace::new(5).dismisses(
            &key(),
            Instant::now(),
            SystemTime::now() + Duration::from_secs(6),
        ));
    }

    #[test]
    fn motion_and_releases_never_dismiss() {
        let events = [
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerAbsolute { x: 0.5, y: 0.5 },
            InputEvent::Key {
                keysym: 'a' as u32,
                utf8: Some("a".into()),
                pressed: false,
            },
            InputEvent::PointerButton {
                button: 0x110,
                pressed: false,
            },
        ];
        for event in events {
            assert!(!Grace::new(5).dismisses(&event, Instant::now(), SystemTime::now()));
        }
    }

    #[test]
    fn zero_grace_never_dismisses() {
        assert!(!Grace::new(0).dismisses(&key(), Instant::now(), SystemTime::now()));
    }

    #[test]
    fn unlock_signal_releases_without_auth() {
        assert_eq!(
            handle_login_event(LoginEvent::Unlock, &mut None),
            LoginAction::Unlock
        );
    }

    #[test]
    fn lock_signal_is_a_noop_while_locked() {
        let mut grace = Some(Grace::new(5));
        assert_eq!(
            handle_login_event(LoginEvent::Lock, &mut grace),
            LoginAction::Ignore
        );
        assert!(grace.is_some());
    }

    #[test]
    fn sleep_invalidates_grace() {
        let mut grace = Some(Grace::new(60));
        assert_eq!(
            handle_login_event(LoginEvent::PrepareForSleep(true), &mut grace),
            LoginAction::Ignore
        );
        assert!(grace.is_none());
    }

    #[test]
    fn resume_leaves_grace_alone() {
        let mut grace = Some(Grace::new(60));
        handle_login_event(LoginEvent::PrepareForSleep(false), &mut grace);
        assert!(grace.is_some());
    }

    #[test]
    fn invalidated_grace_never_dismisses() {
        let mut grace = Some(Grace::new(60));
        handle_login_event(LoginEvent::PrepareForSleep(true), &mut grace);
        assert!(grace.is_none());
        assert!(!Grace::new(0).dismisses(&key(), Instant::now(), SystemTime::now()));
    }
}
