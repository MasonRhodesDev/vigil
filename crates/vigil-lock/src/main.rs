use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use slint_idle_runtime::{DirtySet, IdleScheduler, Metrics, WaitDecision, WakeHandle};
use vigil_config::{Config, LockTransition};
use vigil_core::{
    AppearanceEvent, AuthError, AuthEvent, AuthUi, BackgroundFit, ColorScheme, FrameTarget,
    LoginEvent, OutputId, OutputInfo, UiMessage,
};
use vigil_flow::{FlowCmd, FlowEvent};
use vigil_login::{AppearanceWatcher, LoginSession};
use vigil_pam::PamAttempt;
use vigil_theme::Theme;
use vigil_ui::{Looks, OutputWindow, UiSnapshot, VigilPlatform, apply_kit_tokens_from_disk};
use vigil_wayland::{LockOutcome, LockSession};

struct Cli {
    user: Option<String>,
    config: Option<PathBuf>,
    theme: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: Option<BackgroundFit>,
    grace: Option<u64>,
    ready_fd: Option<i32>,
    wait: bool,
    warning_ms: Option<u64>,
    immediate: bool,
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
    let mut cli = parse_cli_from(std::env::args().skip(1))?;
    cli.user = Some(resolve_user(
        cli.user.take(),
        std::env::var("USER").ok(),
        std::env::var("LOGNAME").ok(),
    )?);
    Ok(cli)
}

