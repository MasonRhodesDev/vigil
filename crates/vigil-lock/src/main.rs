//! vigil-lock: session lockscreen sharing vigil's theme and auth seams
//! (DESIGN.md §12). Policy layer only: vigil-wayland owns the protocol,
//! vigil-pam owns authentication, vigil-ui/-theme own the scene.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use hypr_slint_runtime::{DirtySet, IdleScheduler, Metrics, WaitDecision, WakeHandle};
use vigil_config::Config;
use vigil_core::{
    AppearanceEvent, AuthEvent, AuthUi, BackgroundFit, ColorScheme, FrameTarget, InputEvent,
    LoginEvent, OutputId, OutputInfo, UiMessage,
};
use vigil_login::{AppearanceWatcher, LoginSession};
use vigil_pam::PamAttempt;
use vigil_theme::Theme;
use vigil_ui::{Looks, OutputWindow, UiSnapshot, VigilPlatform, apply_kit_tokens_from_disk};
use vigil_warning::ElementSample;
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
    warning_ms: Option<u64>,
}

fn clock_interval(format: &str) -> Duration {
    if ["%S", "%T", "%X", "%r"]
        .iter()
        .any(|token| format.contains(token))
    {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(60)
    }
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
        warning_ms: None,
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
            "--warn" => {
                let v = value("--warn")?;
                let seconds: f64 = v.parse().map_err(|_| format!("bad --warn {v}"))?;
                cli.warning_ms = Some((seconds.max(0.0) * 1000.0).round() as u64);
            }
            "--no-warn" => cli.warning_ms = Some(0),
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
    connector: String,
    description: String,
    window: OutputWindow,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BackgroundKey {
    path: PathBuf,
    fit: BackgroundFit,
    width: u32,
    height: u32,
}

#[derive(Clone)]
enum BackgroundData {
    Rgba(Arc<Vec<u8>>),
    Xrgb(Arc<[u8]>),
}

type BackgroundPixels = Result<BackgroundData, String>;
type SourceImage = Result<Arc<vigil_ui::BackgroundImage>, String>;

struct BackgroundResult {
    id: OutputId,
    key: BackgroundKey,
    pixels: BackgroundPixels,
    elapsed: Duration,
    cache_hit: bool,
}

/// Process-lifetime wallpaper cache and renderer. Expensive image work never
/// runs on the Wayland thread; OnceLock coalesces equal source decodes and
/// equal output-sized renders when outputs arrive together.
struct BackgroundWorker {
    tx: mpsc::Sender<BackgroundResult>,
    sources: Arc<Mutex<HashMap<PathBuf, Arc<OnceLock<SourceImage>>>>>,
    rendered: Arc<Mutex<HashMap<BackgroundKey, Arc<OnceLock<BackgroundPixels>>>>>,
}

impl BackgroundWorker {
    fn new(tx: mpsc::Sender<BackgroundResult>) -> Self {
        Self {
            tx,
            sources: Arc::default(),
            rendered: Arc::default(),
        }
    }

    fn render_prepared(
        &self,
        id: OutputId,
        key: BackgroundKey,
        prepared: Option<appearance_profiles::PreparedBackground>,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
    ) {
        let tx = self.tx.clone();
        let sources = self.sources.clone();
        let rendered = self.rendered.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let cell = rendered
                .lock()
                .expect("background render cache poisoned")
                .entry(key.clone())
                .or_default()
                .clone();
            let cache_hit = cell.get().is_some();
            let pixels = cell
                .get_or_init(|| {
                    if let Some(prepared) = prepared.as_ref() {
                        let read_started = Instant::now();
                        let result =
                            appearance_profiles::read_prepared_asset(prepared).map(|asset| {
                                match asset.format {
                                    appearance_profiles::PixelFormat::Rgba8 => {
                                        BackgroundData::Rgba(Arc::new(asset.bytes))
                                    }
                                    appearance_profiles::PixelFormat::Xrgb8888Le => {
                                        BackgroundData::Xrgb(asset.bytes.into())
                                    }
                                }
                            });
                        eprintln!(
                            "vigil-lock: prepared background read {}: {:?}",
                            prepared.asset.display(),
                            read_started.elapsed()
                        );
                        return result.map_err(|error| error.to_string());
                    }
                    let source_cell = sources
                        .lock()
                        .expect("background source cache poisoned")
                        .entry(key.path.clone())
                        .or_default()
                        .clone();
                    let source = source_cell.get_or_init(|| {
                        let decode_started = Instant::now();
                        let result = vigil_ui::load_background(&key.path).map(Arc::new);
                        eprintln!(
                            "vigil-lock: background decode {}: {:?}",
                            key.path.display(),
                            decode_started.elapsed()
                        );
                        result
                    });
                    source.as_ref().map_err(Clone::clone).and_then(|source| {
                        let render_started = Instant::now();
                        let result =
                            vigil_ui::render_background(source, key.fit, key.width, key.height)
                                .map(|rgba| BackgroundData::Rgba(Arc::new(rgba)));
                        eprintln!(
                            "vigil-lock: background resize {}x{}: {:?}",
                            key.width,
                            key.height,
                            render_started.elapsed()
                        );
                        result
                    })
                })
                .clone();
            let _ = tx.send(BackgroundResult {
                id,
                key,
                pixels,
                elapsed: started.elapsed(),
                cache_hit,
            });
            if let Some(waker) = waker {
                waker();
            }
        });
    }
}