fn parse_cli_from(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli {
        user: None,
        config: None,
        theme: None,
        background: None,
        bg_mode: None,
        grace: None,
        ready_fd: None,
        wait: false,
        warning_ms: None,
        immediate: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--user" => cli.user = Some(value("--user")?),
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
            "--wait" | "--daemonize" => cli.wait = true,
            "--warn" => {
                let v = value("--warn")?;
                let seconds: f64 = v.parse().map_err(|_| format!("bad --warn {v}"))?;
                cli.warning_ms = Some((seconds.max(0.0) * 1000.0).round() as u64);
            }
            "--no-warn" => cli.warning_ms = Some(0),
            "--immediate" => cli.immediate = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(cli)
}

fn apply_cli_to_config(cli: &Cli, config: &mut Config) {
    if let Some(duration_ms) = cli.warning_ms {
        config.lock.warning.duration_ms = duration_ms;
    }
    if let Some(grace_secs) = cli.grace {
        config.lock.grace_secs = grace_secs;
    }
    if cli.immediate {
        config.lock.transition = LockTransition::immediate();
    }
    if config.lock.warning.clamp() {
        eprintln!(
            "vigil-lock: lock.warning.wallpaper_hold_max_ms clamped to {} ms",
            vigil_config::LockWarning::MAX_WALLPAPER_HOLD_MS
        );
    }
    if config.lock.transition.clamp() {
        eprintln!(
            "vigil-lock: lock.transition ramps clamped to {} ms",
            LockTransition::MAX_RAMP_MS
        );
    }
}

fn resolve_user(
    explicit: Option<String>,
    env_user: Option<String>,
    env_logname: Option<String>,
) -> Result<String, String> {
    explicit
        .or(env_user)
        .or(env_logname)
        .ok_or_else(|| "cannot determine user (USER/LOGNAME unset); pass --user".into())
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

fn rebound_needs_resize(panel: (u32, u32), configured: (u32, u32)) -> bool {
    panel != configured
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
            let (pixels, cache_hit) = cosmetic(|| {
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
                (pixels, cache_hit)
            })
            .unwrap_or_else(|| (Err("background render panicked".to_owned()), false));
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

trait AuthAttempt {
    fn respond(&self, text: String);
    fn cancel(&mut self);
    fn detach(&mut self);
}

impl AuthAttempt for PamAttempt {
    fn respond(&self, text: String) {
        PamAttempt::respond(self, text);
    }
    fn cancel(&mut self) {
        PamAttempt::cancel(self);
    }
    fn detach(&mut self) {
        PamAttempt::detach(self);
    }
}

struct LiveAttempt<A> {
    attempt: A,
    events: mpsc::Receiver<AuthEvent>,
    secret: bool,
}

struct AuthTrack<A = PamAttempt> {
    live: Option<LiveAttempt<A>>,
}

impl<A: AuthAttempt> AuthTrack<A> {
    fn new() -> Self {
        Self { live: None }
    }

    fn start(&mut self, spawn: impl FnOnce(mpsc::Sender<AuthEvent>) -> A) {
        let (tx, events) = mpsc::channel();
        self.live = Some(LiveAttempt {
            attempt: spawn(tx),
            events,
            secret: true,
        });
    }

    fn is_live(&self) -> bool {
        self.live.is_some()
    }

    fn retire(&mut self) {
        self.live = None;
    }

    fn respond(&self, text: String) -> bool {
        match &self.live {
            Some(live) if !(live.secret && text.is_empty()) => {
                live.attempt.respond(text);
                true
            }
            _ => false,
        }
    }

    fn cancel(&mut self) {
        if let Some(live) = &mut self.live {
            live.attempt.cancel();
        }
    }

    fn detach(&mut self) {
        if let Some(live) = &mut self.live {
            live.attempt.detach();
        }
    }

    fn drain(&mut self) -> Vec<AuthEvent> {
        let Some(live) = &mut self.live else {
            return Vec::new();
        };
        let events: Vec<AuthEvent> = live.events.try_iter().collect();
        for event in &events {
            if let AuthEvent::Prompt { secret, .. } = event {
                live.secret = *secret;
            }
        }
        events
    }
}

const MAX_SILENT_CONVERSATION_RETRIES: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DoneOutcome {
    Authorized,
    Ignore,
    Denied(String),
    LockedOut {
        message: String,
        remaining: Option<Duration>,
    },
    ReopenQuietly(String),
    Surface(String),
}

fn done_outcome(result: Result<(), AuthError>, unlocked: bool, losses: u32) -> DoneOutcome {
    match result {
        Ok(()) => DoneOutcome::Authorized,
        Err(_) if unlocked => DoneOutcome::Ignore,
        Err(AuthError::Denied(message)) => DoneOutcome::Denied(message),
        Err(AuthError::Locked { message, remaining }) => {
            DoneOutcome::LockedOut { message, remaining }
        }
        Err(AuthError::Conversation(message)) => {
            if losses > MAX_SILENT_CONVERSATION_RETRIES {
                DoneOutcome::Surface(message)
            } else {
                DoneOutcome::ReopenQuietly(message)
            }
        }
    }
}

fn account_locked_message(remaining: Option<Duration>) -> String {
    let Some(remaining) = remaining else {
        return "Account locked \u{2014} it will not unlock on its own".into();
    };
    let seconds = remaining.as_secs();
    match (seconds / 60, seconds % 60) {
        (0, 0) => "Account unlocked \u{2014} try again".into(),
        (0, s) => format!("Account locked \u{2014} try again in {s}s"),
        (m, s) => format!("Account locked \u{2014} try again in {m}m {s:02}s"),
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
    auth: AuthTrack,
    conversation_losses: u32,
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
    pending: Vec<FlowEvent>,
    wallpaper_ready: bool,
    snapshot: UiSnapshot,
    background_worker: BackgroundWorker,
    background_rx: mpsc::Receiver<BackgroundResult>,
    background_waker: Option<Arc<dyn Fn() + Send + Sync>>,
    event_waker: Arc<Mutex<Option<WakeHandle>>>,
    scheduler: IdleScheduler,
    lock_ipc: Arc<Mutex<LockIpcState>>,
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

// The field keeps a retry typed while the attempt was checked. The failure
// hides it, and wiping it would turn the rest of the retry into a wrong password.
fn show_failure(window: &mut OutputWindow, message: &str) {
    window.show_error(message);
    window.set_busy(false);
}

fn login_flow_event(event: LoginEvent) -> Option<FlowEvent> {
    match event {
        LoginEvent::Unlock => Some(FlowEvent::LogindUnlock),
        LoginEvent::Lock => Some(FlowEvent::CommitRequested),
        LoginEvent::PrepareForSleep(sleeping) => Some(FlowEvent::PrepareForSleep(sleeping)),
    }
}

impl Locker {
    fn new(
        mut cli: Cli,
        config: Config,
        lock_ipc: Arc<Mutex<LockIpcState>>,
    ) -> Result<Self, String> {
        let user = cli
            .user
            .take()
            .ok_or("user not resolved before Locker::new")?;
        let platform = VigilPlatform::install().map_err(|e| e.to_string())?;
        let theme = Theme::load_or_default(cli.theme.as_deref());
        let clock_format = config.look.clock_format.clone();
        let event_waker = Arc::new(Mutex::new(None));
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
        let appearance_bundle = appearance_profiles::PreparedBundle::load_published(&user)
            .unwrap_or_else(|error| {
                eprintln!("vigil-lock: prepared appearance bundle: {error}");
                None
            });
        let locker = Self {
            platform,
            theme,
            entries: Vec::new(),
            panel: 0,
            user,
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
            auth: AuthTrack::new(),
            conversation_losses: 0,
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
            pending: Vec::new(),
            wallpaper_ready: vigil_flow::WALLPAPER_READY_DEFAULT,
            snapshot: {
                let mut snapshot = UiSnapshot::default();
                snapshot.on_prompt("Password", true);
                snapshot
            },
            background_worker: BackgroundWorker::new(background_tx),
            background_rx,
            background_waker: None,
            event_waker,
            scheduler: IdleScheduler::default(),
            lock_ipc,
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
        debug_assert!(
            self.lock_ipc.lock().expect("lock IPC poisoned").locked,
            "PAM conversation must not start before the compositor grants the lock"
        );
        let waker = self.event_waker.clone();
        let user = self.user.clone();
        self.auth.start(move |tx| {
            PamAttempt::start(&user, move |event| {
                let _ = tx.send(event);
                if let Some(wake) = waker.lock().expect("event waker poisoned").as_ref() {
                    wake.wake();
                }
            })
        });
    }

    fn signal_locked(&mut self) {
        eprintln!("vigil-lock: session locked");
        let joiners = {
            let mut ipc = self.lock_ipc.lock().expect("warning IPC poisoned");
            ipc.locked = true;
            std::mem::take(&mut ipc.joiners)
        };
        for mut joiner in joiners {
            let _ = joiner.write_all(b"locked\n");
        }
        if let Some(fd) = self.ready_fd.take() {
            use std::io::Write;
            use std::os::fd::FromRawFd;
            let mut ready = unsafe { std::fs::File::from_raw_fd(fd) };
            let _ = ready.write_all(b"1");
        }
    }

    fn apply_panel(&mut self) {
        for (i, e) in self.entries.iter_mut().enumerate() {
            e.window.set_panel_visible(i == self.panel);
            e.window.set_cursor_visible(i == self.panel);
        }
    }

    fn each_window(&mut self, f: impl Fn(&mut OutputWindow)) {
        for e in self.entries.iter_mut() {
            f(&mut e.window);
        }
    }

    fn pump_auth(&mut self) {
        for event in self.auth.drain() {
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
                AuthEvent::Done(result) => {
                    self.finish_attempt(result);
                    return;
                }
            }
        }
    }

    fn finish_attempt(&mut self, result: Result<(), AuthError>) {
        let conversation = result
            .as_ref()
            .err()
            .is_some_and(AuthError::is_conversation);
        self.conversation_losses = if conversation {
            self.conversation_losses + 1
        } else {
            0
        };
        match done_outcome(result, self.unlocked, self.conversation_losses) {
            DoneOutcome::Authorized => self.pending.push(FlowEvent::AuthOk),
            DoneOutcome::Ignore => {}
            DoneOutcome::Denied(message) => {
                self.auth.retire();
                self.pending.push(FlowEvent::AuthErr(message));
            }
            DoneOutcome::LockedOut { message, remaining } => {
                eprintln!("vigil-lock: account is faillocked ({message})");
                self.auth.retire();
                self.pending.push(FlowEvent::AuthLocked { remaining });
            }
            DoneOutcome::ReopenQuietly(message) => {
                eprintln!("vigil-lock: PAM conversation lost ({message}); reopening");
                self.auth.retire();
                self.pending.push(FlowEvent::AuthConversationLost);
            }
            DoneOutcome::Surface(message) => {
                eprintln!(
                    "vigil-lock: {} conversation failures in a row; surfacing the last ({message})",
                    self.conversation_losses
                );
                self.auth.retire();
                self.pending.push(FlowEvent::AuthErr(message));
            }
        }
    }

    fn show_failure(&mut self, message: String) {
        self.snapshot.error = message.clone();
        self.snapshot.busy = false;
        self.each_window(|window| show_failure(window, &message));
    }

    fn pump_ui(&mut self) {
        loop {
            let msg = self.queue.borrow_mut().pop_front();
            let Some(msg) = msg else { break };
            match msg {
                UiMessage::Respond(text) => {
                    if self.auth.respond(text) {
                        self.snapshot.busy = true;
                        self.each_window(|w| w.set_busy(true));
                        self.conversation_losses = 0;
                    }
                }
                UiMessage::Cancel => self.auth.cancel(),
                UiMessage::SelectSession(_) | UiMessage::SelectUser(_) | UiMessage::Power(_) => {}
            }
        }
    }

    fn pump_login(&mut self) {
        while let Ok(event) = self.login_rx.try_recv() {
            if let Some(event) = login_flow_event(event) {
                self.pending.push(event);
            }
        }
    }

    fn pump_appearance(&mut self) {
        while let Ok(event) = self.appearance_rx.try_recv() {
            match event {
                AppearanceEvent::Scheme(scheme) => {
                    self.scheme = scheme;
                    self.retint();
                }
                AppearanceEvent::Accent(_) => {
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

    fn detach_auth(&mut self) {
        self.auth.detach();
        self.unlocked = true;
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(5));
            unsafe { libc::_exit(0) };
        });
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
        self.lock_ipc.lock().expect("warning IPC poisoned").waker = Some(wake.clone());
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
        window.set_power_visible(false);
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
        self.entries.retain(|e| e.id != id);
        self.output_ready(id, info);
    }

    fn output_rebound(&mut self, id: OutputId, info: &OutputInfo) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if rebound_needs_resize(entry.window.panel_size(), (info.width, info.height)) {
            self.output_resized(id, info);
            return;
        }
        entry.window.request_present();
    }

    fn force_repaint(&mut self, id: OutputId) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.window.request_present();
        }
    }

    fn force_copy_out(&mut self, id: OutputId) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.window.request_present_deferred();
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

    fn overlay_progress(&mut self, _frost: f32, _wallpaper: f32) {
        self.each_window(|window| {
            window.set_panel_visible(false);
            window.set_cursor_visible(false);
            window.request_present();
        });
    }

    fn overlay_elements(&mut self, elements: &[vigil_flow::ElementSample]) {
        self.each_window(|window| {
            for element in elements {
                window.set_warning_element(&element.selector, element.progress);
            }
        });
    }

    fn poll_events(&mut self) -> Vec<FlowEvent> {
        let mut events = std::mem::take(&mut self.pending);
        let ipc_commit = {
            let mut ipc = self.lock_ipc.lock().expect("warning IPC poisoned");
            std::mem::take(&mut ipc.commit_requested)
        };
        if ipc_commit {
            events.push(FlowEvent::CommitRequested);
        }
        let ready = !self.entries.is_empty() && self.warning_backgrounds.is_empty();
        if ready != self.wallpaper_ready {
            self.wallpaper_ready = ready;
            events.push(FlowEvent::WallpaperReady(ready));
        }
        events
    }

    fn flow_command(&mut self, cmd: &FlowCmd) {
        match cmd {
            FlowCmd::ShowPanel(true) => self.apply_panel(),
            FlowCmd::ShowPanel(false) => self.each_window(|window| {
                window.set_panel_visible(false);
                window.set_cursor_visible(false);
                window.request_present();
            }),
            FlowCmd::DispatchInput(event) => {
                if let Some(entry) = self.entries.get_mut(self.panel) {
                    entry.window.dispatch(event.clone());
                }
            }
            FlowCmd::StartAuth => {
                if !self.auth.is_live() {
                    self.start_attempt();
                }
            }
            FlowCmd::ShowAuthError(message) => self.show_failure(message.clone()),
            FlowCmd::ShowAccountLocked { remaining } => {
                self.show_failure(account_locked_message(*remaining))
            }
            FlowCmd::DetachAuth => self.detach_auth(),
            FlowCmd::SetLockedHint(on) => {
                if let Some(login) = &self.login {
                    login.set_locked_hint(*on);
                }
            }
            FlowCmd::SignalReady => self.signal_locked(),
            _ => {}
        }
    }

    fn keyboard_left(&mut self) {
        self.each_window(OutputWindow::release_keys);
    }

    fn caps_lock(&mut self, on: bool) {
        if self.caps_lock != on {
            self.caps_lock = on;
            self.each_window(|w| w.set_caps_lock(on));
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

    fn scene_needs_present(&mut self, id: OutputId) -> bool {
        self.entries
            .iter_mut()
            .find(|e| e.id == id)
            .map(|e| e.window.scene_needs_present())
            .unwrap_or(false)
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
    let Ok(fmt) = std::ffi::CString::new(format) else {
        return String::new();
    };
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = vec![0_u8; 256];
        let written = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm);
        buf.truncate(written);
        String::from_utf8(buf).unwrap_or_default()
    }
}

fn detach_and_wait_for_lock() -> ! {
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
            .args(daemon_child_args(std::env::args().skip(1)))
            .args(["--ready-fd", "1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(OwnedFd::from(child_end)));
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
        drop(command);
        let mut byte = [0u8; 1];
        match parent_end.read_exact(&mut byte) {
            Ok(()) => Ok(0),
            Err(_) => {
                let status = child.wait().map_err(|e| format!("wait locker: {e}"))?;
                Ok(status.code().unwrap_or(1).max(1))
            }
        }
    })();
    match result {
        Ok(code) => span_lines::exit(code),
        Err(e) => {
            eprintln!("vigil-lock: wait: {e}");
            span_lines::exit(1);
        }
    }
}

fn daemon_child_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    args.into_iter()
        .filter(|arg| arg != "--wait" && arg != "--daemonize")
        .collect()
}

struct LockIpcSocket {
    path: PathBuf,
}

#[derive(Default)]
struct LockIpcState {
    locked: bool,
    commit_requested: bool,
    joiners: Vec<UnixStream>,
    waker: Option<WakeHandle>,
}

impl Drop for LockIpcSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_instance_name() -> String {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let display = std::path::Path::new(&display)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wayland-0".into());
    format!("vigil-lock-{display}")
}

fn lock_socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join(format!("{}.sock", lock_instance_name())))
}

fn join_lock() -> Result<bool, String> {
    let Some(path) = lock_socket_path() else {
        return Ok(false);
    };
    join_lock_at(&path)
}

const JOIN_LOCKED_TIMEOUT: Duration = Duration::from_secs(30);

fn join_lock_at(path: &std::path::Path) -> Result<bool, String> {
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
        Err(error) => return Err(format!("join lock {}: {error}", path.display())),
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(JOIN_LOCKED_TIMEOUT));
    stream
        .write_all(b"commit\n")
        .map_err(|error| format!("request lock commit: {error}"))?;
    let mut response = [0_u8; 7];
    stream
        .read_exact(&mut response)
        .map_err(|error| format!("wait for lock confirmation: {error}"))?;
    Ok(response == *b"locked\n")
}

fn start_lock_ipc() -> Result<(LockIpcSocket, Arc<Mutex<LockIpcState>>), String> {
    let path = lock_socket_path().ok_or("XDG_RUNTIME_DIR is unset")?;
    start_lock_ipc_at(path)
}

fn start_lock_ipc_at(path: PathBuf) -> Result<(LockIpcSocket, Arc<Mutex<LockIpcState>>), String> {
    use std::os::unix::fs::PermissionsExt;
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            std::fs::remove_file(&path)
                .map_err(|error| format!("clear stale socket {}: {error}", path.display()))?;
            UnixListener::bind(&path)
                .map_err(|error| format!("bind lock socket {}: {error}", path.display()))?
        }
        Err(error) => return Err(format!("bind lock socket {}: {error}", path.display())),
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure lock socket {}: {error}", path.display()))?;
    let state = Arc::new(Mutex::new(LockIpcState::default()));
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
    Ok((LockIpcSocket { path }, state))
}

fn signal_ready_fd(ready_fd: Option<i32>) {
    if let Some(fd) = ready_fd {
        use std::os::fd::FromRawFd;
        let mut ready = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = ready.write_all(b"1");
    }
}

thread_local! {
    static COSMETIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// A panic ends the lock before it can sit half alive with a dead PAM worker,
// a lost sleep signal or a poisoned lock. ext-session-lock keeps the session
// locked when its client dies. _exit leaves no core, which would hold the
// password, and the report names only the place, since a panic message can
// quote text. Only work run through `cosmetic` survives its own panic.
fn exit_on_panic() {
    std::panic::set_hook(Box::new(|info| {
        let place = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()))
            .unwrap_or_default();
        let cosmetic = COSMETIC.try_with(std::cell::Cell::get).unwrap_or(false);
        let then = if cosmetic {
            "going on without it"
        } else {
            "exiting"
        };
        let _ = writeln!(std::io::stderr(), "vigil-lock: panic at {place}; {then}");
        if !cosmetic {
            unsafe { libc::_exit(101) };
        }
    }));
}