struct Locker {
    platform: VigilPlatform,
    theme: Theme,
    entries: Vec<Entry>,
    panel: usize,
    user: String,
    looks: Looks,
    appearance_registry: appearance_profiles::Registry,
    appearance_bundle: Option<appearance_profiles::PreparedBundle>,
    monitor_profiles: Vec<monitor_profiles::Profile>,
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
    unlocked: bool,
    last_clock: (Instant, String),
    ready_fd: Option<i32>,
    grace_secs: u64,
    grace: Option<Grace>,
    /// Auth state to replay onto scenes rebuilt mid-lock (resume/resize
    /// recreates outputs; a fresh theme instance starts blank).
    snapshot: UiSnapshot,
    background_worker: BackgroundWorker,
    background_rx: mpsc::Receiver<BackgroundResult>,
    background_waker: Option<Arc<dyn Fn() + Send + Sync>>,
    event_waker: Arc<Mutex<Option<WakeHandle>>>,
    scheduler: IdleScheduler,
    warning_active: bool,
    warning_commit: bool,
    warning_ipc: Arc<Mutex<WarningIpcState>>,
    warning_backgrounds: std::collections::HashSet<OutputId>,
}

fn forwarded_channel<T: Send + 'static>(
    waker: Arc<Mutex<Option<WakeHandle>>>,
) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    let (source_tx, source_rx) = mpsc::channel();
    let (app_tx, app_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(event) = source_rx.recv() {
            if app_tx.send(event).is_err() {
                break;
            }
            if let Some(wake) = waker.lock().expect("event waker poisoned").as_ref() {
                wake.wake();
            }
        }
    });
    (source_tx, app_rx)
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
    fn new(
        cli: Cli,
        config: Config,
        grace_secs: u64,
        warning_ipc: Arc<Mutex<WarningIpcState>>,
    ) -> Result<Self, String> {
        let warning_active = config.lock.warning.duration_ms > 0;
        let platform = VigilPlatform::install().map_err(|e| e.to_string())?;
        let theme = Theme::load_or_default(cli.theme.as_deref());
        let clock_format = config.look.clock_format.clone();
        let event_waker = Arc::new(Mutex::new(None));
        let (auth_tx, auth_rx) = mpsc::channel();
        let (login_tx, login_rx) = forwarded_channel(event_waker.clone());
        let (appearance_tx, appearance_rx) = forwarded_channel(event_waker.clone());
        let monitor_profiles = config
            .profiles
            .dir
            .as_deref()
            .map(monitor_profiles::load_dir)
            .map(|(profiles, diagnostics)| {
                for diagnostic in diagnostics {
                    eprintln!("vigil-lock: monitor profile: {diagnostic:?}");
                }
                profiles
            })
            .unwrap_or_default();
        let (background_tx, background_rx) = mpsc::channel();
        let appearance_bundle = appearance_profiles::PreparedBundle::load_published(&cli.user)
            .unwrap_or_else(|error| {
                eprintln!("vigil-lock: prepared appearance bundle: {error}");
                None
            });
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
            appearance_bundle,
            monitor_profiles,
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
            background_worker: BackgroundWorker::new(background_tx),
            background_rx,
            background_waker: None,
            event_waker,
            scheduler: IdleScheduler::default(),
            warning_active,
            warning_commit: false,
            warning_ipc,
            warning_backgrounds: std::collections::HashSet::new(),
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
        let waker = self.event_waker.clone();
        self.attempt = Some(PamAttempt::start(&self.user, move |event| {
            let _ = tx.send(event);
            if let Some(wake) = waker.lock().expect("event waker poisoned").as_ref() {
                wake.wake();
            }
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
            if self.warning_active
                && matches!(event, LoginEvent::Lock | LoginEvent::PrepareForSleep(true))
            {
                self.warning_commit = true;
            }
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
                    self.retint();
                }
                AppearanceEvent::Accent(_) => {
                    // `lmtt switch` writes tokens then the portal; re-read LMTT.
                    self.retint();
                }
            }
        }
    }

    fn focus_profile_origin(&mut self) {
        let signature: Vec<_> = self
            .entries
            .iter()
            .map(|entry| entry.description.clone())
            .collect();
        let connected: Vec<_> = self
            .entries
            .iter()
            .map(|entry| monitor_profiles::ConnectedOutput {
                name: entry.connector.clone(),
                description: entry.description.clone(),
            })
            .collect();
        let Some(profile) = monitor_profiles::select(&signature, &self.monitor_profiles) else {
            return;
        };
        let resolved = monitor_profiles::resolve(profile, &connected);
        let Some(origin) = resolved
            .outputs
            .iter()
            .find(|output| output.position == (0, 0))
        else {
            return;
        };
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.connector == origin.name)
        {
            self.panel = index;
        }
    }

    fn retint(&mut self) {
        let mode = self.scheme.as_theme_str();
        self.each_window(|w| apply_kit_tokens_from_disk(w, mode));
    }

    /// Single exit path: clear the logind hint, then release the screen.
    fn unlock_now(&mut self) {
        if let Some(login) = &self.login {
            login.set_locked_hint(false);
        }
        self.unlocked = true;
    }

    fn pump_backgrounds(&mut self) {
        while let Ok(result) = self.background_rx.try_recv() {
            self.warning_backgrounds.remove(&result.id);
            let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == result.id) else {
                continue;
            };
            if entry.window.scene_size() != (result.key.width, result.key.height) {
                eprintln!(
                    "vigil-lock: discarding stale background for {} ({}x{})",
                    entry.connector, result.key.width, result.key.height
                );
                continue;
            }
            match result.pixels {
                Ok(BackgroundData::Rgba(rgba)) => {
                    entry
                        .window
                        .set_background_pixels(&rgba, result.key.width, result.key.height);
                    eprintln!(
                        "vigil-lock: background ready for {} in {:?}{}",
                        entry.connector,
                        result.elapsed,
                        if result.cache_hit { " (cache hit)" } else { "" }
                    );
                }
                Ok(BackgroundData::Xrgb(xrgb)) => {
                    if !entry.window.set_native_background_xrgb(
                        xrgb,
                        result.key.width,
                        result.key.height,
                    ) {
                        eprintln!(
                            "vigil-lock: native background unsupported for {}; using fallback",
                            entry.connector
                        );
                        continue;
                    }
                    eprintln!(
                        "vigil-lock: native background ready for {} in {:?}{}",
                        entry.connector,
                        result.elapsed,
                        if result.cache_hit { " (cache hit)" } else { "" }
                    );
                }
                Err(error) => eprintln!("vigil-lock: background: {error}"),
            }
        }
    }
}