fn cosmetic<T>(work: impl FnOnce() -> T) -> Option<T> {
    COSMETIC.set(true);
    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).ok();
    COSMETIC.set(false);
    done
}

// RLIMIT_CORE survives exec but PR_SET_DUMPABLE resets on it, so every exec'd binary calls this itself.
fn deny_memory_dumps() -> std::io::Result<()> {
    use rustix::process::{DumpableBehavior, Resource, Rlimit, set_dumpable_behavior, setrlimit};
    let core = setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );
    let dumpable = set_dumpable_behavior(DumpableBehavior::NotDumpable);
    core.and(dumpable).map_err(std::io::Error::from)
}

fn main() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    if let Err(e) = deny_memory_dumps() {
        let _ = writeln!(
            std::io::stderr(),
            "vigil-lock: could not keep memory out of core dumps: {e}"
        );
    }
    exit_on_panic();
    tracing_subscriber::registry()
        .with(span_lines::tracing_layer::install(&["vigil"]))
        .init();

    let mut cli = match parse_cli() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            span_lines::exit(2);
        }
    };
    if cli.wait {
        detach_and_wait_for_lock();
    }
    let mut config = Config::load_layered(cli.config.as_deref());
    apply_cli_to_config(&cli, &mut config);
    if let Err(error) = config.validate_warning() {
        eprintln!("vigil-lock: {error}");
        span_lines::exit(2);
    }
    let mut lock_policy = config.lock.clone();
    cli.theme = cli.theme.or(config.look.theme.clone());
    let (_singleton, lock_ipc_socket, lock_ipc) = loop {
        match singleton_guard::try_acquire(&lock_instance_name()) {
            Ok(Some(guard)) => match start_lock_ipc() {
                Ok((socket, state)) => break (guard, Some(socket), state),
                Err(error) => {
                    eprintln!("vigil-lock: lock IPC unavailable: {error}; locking immediately");
                    lock_policy.warning.duration_ms = 0;
                    break (guard, None, Arc::new(Mutex::new(LockIpcState::default())));
                }
            },
            Ok(None) => {
                let mut attempts = 0;
                loop {
                    match join_lock() {
                        Ok(true) => {
                            signal_ready_fd(cli.ready_fd.take());
                            span_lines::exit(0);
                        }
                        Ok(false) => {}
                        Err(error) => eprintln!("vigil-lock: {error}"),
                    }
                    attempts += 1;
                    if attempts >= 20 {
                        eprintln!(
                            "vigil-lock: another locker owns the seat but never confirmed the lock; refusing to stack"
                        );
                        span_lines::exit(2);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    if matches!(
                        singleton_guard::try_acquire(&lock_instance_name()),
                        Ok(Some(_))
                    ) {
                        break;
                    }
                }
                continue;
            }
            Err(error) => {
                eprintln!("vigil-lock: {error}");
                span_lines::exit(2);
            }
        }
    };
    let locker = match Locker::new(cli, config, lock_ipc) {
        Ok(locker) => locker,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            span_lines::exit(1);
        }
    };
    let _lock_ipc_socket = lock_ipc_socket;
    let outcome = match vigil_wayland::run_with_lock(locker, &lock_policy, None) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            span_lines::exit(1);
        }
    };
    match outcome {
        LockOutcome::Unlocked | LockOutcome::Cancelled => {}
        LockOutcome::Denied => eprintln!("vigil-lock: lock denied (another locker running?)"),
        LockOutcome::Invalidated => eprintln!("vigil-lock: lock invalidated by the compositor"),
        LockOutcome::Terminated(signal) => eprintln!(
            "vigil-lock: {} received; exiting, a held session lock stays held by the compositor",
            signal.name()
        ),
    }
    span_lines::exit(outcome.exit_code());
}

#[cfg(test)]
mod tests {
    #[test]
    fn clock_text_matches_what_date_would_have_printed() {
        for format in ["%H:%M", "%Y-%m-%d", "%A", "%I:%M %p"] {
            let mine = super::clock_text(format);
            let theirs = std::process::Command::new("date")
                .arg(format!("+{format}"))
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if mine != theirs {
                assert_eq!(super::clock_text(format), theirs, "format {format}");
            }
            assert!(!mine.is_empty(), "format {format} produced nothing");
        }
    }

    #[test]
    fn clock_text_survives_a_hostile_format() {
        assert_eq!(super::clock_text("with\0nul"), "");
        let _ = super::clock_text("");
        let _ = super::clock_text(&"%Y".repeat(300));
    }

    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeAttempt {
        answered: Rc<RefCell<Vec<String>>>,
        cancelled: Rc<std::cell::Cell<bool>>,
        detached: Rc<std::cell::Cell<bool>>,
    }

    impl AuthAttempt for FakeAttempt {
        fn respond(&self, text: String) {
            self.answered.borrow_mut().push(text);
        }
        fn cancel(&mut self) {
            self.cancelled.set(true);
        }
        fn detach(&mut self) {
            self.detached.set(true);
        }
    }

    fn start(track: &mut AuthTrack<FakeAttempt>) -> mpsc::Sender<AuthEvent> {
        let mut sender = None;
        track.start(|tx| {
            sender = Some(tx);
            FakeAttempt::default()
        });
        sender.expect("start must hand the worker a sender")
    }

    #[test]
    fn a_superseded_worker_cannot_touch_the_live_attempt() {
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let stale = start(&mut track);
        let live = start(&mut track);

        let _ = stale.send(AuthEvent::Error("conversation failed".into()));
        let _ = stale.send(AuthEvent::Done(Err(AuthError::Conversation(
            "Authentication failure".into(),
        ))));

        assert_eq!(
            track.drain(),
            Vec::new(),
            "a superseded worker was heard by the live attempt"
        );
        assert!(
            track.is_live(),
            "a superseded worker retired the attempt the user is typing into"
        );

        let _ = live.send(AuthEvent::Prompt {
            text: "Password:".into(),
            secret: true,
        });
        assert_eq!(
            track.drain(),
            vec![AuthEvent::Prompt {
                text: "Password:".into(),
                secret: true
            }]
        );
    }