impl LockSession for Locker {
    fn set_runtime(
        &mut self,
        wake: WakeHandle,
        dirty: Arc<DirtySet<OutputId>>,
        metrics: Arc<Metrics>,
    ) {
        self.platform.set_runtime(wake.clone(), dirty, metrics);
        self.warning_ipc.lock().expect("warning IPC poisoned").waker = Some(wake.clone());
        *self.event_waker.lock().expect("event waker poisoned") = Some(wake.clone());
        self.background_waker = Some(Arc::new(move || {
            wake.wake();
        }));
    }

    fn output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        let scene_started = Instant::now();
        self.platform.set_next_output(id);
        let component = self.theme.instantiate();
        self.platform.clear_next_output();
        let component = match component {
            Ok(component) => component,
            Err(e) => {
                eprintln!("vigil-lock: skipping output {id:?}: {e}");
                return;
            }
        };
        let Some(adapter) = self.platform.claim_last_adapter() else {
            eprintln!("vigil-lock: skipping output {id:?}: no adapter captured");
            return;
        };
        let mut window =
            match OutputWindow::new(id, info.width, info.height, info.scale, adapter, component) {
                Ok(window) => window,
                Err(e) => {
                    eprintln!("vigil-lock: skipping output {id:?}: {e}");
                    return;
                }
            };
        let identity =
            appearance_profiles::OutputIdentity::new(&info.connector, info.description());
        let resolved = self.appearance_registry.resolve(&identity, None);
        let resolved_path = resolved.path.clone();
        let resolved_fit = resolved.fit;
        let (background, fit) = self.looks.for_connector_with_fallback(
            &info.connector,
            resolved.path,
            Some(appearance_fit(resolved.fit)),
        );
        window.set_clock(&self.last_clock.1);
        window.set_caps_lock(self.caps_lock);
        window.set_panel_visible(false);
        window.set_user_name(&self.user);
        apply_kit_tokens_from_disk(&mut window, self.scheme.as_theme_str());
        self.snapshot.apply(&mut window);
        let native_background_supported = window.supports_native_background();
        let queue = self.queue.clone();
        window.on_ui_message(Rc::new(move |m| queue.borrow_mut().push_back(m)));
        eprintln!(
            "vigil-lock: output {} {}x{}@{:.2}",
            info.connector, info.width, info.height, info.scale
        );
        self.entries.push(Entry {
            id,
            connector: info.connector.clone(),
            description: info.description().unwrap_or_else(|| info.connector.clone()),
            window,
        });
        self.focus_profile_origin();
        self.apply_panel();
        eprintln!(
            "vigil-lock: scene ready for {} in {:?}",
            info.connector,
            scene_started.elapsed()
        );
        if let Some(path) = background {
            self.warning_backgrounds.insert(id);
            let prepared = (Some(&path) == resolved_path.as_ref()
                && fit == appearance_fit(resolved_fit))
            .then(|| {
                self.appearance_bundle
                    .as_ref()
                    .and_then(|bundle| {
                        bundle.resolve(&identity, info.width, info.height, resolved_fit)
                    })
                    .cloned()
            })
            .flatten()
            .filter(|prepared| {
                prepared.format != appearance_profiles::PixelFormat::Xrgb8888Le
                    || native_background_supported
            });
            self.background_worker.render_prepared(
                id,
                BackgroundKey {
                    path,
                    fit,
                    width: info.width,
                    height: info.height,
                },
                prepared,
                self.background_waker.clone(),
            );
        }
    }

    fn warning_output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_ready(id, info);
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            for selector in vigil_warning::DEFAULT_SELECTORS {
                entry.window.set_warning_element(selector, 0.0);
            }
        }
    }

    fn output_resized(&mut self, id: OutputId, info: &OutputInfo) {
        // Simplest correct handling: rebuild the scene at the new geometry.
        self.entries.retain(|e| e.id != id);
        self.output_ready(id, info);
    }

    fn output_rebound(&mut self, id: OutputId, _info: &OutputInfo) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.window.request_present();
        }
    }

    fn output_gone(&mut self, id: OutputId) {
        if let Some(e) = self.entries.iter().find(|e| e.id == id) {
            eprintln!("vigil-lock: output {} gone", e.connector);
        }
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

    fn warning_progress(&mut self, _frost: f32, _wallpaper: f32) {
        // Surface opacity and compositor frost are owned by vigil-wayland.
        // The lock scene contributes only its wallpaper during this phase.
        self.each_window(|window| {
            window.set_panel_visible(false);
            window.set_cursor_visible(false);
            window.request_present();
        });
    }

    fn warning_elements(&mut self, elements: &[ElementSample]) {
        self.each_window(|window| {
            for element in elements {
                window.set_warning_element(&element.selector, element.progress);
            }
        });
    }

    fn warning_commit_requested(&mut self) -> bool {
        let ipc_commit = {
            let mut ipc = self.warning_ipc.lock().expect("warning IPC poisoned");
            std::mem::take(&mut ipc.commit_requested)
        };
        std::mem::take(&mut self.warning_commit) || ipc_commit
    }

    fn warning_wallpaper_ready(&self) -> bool {
        !self.entries.is_empty() && self.warning_backgrounds.is_empty()
    }

    fn caps_lock(&mut self, on: bool) {
        if self.caps_lock != on {
            self.caps_lock = on;
            self.each_window(|w| w.set_caps_lock(on));
        }
    }

    fn locked(&mut self) {
        eprintln!("vigil-lock: session locked");
        self.warning_active = false;
        let joiners = {
            let mut ipc = self.warning_ipc.lock().expect("warning IPC poisoned");
            ipc.locked = true;
            std::mem::take(&mut ipc.joiners)
        };
        for mut joiner in joiners {
            let _ = joiner.write_all(b"locked\n");
        }
        self.apply_panel();
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
        // Auth belongs on a held lock. Starting PAM in main() made every
        // denied second locker (hypridle re-lock) log a failed conversation.
        if self.attempt.is_none() {
            self.start_attempt();
        }
    }

    fn tick(&mut self) {
        vigil_ui::advance_timers();
        if self.last_clock.0.elapsed() >= clock_interval(&self.clock_format) {
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
        self.pump_backgrounds();
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

    fn wait_decision(&self) -> WaitDecision {
        let slint = self
            .scheduler
            .from_slint(self.entries.iter().map(|entry| entry.window.slint_window()));
        let clock = clock_interval(&self.clock_format).saturating_sub(self.last_clock.0.elapsed());
        match slint {
            WaitDecision::Frame(delay) => WaitDecision::Frame(delay.min(clock)),
            WaitDecision::Timer(delay) => WaitDecision::Timer(delay.min(clock)),
            WaitDecision::Indefinite => WaitDecision::Timer(clock),
        }
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
    use std::os::unix::process::CommandExt;

    let result = (|| -> Result<i32, String> {
        let (mut parent_end, child_end) =
            UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut command = std::process::Command::new(exe);
        command
            .args(std::env::args().skip(1).filter(|a| a != "--daemonize"))
            .args(["--ready-fd", "1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(OwnedFd::from(child_end)));
        // SAFETY: this hook runs in the forked child before exec and calls only
        // the async-signal-safe setsid(2). A new child is not a process-group
        // leader, so it can create a session and detach from the caller's TTY.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().map_err(|e| format!("spawn locker: {e}"))?;
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

struct WarningSocket {
    path: PathBuf,
}

#[derive(Default)]
struct WarningIpcState {
    locked: bool,
    commit_requested: bool,
    joiners: Vec<UnixStream>,
    waker: Option<WakeHandle>,
}

impl Drop for WarningSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn warning_socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("vigil-lock-warning.sock"))
}

/// Join an already-running warning. Success means the compositor has
/// confirmed session-lock, not merely that the commit request was accepted.
fn join_warning() -> Result<bool, String> {
    let Some(path) = warning_socket_path() else {
        return Ok(false);
    };
    join_warning_at(&path)
}

fn join_warning_at(path: &std::path::Path) -> Result<bool, String> {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(format!("join warning {}: {error}", path.display())),
    };
    stream
        .write_all(b"commit\n")
        .map_err(|error| format!("request warning commit: {error}"))?;
    let mut response = [0_u8; 7];
    stream
        .read_exact(&mut response)
        .map_err(|error| format!("wait for warning lock: {error}"))?;
    Ok(response == *b"locked\n")
}

fn start_warning_ipc() -> Result<(WarningSocket, Arc<Mutex<WarningIpcState>>), String> {
    let path = warning_socket_path().ok_or("XDG_RUNTIME_DIR is unset")?;
    start_warning_ipc_at(path)
}

fn start_warning_ipc_at(
    path: PathBuf,
) -> Result<(WarningSocket, Arc<Mutex<WarningIpcState>>), String> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(format!(
                "warning socket {} is already active",
                path.display()
            ));
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("bind warning socket {}: {error}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure warning socket {}: {error}", path.display()))?;
    let state = Arc::new(Mutex::new(WarningIpcState::default()));
    let server_state = state.clone();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut command = [0_u8; 7];
            if stream.read_exact(&mut command).is_ok() && command == *b"commit\n" {
                let mut state = server_state.lock().expect("warning IPC poisoned");
                if state.locked {
                    let _ = stream.write_all(b"locked\n");
                } else {
                    eprintln!("vigil-lock: warning join requested immediate lock");
                    state.commit_requested = true;
                    state.joiners.push(stream);
                    if let Some(waker) = state.waker.clone() {
                        waker.wake();
                    }
                }
            }
        }
    });
    Ok((WarningSocket { path }, state))
}

fn signal_ready_fd(ready_fd: Option<i32>) {
    if let Some(fd) = ready_fd {
        use std::os::fd::FromRawFd;
        let mut ready = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = ready.write_all(b"1");
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
    let mut config = Config::load_layered(cli.config.as_deref());
    if let Some(duration_ms) = cli.warning_ms {
        config.lock.warning.duration_ms = duration_ms;
    }
    if let Err(error) = config.validate_warning() {
        eprintln!("vigil-lock: {error}");
        std::process::exit(2);
    }
    match join_warning() {
        Ok(true) => {
            signal_ready_fd(cli.ready_fd.take());
            std::process::exit(0);
        }
        Ok(false) => {}
        Err(error) => eprintln!("vigil-lock: {error}; starting a new lock"),
    }
    let mut warning = config.lock.warning.clone();
    cli.theme = cli.theme.or(config.look.theme.clone());
    let grace_secs = cli.grace.unwrap_or(config.lock.grace_secs);
    let (warning_socket, warning_ipc) = if warning.duration_ms > 0 {
        match start_warning_ipc() {
            Ok((guard, state)) => (Some(guard), state),
            Err(error) => {
                if let Ok(true) = join_warning() {
                    signal_ready_fd(cli.ready_fd.take());
                    std::process::exit(0);
                }
                eprintln!("vigil-lock: warning IPC unavailable: {error}; locking immediately");
                warning.duration_ms = 0;
                (None, Arc::new(Mutex::new(WarningIpcState::default())))
            }
        }
    } else {
        (None, Arc::new(Mutex::new(WarningIpcState::default())))
    };
    let locker = match Locker::new(cli, config, grace_secs, warning_ipc) {
        Ok(locker) => locker,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            std::process::exit(1);
        }
    };
    let _warning_socket = warning_socket;
    match vigil_wayland::run_with_warning(locker, warning) {
        Ok(LockOutcome::Unlocked) => std::process::exit(0),
        Ok(LockOutcome::Denied) => {
            eprintln!("vigil-lock: lock denied (another locker running?)");
            std::process::exit(2);
        }
        Ok(LockOutcome::Invalidated) => {
            eprintln!("vigil-lock: lock invalidated by the compositor");
            std::process::exit(1);
        }
        Ok(LockOutcome::Cancelled) => std::process::exit(3),
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

    #[test]
    fn background_worker_wakes_and_reuses_rendered_pixels() {
        let path = std::env::temp_dir().join(format!(
            "vigil-lock-background-worker-{}.png",
            std::process::id()
        ));
        image::RgbaImage::from_pixel(2, 2, image::Rgba([12, 34, 56, 255]))
            .save(&path)
            .unwrap();

        let (tx, rx) = mpsc::channel();
        let worker = BackgroundWorker::new(tx);
        let (wake_tx, wake_rx) = mpsc::channel();
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            wake_tx.send(()).unwrap();
        });
        let key = BackgroundKey {
            path: path.clone(),
            fit: BackgroundFit::Fill,
            width: 16,
            height: 16,
        };

        worker.render_prepared(OutputId(1), key.clone(), None, Some(waker.clone()));
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(first.pixels.is_ok());
        assert!(!first.cache_hit);
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        worker.render_prepared(OutputId(2), key, None, Some(waker));
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(second.pixels.is_ok());
        assert!(second.cache_hit);
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn warning_join_waits_for_locked_acknowledgement() {
        let path = std::env::temp_dir().join(format!(
            "vigil-warning-ipc-test-{}-{}.sock",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let (guard, state) = start_warning_ipc_at(path.clone()).unwrap();
        let join = std::thread::spawn(move || join_warning_at(&path).unwrap());

        for _ in 0..200 {
            if state.lock().unwrap().commit_requested {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let joiners = {
            let mut state = state.lock().unwrap();
            assert!(state.commit_requested);
            state.locked = true;
            std::mem::take(&mut state.joiners)
        };
        for mut stream in joiners {
            stream.write_all(b"locked\n").unwrap();
        }
        assert!(join.join().unwrap());
        drop(guard);
    }
}
#[test]
fn static_clock_uses_minute_deadline() {
    assert_eq!(clock_interval("%H:%M"), Duration::from_secs(60));
    assert_eq!(clock_interval("%H:%M:%S"), Duration::from_secs(1));
}