    #[test]
    fn a_retired_attempt_is_deaf() {
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let worker = start(&mut track);
        track.retire();
        let _ = worker.send(AuthEvent::Done(Err(AuthError::Conversation("late".into()))));
        assert!(track.drain().is_empty());
        assert!(!track.is_live());
    }

    #[test]
    fn a_detached_attempt_still_reports_so_the_pump_can_ignore_it() {
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let worker = start(&mut track);
        track.detach();
        let err = AuthError::Conversation("Authentication failure".into());
        let _ = worker.send(AuthEvent::Done(Err(err.clone())));
        assert_eq!(track.drain(), vec![AuthEvent::Done(Err(err.clone()))]);
        assert_eq!(done_outcome(Err(err), true, 1), DoneOutcome::Ignore);
    }

    #[test]
    fn responses_and_cancellation_reach_the_live_attempt_only() {
        let answered: Rc<RefCell<Vec<String>>> = Rc::default();
        let cancelled: Rc<std::cell::Cell<bool>> = Rc::default();
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let (answered_probe, cancelled_probe) = (answered.clone(), cancelled.clone());
        track.start(move |_tx| FakeAttempt {
            answered: answered_probe,
            cancelled: cancelled_probe,
            detached: Rc::default(),
        });
        track.respond("hunter2".into());
        track.cancel();
        assert_eq!(*answered.borrow(), vec!["hunter2".to_owned()]);
        assert!(cancelled.get());

        track.retire();
        track.respond("hunter2".into());
        assert_eq!(answered.borrow().len(), 1);
    }

    #[test]
    fn an_empty_enter_never_answers_a_secret_question() {
        let answered: Rc<RefCell<Vec<String>>> = Rc::default();
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let probe = answered.clone();
        let mut worker = None;
        track.start(|tx| {
            worker = Some(tx);
            FakeAttempt {
                answered: probe,
                ..FakeAttempt::default()
            }
        });
        let worker = worker.expect("worker");
        let prompt = |text: &str, secret| AuthEvent::Prompt {
            text: text.into(),
            secret,
        };
        let _ = worker.send(prompt("Password:", true));
        let _ = track.drain();
        track.respond(String::new());
        track.respond("hunter2".into());
        let _ = worker.send(prompt("Code:", false));
        let _ = track.drain();
        track.respond(String::new());
        assert_eq!(
            *answered.borrow(),
            ["hunter2", ""],
            "an empty visible answer is sent, an empty secret one is not"
        );
    }

    fn panic_probe(stderr: std::process::Stdio) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::a_panic_on_any_thread_ends_the_lock_at_once_without_a_core",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("VIGIL_LOCK_PANIC_PROBE", "1")
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .output()
            .unwrap()
    }

    #[test]
    fn a_panic_on_any_thread_ends_the_lock_at_once_without_a_core() {
        use std::os::unix::process::ExitStatusExt;
        if std::env::var_os("VIGIL_LOCK_PANIC_PROBE").is_some() {
            exit_on_panic();
            let _ = std::thread::spawn(|| {
                let typed = String::from("hunter\u{e9}");
                let _ = &typed[..7];
            })
            .join();
            std::process::exit(0);
        }
        let output = panic_probe(std::process::Stdio::piped());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.signal(), None, "no signal, so no core dump");
        assert_eq!(
            output.status.code(),
            Some(101),
            "a worker panic ends the process"
        );
        assert!(stderr.contains("panic"), "the panic is reported: {stderr}");
        assert!(
            !stderr.contains("hunter") && !stderr.contains('\u{e9}'),
            "the report names no text the panic was holding: {stderr}"
        );
    }

    #[test]
    fn the_lock_keeps_its_memory_out_of_core_dumps() {
        if std::env::var_os("VIGIL_LOCK_DUMP_PROBE").is_some() {
            deny_memory_dumps().unwrap();
            let core = rustix::process::getrlimit(rustix::process::Resource::Core);
            let dumpable = rustix::process::dumpable_behavior().unwrap();
            let denied = (core.current, core.maximum) == (Some(0), Some(0))
                && dumpable == rustix::process::DumpableBehavior::NotDumpable;
            std::process::exit(if denied { 0 } else { 3 });
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::the_lock_keeps_its_memory_out_of_core_dumps",
                "--test-threads=1",
            ])
            .env("VIGIL_LOCK_DUMP_PROBE", "1")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(0), "no core limit, not dumpable");
    }

    #[test]
    fn a_panic_with_stderr_unwritable_still_exits_without_a_core() {
        use std::os::unix::process::ExitStatusExt;
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let status = panic_probe(full.into()).status;
        assert_eq!(status.signal(), None, "no abort, so no core dump");
        assert_eq!(
            status.code(),
            Some(101),
            "the report failing changes nothing"
        );
    }

    #[test]
    fn an_empty_enter_before_the_first_question_never_reaches_pam() {
        let answered: Rc<RefCell<Vec<String>>> = Rc::default();
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let probe = answered.clone();
        track.start(|_| FakeAttempt {
            answered: probe,
            ..FakeAttempt::default()
        });
        track.respond(String::new());
        track.respond("hunter2".into());
        assert_eq!(*answered.borrow(), ["hunter2"]);
    }

    #[test]
    fn a_rejected_credential_is_still_shown_and_reprompted() {
        assert_eq!(
            done_outcome(
                Err(AuthError::Denied("Authentication failure".into())),
                false,
                0
            ),
            DoneOutcome::Denied("Authentication failure".into())
        );
        assert_eq!(done_outcome(Ok(()), false, 0), DoneOutcome::Authorized);
    }

    #[test]
    fn a_broken_conversation_reopens_quietly_but_not_forever() {
        let broken = || AuthError::Conversation("Authentication failure".into());
        for losses in 1..=MAX_SILENT_CONVERSATION_RETRIES {
            assert_eq!(
                done_outcome(Err(broken()), false, losses),
                DoneOutcome::ReopenQuietly("Authentication failure".into()),
                "loss {losses}"
            );
        }
        assert_eq!(
            done_outcome(Err(broken()), false, MAX_SILENT_CONVERSATION_RETRIES + 1),
            DoneOutcome::Surface("Authentication failure".into())
        );
    }

    #[test]
    fn the_lockout_line_says_locked_and_counts_down() {
        let line = |secs| account_locked_message(Some(Duration::from_secs(secs)));
        assert_eq!(line(581), "Account locked \u{2014} try again in 9m 41s");
        assert_eq!(line(600), "Account locked \u{2014} try again in 10m 00s");
        assert_eq!(line(41), "Account locked \u{2014} try again in 41s");
        assert_eq!(line(0), "Account unlocked \u{2014} try again");
        assert_eq!(
            account_locked_message(None),
            "Account locked \u{2014} it will not unlock on its own"
        );
        for secs in [1u64, 59, 60, 600] {
            assert!(line(secs).starts_with("Account locked"), "{}", line(secs));
        }
    }

    #[test]
    fn a_lockout_is_neither_a_denial_nor_a_conversation_failure() {
        let remaining = Some(Duration::from_secs(581));
        assert_eq!(
            done_outcome(
                Err(AuthError::Locked {
                    message: "Authentication failure".into(),
                    remaining,
                }),
                false,
                0
            ),
            DoneOutcome::LockedOut {
                message: "Authentication failure".into(),
                remaining,
            }
        );
        for losses in [1, MAX_SILENT_CONVERSATION_RETRIES + 1] {
            assert!(!matches!(
                done_outcome(
                    Err(AuthError::Conversation("Authentication failure".into())),
                    false,
                    losses
                ),
                DoneOutcome::LockedOut { .. }
            ));
        }
        assert_eq!(
            done_outcome(
                Err(AuthError::Locked {
                    message: "no".into(),
                    remaining
                }),
                true,
                0
            ),
            DoneOutcome::Ignore
        );
    }

    #[test]
    fn wait_requests_blocking_lock_readiness() {
        let cli = parse_cli_from(["--wait".to_owned()]).unwrap();
        assert!(cli.wait);
    }

    #[test]
    fn daemonize_remains_a_compatibility_alias() {
        let cli = parse_cli_from(["--daemonize".to_owned()]).unwrap();
        assert!(cli.wait);
    }

    #[test]
    fn parsing_never_resolves_the_user_from_the_environment() {
        let cli = parse_cli_from(["--wait".to_owned()]).unwrap();
        assert!(cli.user.is_none());
    }

    #[test]
    fn explicit_user_wins_over_environment() {
        let user = resolve_user(
            Some("alice".into()),
            Some("bob".into()),
            Some("carol".into()),
        );
        assert_eq!(user.unwrap(), "alice");
    }

    #[test]
    fn environment_resolution_prefers_user_over_logname() {
        let user = resolve_user(None, Some("bob".into()), Some("carol".into()));
        assert_eq!(user.unwrap(), "bob");
        let user = resolve_user(None, None, Some("carol".into()));
        assert_eq!(user.unwrap(), "carol");
    }

    #[test]
    fn user_resolution_fails_closed_without_any_source() {
        let error = resolve_user(None, None, None).unwrap_err();
        assert!(error.contains("pass --user"), "{error}");
    }

    #[test]
    fn a_rebind_at_a_new_size_rebuilds_the_scene() {
        assert!(!rebound_needs_resize((3840, 2160), (3840, 2160)));
        assert!(rebound_needs_resize((3840, 2160), (3200, 1800)));
        assert!(rebound_needs_resize((3840, 2160), (2160, 3840)));
        assert!(rebound_needs_resize((3840, 2160), (3841, 2160)));
        assert!(rebound_needs_resize((2160, 3840), (3840, 2160)));
    }

    #[test]
    fn immediate_flag_disables_both_ramps() {
        let cli = parse_cli_from(["--no-warn".to_owned(), "--immediate".to_owned()]).unwrap();
        assert!(cli.immediate);
        let mut config = Config::default();
        apply_cli_to_config(&cli, &mut config);
        assert_eq!(config.lock.warning.duration_ms, 0);
        assert!(!config.lock.transition.ramps_in());
        assert!(!config.lock.transition.reveals());

        let cli = parse_cli_from(["--no-warn".to_owned()]).unwrap();
        let mut config = Config::default();
        config.lock.transition.frost_in_ms = 5_000;
        apply_cli_to_config(&cli, &mut config);
        assert!(config.lock.transition.ramps_in());
        assert_eq!(config.lock.transition.in_ms(), LockTransition::MAX_RAMP_MS);
    }

    #[test]
    fn immediate_is_forwarded_to_detached_child() {
        assert_eq!(
            daemon_child_args([
                "--wait".to_owned(),
                "--no-warn".to_owned(),
                "--immediate".to_owned(),
            ]),
            ["--no-warn", "--immediate"]
        );
    }

    #[test]
    fn readiness_flag_is_not_forwarded_to_detached_child() {
        assert_eq!(
            daemon_child_args([
                "--wait".to_owned(),
                "--warn".to_owned(),
                "10".to_owned(),
                "--daemonize".to_owned(),
            ]),
            ["--warn", "10"]
        );
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
    fn a_panic_rendering_the_wallpaper_still_locks_without_one() {
        if std::env::var_os("VIGIL_LOCK_WALLPAPER_PROBE").is_some() {
            exit_on_panic();
            let path = std::env::temp_dir().join(format!(
                "vigil-lock-wallpaper-panic-{}.png",
                std::process::id()
            ));
            image::RgbaImage::from_pixel(2, 2, image::Rgba([12, 34, 56, 255]))
                .save(&path)
                .unwrap();
            let (tx, rx) = mpsc::channel();
            let (wake_tx, wake_rx) = mpsc::channel();
            let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = wake_tx.send(());
            });
            let key = BackgroundKey {
                path: path.clone(),
                fit: BackgroundFit::Fill,
                width: 16,
                height: 16,
            };
            BackgroundWorker::new(tx).render_prepared(OutputId(1), key, None, Some(waker));
            let result = rx.recv_timeout(Duration::from_secs(5));
            let woken = wake_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            let _ = std::fs::remove_file(path);
            let delivered = result.is_ok_and(|result| result.pixels.is_err());
            std::process::exit(if delivered && woken { 0 } else { 3 });
        }
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::a_panic_rendering_the_wallpaper_still_locks_without_one",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("VIGIL_LOCK_WALLPAPER_PROBE", "1")
            .stdout(std::process::Stdio::null())
            .stderr(full)
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(0),
            "the panic costs the wallpaper and the lock goes on without it"
        );
    }

    #[test]
    fn a_retry_typed_during_the_check_survives_the_failure_hidden() {
        const W: u32 = 1280;
        const H: u32 = 800;
        let platform = VigilPlatform::install().unwrap();
        let component = Theme::load_or_default(None).instantiate().unwrap();
        let adapter = platform.claim_last_adapter().unwrap();
        let mut window = OutputWindow::new(OutputId(1), W, H, 1.0, adapter, component).unwrap();
        let sent: Rc<RefCell<Vec<UiMessage>>> = Rc::default();
        let sink = sent.clone();
        window.on_ui_message(Rc::new(move |message| sink.borrow_mut().push(message)));
        window.set_panel_visible(true);
        window.show_prompt("Password", true);
        let key = |window: &mut OutputWindow, keysym: u32, utf8: Option<&str>, pressed| {
            window.dispatch(vigil_core::InputEvent::Key {
                keysym,
                utf8: utf8.map(str::to_owned),
                pressed,
            });
        };
        let tap = |window: &mut OutputWindow, keysym: u32, utf8: Option<&str>| {
            key(window, keysym, utf8, true);
            key(window, keysym, utf8, false);
        };
        let field = |window: &mut OutputWindow| {
            std::thread::sleep(Duration::from_millis(400));
            vigil_ui::advance_timers();
            let mut buf = vec![0u8; (W * H * 4) as usize];
            window.render_if_needed(FrameTarget {
                buffer: &mut buf,
                width: W,
                height: H,
                stride: (W * 4) as usize,
                damage: None,
            });
            let row = 419 * W as usize * 4;
            buf[row + 500 * 4..row + 735 * 4].to_vec()
        };

        "aaaa"
            .chars()
            .for_each(|_| tap(&mut window, 0x61, Some("a")));
        let masked = field(&mut window);
        tap(&mut window, 0xff0d, Some("\r"));
        window.set_busy(true);
        "aa".chars().for_each(|_| tap(&mut window, 0x61, Some("a")));
        key(&mut window, 0xffe9, None, true);
        tap(&mut window, 0xffc5, None);
        key(&mut window, 0xffe9, None, false);
        show_failure(&mut window, "Authentication failure");
        window.show_prompt("Password", true);
        "aa".chars().for_each(|_| tap(&mut window, 0x61, Some("a")));
        assert!(
            masked == field(&mut window),
            "four masked dots after the failure: the retry is kept and hidden"
        );
        tap(&mut window, 0xff0d, Some("\r"));
        let answers: Vec<_> = sent
            .borrow()
            .iter()
            .filter_map(|message| match message {
                UiMessage::Respond(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            ["aaaa", "aaaa"],
            "the retry typed during the check is kept"
        );
    }

    #[test]
    fn warning_join_waits_for_locked_acknowledgement() {
        let path = std::env::temp_dir().join(format!(
            "vigil-warning-ipc-test-{}-{}.sock",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let (guard, state) = start_lock_ipc_at(path.clone()).unwrap();
        let join = std::thread::spawn(move || join_lock_at(&path).unwrap());

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

    #[test]
    fn stale_socket_from_a_dead_owner_is_reclaimed() {
        let path = std::env::temp_dir().join(format!(
            "vigil-lock-ipc-stale-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        drop(UnixListener::bind(&path).unwrap());
        let (guard, _state) = start_lock_ipc_at(path.clone()).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(guard);
    }
}
#[test]
fn static_clock_uses_minute_deadline() {
    assert_eq!(clock_interval("%H:%M"), Duration::from_secs(60));
    assert_eq!(clock_interval("%H:%M:%S"), Duration::from_secs(1));
}
