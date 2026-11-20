#![deny(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime};

use slint_idle_runtime::{DirtySet, IdleScheduler, Metrics, WaitDecision, WakeHandle};
use vigil_config::{Config, LockTransition};
use vigil_core::{
    AppearanceEvent, AuthError, AuthEvent, AuthUi, BackgroundFit, ColorScheme, FrameTarget,
    LoginEvent, OutputId, OutputInfo, Secret, UiMessage,
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
    ready_fd: Option<OwnedFd>,
    wait: bool,
    warning_ms: Option<u64>,
    immediate: bool,
}

fn clock_interval(format: &str) -> Duration {
    let mut chars = format.chars();
    while chars.by_ref().any(|c| c == '%') {
        let conversion = chars
            .by_ref()
            .find(|c| !matches!(c, '_' | '-' | '^' | '#' | 'E' | 'O' | '0'..='9'));
        if matches!(conversion, Some('S' | 'T' | 'X' | 'r' | 'c' | 's')) {
            return Duration::from_secs(1);
        }
    }
    Duration::from_secs(60)
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
                if cli.ready_fd.is_some() {
                    return Err("--ready-fd given twice".into());
                }
                cli.ready_fd = Some(adopt_ready_fd(&v)?);
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
        vigil_log::say!(
            "vigil-lock: lock.warning.wallpaper_hold_max_ms clamped to {} ms",
            vigil_config::LockWarning::MAX_WALLPAPER_HOLD_MS
        );
    }
    if config.lock.transition.clamp() {
        vigil_log::say!(
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
            let (pixels, cache_hit) = vigil_secret::cosmetic(|| {
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
                            vigil_log::say!(
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
                            vigil_log::say!(
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
                            vigil_log::say!(
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
    fn respond(&self, text: Secret);
    fn cancel(&mut self);
    fn detach(&mut self);
}

impl AuthAttempt for PamAttempt {
    fn respond(&self, text: Secret) {
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

    fn respond(&self, text: Secret) -> bool {
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
    clock: Clock,
    caps_lock: bool,
    queue: Rc<std::cell::RefCell<VecDeque<UiMessage>>>,
    auth: AuthTrack,
    refusal: Refusal,
    conversation_losses: u32,
    login: Option<LoginSession>,
    login_rx: mpsc::Receiver<LoginEvent>,
    login_tx: mpsc::Sender<LoginEvent>,
    appearance: Option<AppearanceWatcher>,
    appearance_rx: mpsc::Receiver<AppearanceEvent>,
    appearance_tx: mpsc::Sender<AppearanceEvent>,
    scheme: ColorScheme,
    unlocked: bool,
    ready_fd: Option<OwnedFd>,
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

struct LockUi<'a> {
    entries: &'a mut [Entry],
    snapshot: &'a mut UiSnapshot,
}

impl AuthUi for LockUi<'_> {
    fn show_prompt(&mut self, text: &str, secret: bool) {
        self.snapshot.on_prompt(text, secret);
        for e in self.entries.iter_mut() {
            e.window.show_prompt(text, secret);
        }
    }
    fn reset_input(&mut self) {
        for e in self.entries.iter_mut() {
            e.window.reset_input();
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

// The lock asks again as soon as an attempt fails, and a new question clears
// the message line. As at the greeter, the refusal is shown again over that
// question, and it stays until the user answers.
#[derive(Default)]
struct Refusal(Option<String>);

impl Refusal {
    // The field keeps a retry typed while the attempt was checked. The failure
    // hides it, and wiping it would turn the rest of the retry into a wrong password.
    fn show(&mut self, message: &str, ui: &mut dyn AuthUi) {
        self.0 = Some(message.to_owned());
        ui.show_error(message);
        ui.set_busy(false);
    }

    fn relay(&self, event: AuthEvent, ui: &mut dyn AuthUi) -> Option<Result<(), AuthError>> {
        match event {
            AuthEvent::Prompt { text, secret } => {
                ui.show_prompt(&text, secret);
                if let Some(message) = &self.0 {
                    ui.show_error(message);
                }
            }
            AuthEvent::Info(text) => ui.show_info(&text),
            AuthEvent::Error(text) => ui.show_error(&text),
            AuthEvent::Done(result) => return Some(result),
        }
        None
    }

    fn answered(&mut self) {
        self.0 = None;
    }
}

#[cfg(test)]
static TEST_TRUST: std::sync::OnceLock<(PathBuf, vigil_unlock_token::Owners)> =
    std::sync::OnceLock::new();

// logind lets the session's owner Unlock without polkit and the signal names no caller, so
// only root's token tells an Unlock root asked for from one the user's own processes sent.
fn root_asked(session: Option<&str>) -> Result<(), String> {
    let session = session.ok_or("no logind session")?;
    #[cfg(test)]
    if let Some((dir, owners)) = TEST_TRUST.get() {
        let at = vigil_unlock_token::Moment::now().map_err(|e| e.to_string())?;
        return vigil_unlock_token::admit(dir, *owners, session, &at)
            .map_err(|why| why.to_string());
    }
    vigil_unlock_token::admit_now(session).map_err(|why| why.to_string())
}

fn login_flow_event(event: LoginEvent, session: Option<&str>) -> Option<FlowEvent> {
    match event {
        LoginEvent::Unlock => match root_asked(session) {
            Ok(()) => Some(FlowEvent::LogindUnlock),
            Err(why) => {
                vigil_log::say!("vigil-lock: unlock ignored: {why}");
                None
            }
        },
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
        let clock = Clock::new(config.look.clock_format.clone(), SystemTime::now());
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
                    vigil_log::say!("vigil-lock: monitor profile: {diagnostic:?}");
                }
                profiles
            })
            .unwrap_or_default();
        let (background_tx, background_rx) = mpsc::channel();
        let appearance_bundle = appearance_profiles::PreparedBundle::load_published(&user)
            .unwrap_or_else(|error| {
                vigil_log::say!("vigil-lock: prepared appearance bundle: {error}");
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
                    vigil_log::say!("vigil-lock: appearance registry: {e}");
                    Default::default()
                },
            ),
            appearance_bundle,
            monitor_profiles,
            clock,
            caps_lock: false,
            queue: Rc::default(),
            auth: AuthTrack::new(),
            refusal: Refusal::default(),
            conversation_losses: 0,
            login: LoginSession::connect(),
            login_rx,
            login_tx,
            appearance: AppearanceWatcher::connect(),
            appearance_rx,
            appearance_tx,
            scheme: ColorScheme::default(),
            unlocked: false,
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
            self.lock_ipc.lock().expect("lock IPC poisoned").hold == Hold::Held,
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
        vigil_log::say!("vigil-lock: session locked");
        signal_ready_fd(self.ready_fd.take());
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
            let mut ui = LockUi {
                entries: &mut self.entries,
                snapshot: &mut self.snapshot,
            };
            if let Some(result) = self.refusal.relay(event, &mut ui) {
                self.finish_attempt(result);
                return;
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
                vigil_log::say!("vigil-lock: account is faillocked ({message})");
                self.auth.retire();
                self.pending.push(FlowEvent::AuthLocked { remaining });
            }
            DoneOutcome::ReopenQuietly(message) => {
                vigil_log::say!("vigil-lock: PAM conversation lost ({message}); reopening");
                self.auth.retire();
                self.pending.push(FlowEvent::AuthConversationLost);
            }
            DoneOutcome::Surface(message) => {
                vigil_log::say!(
                    "vigil-lock: {} conversation failures in a row; surfacing the last ({message})",
                    self.conversation_losses
                );
                self.auth.retire();
                self.pending.push(FlowEvent::AuthErr(message));
            }
        }
    }

    fn show_failure(&mut self, message: String) {
        let mut ui = LockUi {
            entries: &mut self.entries,
            snapshot: &mut self.snapshot,
        };
        self.refusal.show(&message, &mut ui);
    }

    fn pump_ui(&mut self) {
        loop {
            let msg = self.queue.borrow_mut().pop_front();
            let Some(msg) = msg else { break };
            match msg {
                UiMessage::Respond(text) => {
                    if self.auth.respond(text) {
                        self.refusal.answered();
                        LockUi {
                            entries: &mut self.entries,
                            snapshot: &mut self.snapshot,
                        }
                        .set_busy(true);
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
            let session = self.login.as_ref().map(LoginSession::id);
            if let Some(event) = login_flow_event(event, session) {
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

    #[allow(unsafe_code)]
    fn detach_auth(&mut self) {
        self.auth.detach();
        self.unlocked = true;
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(5));
            // SAFETY: _exit takes no pointers and ends the process without
            // running destructors, which an unlocked daemon no longer needs.
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
                vigil_log::say!(
                    "vigil-lock: discarding stale background for {} ({}x{})",
                    entry.connector,
                    result.key.width,
                    result.key.height
                );
                continue;
            }
            match result.pixels {
                Ok(BackgroundData::Rgba(rgba)) => {
                    entry
                        .window
                        .set_background_pixels(&rgba, result.key.width, result.key.height);
                    vigil_log::say!(
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
                        vigil_log::say!(
                            "vigil-lock: native background unsupported for {}; using fallback",
                            entry.connector
                        );
                        continue;
                    }
                    vigil_log::say!(
                        "vigil-lock: native background ready for {} in {:?}{}",
                        entry.connector,
                        result.elapsed,
                        if result.cache_hit { " (cache hit)" } else { "" }
                    );
                }
                Err(error) => vigil_log::say!("vigil-lock: background: {error}"),
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
                vigil_log::say!("vigil-lock: skipping output {id:?}: {e}");
                return;
            }
        };
        let Some(adapter) = self.platform.claim_last_adapter() else {
            vigil_log::say!("vigil-lock: skipping output {id:?}: no adapter captured");
            return;
        };
        let mut window =
            match OutputWindow::new(id, info.width, info.height, info.scale, adapter, component) {
                Ok(window) => window,
                Err(e) => {
                    vigil_log::say!("vigil-lock: skipping output {id:?}: {e}");
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
        window.set_clock(self.clock.shown());
        window.set_caps_lock(self.caps_lock);
        window.set_panel_visible(false);
        window.set_power_visible(false);
        window.set_user_name(&self.user);
        apply_kit_tokens_from_disk(&mut window, self.scheme.as_theme_str());
        self.snapshot.apply(&mut window);
        let native_background_supported = window.supports_native_background();
        let queue = self.queue.clone();
        window.on_ui_message(Rc::new(move |m| queue.borrow_mut().push_back(m)));
        vigil_log::say!(
            "vigil-lock: output {} {}x{}@{:.2}",
            info.connector,
            info.width,
            info.height,
            info.scale
        );
        self.entries.push(Entry {
            id,
            connector: info.connector.clone(),
            description: info.description().unwrap_or_else(|| info.connector.clone()),
            window,
        });
        self.focus_profile_origin();
        self.apply_panel();
        vigil_log::say!(
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
            vigil_log::say!("vigil-lock: output {} gone", e.connector);
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
        if follow_flow(&self.lock_ipc, cmd) {
            self.signal_locked();
        }
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
        if let Some(text) = self.clock.advance(SystemTime::now()) {
            let text = text.to_owned();
            self.each_window(|w| w.set_clock(&text));
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
        let clock = self.clock.wait(SystemTime::now());
        match slint {
            WaitDecision::Frame(delay) => WaitDecision::Frame(delay.min(clock)),
            WaitDecision::Timer(delay) => WaitDecision::Timer(delay.min(clock)),
            WaitDecision::Indefinite => WaitDecision::Timer(clock),
        }
    }
}

// The wait runs to the end of the period the shown text was read in, so a
// minute that turns between a tick and the next wait is shown at once.
struct Clock {
    format: String,
    shown: String,
    read_at: SystemTime,
}

impl Clock {
    fn new(format: String, now: SystemTime) -> Self {
        let shown = clock_text(&format, now);
        Self {
            format,
            shown,
            read_at: now,
        }
    }

    fn shown(&self) -> &str {
        &self.shown
    }

    fn advance(&mut self, now: SystemTime) -> Option<&str> {
        self.read_at = now;
        let text = clock_text(&self.format, now);
        if text == self.shown {
            return None;
        }
        self.shown = text;
        Some(&self.shown)
    }

    fn wait(&self, now: SystemTime) -> Duration {
        let period = clock_interval(&self.format).as_nanos();
        let epoch_ns = |at: SystemTime| {
            at.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        };
        let read = epoch_ns(self.read_at);
        let turns = read - read % period + period;
        // The loop sleeps in whole milliseconds, so round up to land after the boundary.
        let left_ms = turns.saturating_sub(epoch_ns(now)).div_ceil(1_000_000);
        Duration::from_millis(u64::try_from(left_ms).unwrap_or(u64::MAX))
    }
}

#[allow(unsafe_code)]
fn clock_text(format: &str, at: SystemTime) -> String {
    // The libc crate binds tzset only for Windows. It takes nothing, and its
    // read of TZ can race only std::env::set_var, whose caller answers for it.
    unsafe extern "C" {
        safe fn tzset();
    }
    let Ok(fmt) = std::ffi::CString::new(format) else {
        return String::new();
    };
    let Ok(now) = libc::time_t::try_from(
        at.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ) else {
        return String::new();
    };
    // SAFETY: tm is integers and a nullable tm_zone pointer, all valid when zero.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r reads the zone once per process; tzset reads it again.
    tzset();
    // SAFETY: both pointers are live locals of the right types, and localtime_r
    // is the reentrant form.
    if unsafe { libc::localtime_r(&raw const now, &raw mut tm) }.is_null() {
        return String::new();
    }
    let mut buf = vec![0_u8; 256];
    // SAFETY: strftime writes at most buf.len() bytes into buf, fmt is NUL
    // terminated, and localtime_r filled tm.
    let written = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            fmt.as_ptr(),
            &raw const tm,
        )
    };
    buf.truncate(written);
    String::from_utf8(buf).unwrap_or_default()
}

#[allow(unsafe_code)]
fn detach_and_wait_for_lock(ready_fd: Option<OwnedFd>) -> ! {
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
        let detach = || {
            // SAFETY: setsid takes no arguments and moves only this child.
            if unsafe { libc::setsid() } == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        // SAFETY: the hook runs between fork and exec, allocates nothing, and
        // calls only setsid and errno reads, which are async-signal-safe.
        unsafe { command.pre_exec(detach) };
        let mut child = command.spawn().map_err(|e| format!("spawn locker: {e}"))?;
        drop(command);
        let mut byte = [0u8; 1];
        match parent_end.read_exact(&mut byte) {
            Ok(()) => {
                signal_ready_fd(ready_fd);
                Ok(0)
            }
            Err(_) => {
                let status = child.wait().map_err(|e| format!("wait locker: {e}"))?;
                Ok(status.code().unwrap_or(1).max(1))
            }
        }
    })();
    match result {
        Ok(code) => span_lines::exit(code),
        Err(e) => {
            vigil_log::say!("vigil-lock: wait: {e}");
            span_lines::exit(1);
        }
    }
}

fn daemon_child_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut kept = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wait" | "--daemonize" => {}
            "--ready-fd" => {
                args.next();
            }
            _ => kept.push(arg),
        }
    }
    kept
}

struct LockIpcSocket {
    path: PathBuf,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Hold {
    #[default]
    Pending,
    Held,
    Releasing,
}

#[derive(Default)]
struct LockIpcState {
    hold: Hold,
    commit_requested: bool,
    joiners: Vec<UnixStream>,
    waker: Option<WakeHandle>,
}

fn follow_flow(ipc: &Mutex<LockIpcState>, cmd: &FlowCmd) -> bool {
    let joiners = {
        let mut state = ipc.lock().expect("lock IPC poisoned");
        match cmd {
            FlowCmd::SignalReady => {
                state.hold = Hold::Held;
                std::mem::take(&mut state.joiners)
            }
            FlowCmd::DetachAuth => {
                state.hold = Hold::Releasing;
                return false;
            }
            // A signal leaves a held lock to the compositor, so it stays held.
            FlowCmd::Exit(outcome) if !matches!(outcome, LockOutcome::Terminated(_)) => {
                if state.hold == Hold::Pending && *outcome == LockOutcome::Unlocked {
                    vigil_log::say!(
                        "vigil-lock: lock ended before it was confirmed: logind unlocked it"
                    );
                }
                state.hold = Hold::Releasing;
                return false;
            }
            _ => return false,
        }
    };
    for mut joiner in joiners {
        let _ = joiner.write_all(b"locked\n");
    }
    true
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
                match state.hold {
                    Hold::Held => {
                        let _ = stream.write_all(b"locked\n");
                    }
                    Hold::Pending => {
                        vigil_log::say!("vigil-lock: warning join requested immediate lock");
                        state.commit_requested = true;
                        state.joiners.push(stream);
                        if let Some(waker) = state.waker.clone() {
                            waker.wake();
                        }
                    }
                    // Never answered. This client's exit closes the stream,
                    // and the joiner then takes the singleton and locks afresh.
                    Hold::Releasing => state.joiners.push(stream),
                }
            }
        }
    });
    Ok((LockIpcSocket { path }, state))
}

fn signal_ready_fd(ready_fd: Option<OwnedFd>) {
    if let Some(fd) = ready_fd {
        let _ = std::fs::File::from(fd).write_all(b"1");
    }
}

struct StderrLines;

impl span_lines::Sink for StderrLines {
    fn write(&self, record: &[u8]) {
        vigil_log::emit(record);
    }
}

#[allow(unsafe_code)]
fn adopt_ready_fd(arg: &str) -> Result<OwnedFd, String> {
    use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
    use std::os::unix::fs::FileTypeExt;
    let fd: RawFd = arg.parse().map_err(|_| format!("bad --ready-fd {arg}"))?;
    let kind = std::fs::metadata(format!("/proc/self/fd/{fd}"))
        .map_err(|e| format!("--ready-fd {fd} is not open: {e}"))?
        .file_type();
    if !(kind.is_fifo() || kind.is_socket()) {
        return Err(format!("--ready-fd {fd} is not a pipe or socket"));
    }
    // SAFETY: main parses arguments before this process opens any descriptor, and /proc shows
    // `fd` open as a pipe or socket, so it is the caller's descriptor and nothing here owns it.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let ready = owned
        .try_clone()
        .map_err(|e| format!("--ready-fd {fd}: {e}"))?;
    if fd > 2 {
        return Ok(ready);
    }
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|e| format!("/dev/null: {e}"))?;
    match fd {
        0 => rustix::stdio::dup2_stdin(&null),
        1 => rustix::stdio::dup2_stdout(&null),
        _ => rustix::stdio::dup2_stderr(&null),
    }
    .map_err(|e| format!("--ready-fd {fd}: {e}"))?;
    let _stdio_now_null = owned.into_raw_fd();
    Ok(ready)
}

fn main() {
    vigil_secret::exit_on_panic();
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let _ = span_lines::set_sink(StderrLines);
    if let Err(e) = vigil_secret::deny_memory_dumps() {
        vigil_log::say!("vigil-lock: could not keep memory out of core dumps: {e}");
    }
    let parsed = parse_cli();
    tracing_subscriber::registry()
        .with(span_lines::tracing_layer::install(&["vigil"]))
        .init();

    let mut cli = match parsed {
        Ok(cli) => cli,
        Err(e) => {
            vigil_log::say!("vigil-lock: {e}");
            span_lines::exit(2);
        }
    };
    if cli.wait {
        detach_and_wait_for_lock(cli.ready_fd.take());
    }
    let mut config = Config::load_layered(cli.config.as_deref());
    apply_cli_to_config(&cli, &mut config);
    if let Err(error) = config.validate_warning() {
        vigil_log::say!("vigil-lock: {error}");
        span_lines::exit(2);
    }
    let mut lock_policy = config.lock.clone();
    cli.theme = cli.theme.or(config.look.theme.clone());
    let (_singleton, lock_ipc_socket, lock_ipc) = loop {
        match singleton_guard::try_acquire(&lock_instance_name()) {
            Ok(Some(guard)) => match start_lock_ipc() {
                Ok((socket, state)) => break (guard, Some(socket), state),
                Err(error) => {
                    vigil_log::say!(
                        "vigil-lock: lock IPC unavailable: {error}; locking immediately"
                    );
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
                        Err(error) => vigil_log::say!("vigil-lock: {error}"),
                    }
                    attempts += 1;
                    if attempts >= 20 {
                        vigil_log::say!(
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
                vigil_log::say!("vigil-lock: {error}");
                span_lines::exit(2);
            }
        }
    };
    let locker = match Locker::new(cli, config, lock_ipc) {
        Ok(locker) => locker,
        Err(e) => {
            vigil_log::say!("vigil-lock: {e}");
            span_lines::exit(1);
        }
    };
    let _lock_ipc_socket = lock_ipc_socket;
    let outcome = match vigil_wayland::run_with_lock(locker, &lock_policy, None) {
        Ok(outcome) => outcome,
        Err(e) => {
            vigil_log::say!("vigil-lock: {e}");
            span_lines::exit(1);
        }
    };
    match outcome {
        LockOutcome::Unlocked | LockOutcome::Cancelled => {}
        LockOutcome::Denied => vigil_log::say!("vigil-lock: lock denied (another locker running?)"),
        LockOutcome::Invalidated => {
            vigil_log::say!("vigil-lock: lock invalidated by the compositor")
        }
        LockOutcome::Terminated(signal) => vigil_log::say!(
            "vigil-lock: {} received; exiting, a held session lock stays held by the compositor",
            signal.name()
        ),
    }
    span_lines::exit(outcome.exit_code());
}

#[cfg(test)]
mod logind_forgery;

#[cfg(test)]
mod tests {
    #[test]
    fn clock_text_matches_what_date_would_have_printed() {
        for format in ["%H:%M", "%Y-%m-%d", "%A", "%I:%M %p"] {
            let mine = super::clock_text(format, std::time::SystemTime::now());
            let theirs = std::process::Command::new("date")
                .arg(format!("+{format}"))
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if mine != theirs {
                assert_eq!(
                    super::clock_text(format, std::time::SystemTime::now()),
                    theirs,
                    "format {format}"
                );
            }
            assert!(!mine.is_empty(), "format {format} produced nothing");
        }
    }

    #[test]
    fn clock_text_survives_a_hostile_format() {
        let now = std::time::SystemTime::now();
        assert_eq!(super::clock_text("with\0nul", now), "");
        let _ = super::clock_text("", now);
        let _ = super::clock_text(&"%Y".repeat(300), now);
    }

    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeAttempt {
        answered: Rc<RefCell<Vec<Secret>>>,
        cancelled: Rc<std::cell::Cell<bool>>,
        detached: Rc<std::cell::Cell<bool>>,
    }

    impl AuthAttempt for FakeAttempt {
        fn respond(&self, text: Secret) {
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
        let answered: Rc<RefCell<Vec<Secret>>> = Rc::default();
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
        assert_eq!(*answered.borrow(), vec![Secret::from("hunter2")]);
        assert!(cancelled.get());

        track.retire();
        track.respond("hunter2".into());
        assert_eq!(answered.borrow().len(), 1);
    }

    #[test]
    fn an_empty_enter_never_answers_a_secret_question() {
        let answered: Rc<RefCell<Vec<Secret>>> = Rc::default();
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
        track.respond(Secret::default());
        track.respond("hunter2".into());
        let _ = worker.send(prompt("Code:", false));
        let _ = track.drain();
        track.respond(Secret::default());
        assert_eq!(
            *answered.borrow(),
            [Secret::from("hunter2"), Secret::default()],
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
            vigil_secret::exit_on_panic();
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
            vigil_secret::deny_memory_dumps().unwrap();
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
        let answered: Rc<RefCell<Vec<Secret>>> = Rc::default();
        let mut track: AuthTrack<FakeAttempt> = AuthTrack::new();
        let probe = answered.clone();
        track.start(|_| FakeAttempt {
            answered: probe,
            ..FakeAttempt::default()
        });
        track.respond(Secret::default());
        track.respond("hunter2".into());
        assert_eq!(*answered.borrow(), [Secret::from("hunter2")]);
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

    fn ready_fd_args(fd: impl ToString) -> [String; 2] {
        ["--ready-fd".to_owned(), fd.to_string()]
    }

    #[test]
    fn ready_fd_takes_a_socket_and_signals_once() {
        use std::os::fd::IntoRawFd;
        let (mut parent, child) = UnixStream::pair().unwrap();
        let cli = parse_cli_from(ready_fd_args(child.into_raw_fd())).unwrap();
        signal_ready_fd(cli.ready_fd);
        let mut got = Vec::new();
        parent.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"1");
    }

    #[test]
    fn ready_fd_given_twice_is_refused() {
        use std::os::fd::IntoRawFd;
        let (_parent, child) = UnixStream::pair().unwrap();
        let fd = child.into_raw_fd();
        let [flag, value] = ready_fd_args(fd);
        let refused = parse_cli_from([flag.clone(), value.clone(), flag, value])
            .err()
            .unwrap();
        assert!(refused.contains("twice"), "{refused}");
    }

    #[test]
    fn an_adopted_ready_fd_is_not_inherited() {
        use std::os::fd::IntoRawFd;
        let (_parent, child) = UnixStream::pair().unwrap();
        let ready = parse_cli_from(ready_fd_args(child.into_raw_fd()))
            .unwrap()
            .ready_fd
            .unwrap();
        let flags = rustix::io::fcntl_getfd(&ready).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    #[test]
    fn the_callers_ready_fd_is_not_forwarded_to_the_detached_child() {
        assert_eq!(
            daemon_child_args([
                "--wait".to_owned(),
                "--ready-fd".to_owned(),
                "5".to_owned(),
                "--immediate".to_owned(),
            ]),
            ["--immediate"]
        );
    }

    #[test]
    fn ready_fd_refuses_a_closed_descriptor_and_a_regular_file() {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open("Cargo.toml").unwrap();
        let refused = parse_cli_from(ready_fd_args(file.as_raw_fd()))
            .err()
            .unwrap();
        assert!(refused.contains("not a pipe or socket"), "{refused}");
        let refused = parse_cli_from(ready_fd_args(1_000_000)).err().unwrap();
        assert!(refused.contains("not open"), "{refused}");
        assert!(parse_cli_from(ready_fd_args("one")).is_err());
    }

    #[test]
    fn a_ready_fd_on_stdout_leaves_dev_null_behind() {
        if std::env::var_os("VIGIL_READY_CHILD").is_some() {
            vigil_secret::deny_memory_dumps().unwrap();
            let cli = parse_cli_from(ready_fd_args(1)).unwrap();
            signal_ready_fd(cli.ready_fd);
            let opened = std::fs::File::open("Cargo.toml").unwrap();
            assert_ne!(std::os::fd::AsRawFd::as_raw_fd(&opened), 1);
            rustix::io::write(rustix::stdio::stdout(), b"stray").unwrap();
            return;
        }
        let (mut parent, child) = UnixStream::pair().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::a_ready_fd_on_stdout_leaves_dev_null_behind",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("VIGIL_READY_CHILD", "1")
            .stdout(std::process::Stdio::from(OwnedFd::from(child)))
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let mut got = String::new();
        parent.read_to_string(&mut got).unwrap();
        assert!(status.success(), "{status}");
        assert!(got.ends_with('1') && !got.contains("stray"), "{got:?}");
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
            vigil_secret::exit_on_panic();
            let (tx, rx) = mpsc::channel();
            let (wake_tx, wake_rx) = mpsc::channel();
            let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = wake_tx.send(());
            });
            let key = BackgroundKey {
                path: PathBuf::from("/nonexistent/wallpaper.png"),
                fit: BackgroundFit::Fill,
                width: 16,
                height: 16,
            };
            let worker = BackgroundWorker::new(tx);
            let poisoned = vigil_secret::cosmetic(|| {
                let _held = worker.rendered.lock();
                panic!("poison the render cache");
            });
            assert!(poisoned.is_none() && worker.rendered.is_poisoned());
            worker.render_prepared(OutputId(1), key, None, Some(waker));
            let result = rx.recv_timeout(Duration::from_secs(5));
            let woken = wake_rx.recv_timeout(Duration::from_secs(5)).is_ok();
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
                utf8: utf8.map(vigil_core::KeyText::from),
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
        Refusal::default().show("Authentication failure", &mut window);
        window.show_prompt("Password", true);
        "aa".chars().for_each(|_| tap(&mut window, 0x61, Some("a")));
        assert!(
            masked == field(&mut window),
            "four masked dots after the failure: the retry is kept and hidden"
        );
        tap(&mut window, 0xff0d, Some("\r"));
        let sent = sent.borrow();
        let answers: Vec<_> = sent
            .iter()
            .filter_map(|message| match message {
                UiMessage::Respond(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            [&Secret::from("aaaa"), &Secret::from("aaaa")],
            "the retry typed during the check is kept"
        );
    }

    #[derive(Debug, PartialEq)]
    enum Shown {
        Prompt(String),
        Info(String),
        Error(String),
        Busy(bool),
        Reset,
    }

    #[derive(Default)]
    struct RecordingUi(Vec<Shown>);

    impl AuthUi for RecordingUi {
        fn show_prompt(&mut self, text: &str, _: bool) {
            self.0.push(Shown::Prompt(text.into()));
        }
        fn reset_input(&mut self) {
            self.0.push(Shown::Reset);
        }
        fn show_info(&mut self, text: &str) {
            self.0.push(Shown::Info(text.into()));
        }
        fn show_error(&mut self, text: &str) {
            self.0.push(Shown::Error(text.into()));
        }
        fn set_busy(&mut self, busy: bool) {
            self.0.push(Shown::Busy(busy));
        }
    }

    fn question(text: &str) -> AuthEvent {
        AuthEvent::Prompt {
            text: text.into(),
            secret: true,
        }
    }

    #[test]
    fn a_refusal_stays_shown_over_the_question_that_restarts_the_attempt() {
        let mut refusal = Refusal::default();
        let mut ui = RecordingUi::default();
        refusal.show("Incorrect password", &mut ui);
        refusal.relay(AuthEvent::Info("2 tries left".into()), &mut ui);
        refusal.relay(question("Password:"), &mut ui);
        assert_eq!(
            ui.0.last(),
            Some(&Shown::Error("Incorrect password".into())),
            "the re-ask hid the refusal: {:?}",
            ui.0
        );

        refusal.relay(question("Password:"), &mut ui);
        assert_eq!(
            ui.0.last(),
            Some(&Shown::Error("Incorrect password".into())),
            "a reopened conversation hid a refusal the user has not answered: {:?}",
            ui.0
        );

        refusal.answered();
        ui.0.clear();
        refusal.relay(question("Verification code:"), &mut ui);
        assert_eq!(
            ui.0,
            [Shown::Prompt("Verification code:".into())],
            "the refusal outlived the answer that followed it"
        );
    }

    #[test]
    fn a_refusal_is_drawn_after_the_re_ask_and_on_an_output_that_joins_later() {
        const W: u32 = 1280;
        const H: u32 = 800;
        let platform = VigilPlatform::install().unwrap();
        let window = || {
            let component = Theme::load_or_default(None).instantiate().unwrap();
            let adapter = platform.claim_last_adapter().unwrap();
            let mut window = OutputWindow::new(OutputId(1), W, H, 1.0, adapter, component).unwrap();
            window.set_panel_visible(true);
            window
        };
        let frame = |window: &mut OutputWindow| {
            std::thread::sleep(Duration::from_millis(400));
            vigil_ui::advance_timers();
            let mut buf = vec![0u8; (W * H * 4) as usize];
            window.request_present();
            window.render_if_needed(FrameTarget {
                buffer: &mut buf,
                width: W,
                height: H,
                stride: (W * 4) as usize,
                damage: None,
            });
            buf
        };
        let row = |buf: &[u8], y: usize| buf[y * W as usize * 4..(y + 1) * W as usize * 4].to_vec();
        let mut entries = vec![Entry {
            id: OutputId(1),
            connector: "TEST-1".into(),
            description: String::new(),
            window: window(),
        }];
        let mut snapshot = UiSnapshot::default();
        let mut refusal = Refusal::default();
        let mut ui = LockUi {
            entries: &mut entries,
            snapshot: &mut snapshot,
        };
        ui.show_prompt("Password", true);
        let hint = frame(&mut ui.entries[0].window);
        refusal.show("Incorrect password", &mut ui);
        let refused = frame(&mut ui.entries[0].window);
        let line: Vec<usize> = (0..H as usize)
            .filter(|&y| row(&hint, y) != row(&refused, y))
            .collect();
        assert!(!line.is_empty(), "the refusal line is found");

        refusal.relay(question("Password"), &mut ui);
        let reasked = frame(&mut ui.entries[0].window);
        for &y in &line {
            assert!(
                row(&reasked, y) == row(&refused, y),
                "the re-ask replaced the refusal at row {y}"
            );
        }

        let mut joined = window();
        snapshot.apply(&mut joined);
        let joined = frame(&mut joined);
        for &y in &line {
            assert!(
                row(&joined, y) == row(&refused, y),
                "an output that joined after the re-ask lost the refusal at row {y}"
            );
        }
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
            state.hold = Hold::Held;
            std::mem::take(&mut state.joiners)
        };
        for mut stream in joiners {
            stream.write_all(b"locked\n").unwrap();
        }
        assert!(join.join().unwrap());
        drop(guard);
    }

    #[test]
    fn a_join_during_the_unlock_is_not_told_the_session_is_locked() {
        let path = std::env::temp_dir().join(format!(
            "vigil-lock-ipc-releasing-test-{}.sock",
            std::process::id()
        ));
        let (guard, state) = start_lock_ipc_at(path.clone()).unwrap();
        state.lock().unwrap().hold = Hold::Releasing;
        let join = std::thread::spawn(move || join_lock_at(&path));

        for _ in 0..200 {
            if !state.lock().unwrap().joiners.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let parked = std::mem::take(&mut state.lock().unwrap().joiners);
        assert_eq!(parked.len(), 1, "the joiner was answered or never arrived");
        assert!(!state.lock().unwrap().commit_requested);
        drop(parked);
        assert!(
            !matches!(join.join().unwrap(), Ok(true)),
            "a client that is unlocking confirmed the lock to a joiner"
        );
        drop(guard);
    }

    fn step(
        ipc: &Mutex<LockIpcState>,
        flow: &mut vigil_flow::LockFlow,
        ms: u64,
        event: FlowEvent,
    ) -> bool {
        flow.step(vigil_flow::Now::at(Duration::from_millis(ms)), event)
            .iter()
            .fold(false, |confirmed, cmd| follow_flow(ipc, cmd) | confirmed)
    }

    fn locked_flow(ipc: &Mutex<LockIpcState>) -> vigil_flow::LockFlow {
        let (mut flow, _) = vigil_flow::LockFlow::new(
            vigil_flow::Now::at(Duration::ZERO),
            &vigil_config::Lock::default(),
            Some(0),
        );
        step(ipc, &mut flow, 400, FlowEvent::Tick);
        assert!(step(ipc, &mut flow, 410, FlowEvent::LockConfirmed));
        flow
    }

    fn join_until_exit(path: PathBuf, ipc: &Mutex<LockIpcState>) -> Result<bool, String> {
        let join = std::thread::spawn(move || join_lock_at(&path));
        for _ in 0..200 {
            if join.is_finished() || !ipc.lock().unwrap().joiners.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(std::mem::take(&mut ipc.lock().unwrap().joiners));
        join.join().unwrap()
    }

    fn ipc_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vigil-lock-ipc-{name}-{}.sock", std::process::id()))
    }

    #[test]
    fn a_join_before_a_confirm_that_unlocks_is_not_told_the_session_is_locked() {
        let path = ipc_path("latched");
        let (guard, ipc) = start_lock_ipc_at(path.clone()).unwrap();
        let (mut flow, _) = vigil_flow::LockFlow::new(
            vigil_flow::Now::at(Duration::ZERO),
            &vigil_config::Lock::default(),
            Some(0),
        );
        step(&ipc, &mut flow, 100, FlowEvent::LogindUnlock);
        step(&ipc, &mut flow, 400, FlowEvent::Tick);
        let join = std::thread::spawn(move || join_lock_at(&path));
        for _ in 0..200 {
            if !ipc.lock().unwrap().joiners.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !step(&ipc, &mut flow, 410, FlowEvent::LockConfirmed),
            "readiness was signalled for a lock that unlocks at once"
        );
        drop(std::mem::take(&mut ipc.lock().unwrap().joiners));
        assert!(
            !matches!(join.join().unwrap(), Ok(true)),
            "a joiner was told locked by a client that unlocks at once"
        );
        drop(guard);
    }

    #[test]
    fn a_join_after_the_compositor_drops_the_lock_is_not_told_the_session_is_locked() {
        let path = ipc_path("invalidated");
        let (guard, ipc) = start_lock_ipc_at(path.clone()).unwrap();
        let mut flow = locked_flow(&ipc);
        step(&ipc, &mut flow, 500, FlowEvent::LockInvalidated);
        assert!(
            !matches!(join_until_exit(path, &ipc), Ok(true)),
            "a client whose lock the compositor dropped told a joiner locked"
        );
        drop(guard);
    }

    #[test]
    fn only_an_unlock_before_the_lock_is_held_is_journaled_as_unconfirmed() {
        const CHILD: &str = "VIGIL_LOCK_TEST_UNCONFIRMED";
        let cases = [
            (Hold::Pending, LockOutcome::Unlocked),
            (Hold::Held, LockOutcome::Unlocked),
            (Hold::Pending, LockOutcome::Cancelled),
        ];
        if std::env::var_os(CHILD).is_some() {
            for (hold, outcome) in cases {
                vigil_log::say!("case {hold:?} {outcome:?}");
                let ipc = Mutex::new(LockIpcState {
                    hold,
                    ..LockIpcState::default()
                });
                follow_flow(&ipc, &FlowCmd::Exit(outcome));
            }
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::only_an_unlock_before_the_lock_is_held_is_journaled_as_unconfirmed",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(
            stderr.lines().collect::<Vec<_>>(),
            [
                "case Pending Unlocked",
                "vigil-lock: lock ended before it was confirmed: logind unlocked it",
                "case Held Unlocked",
                "case Pending Cancelled",
            ]
        );
    }

    #[test]
    fn a_join_after_a_signal_is_told_the_held_lock_stays() {
        let path = ipc_path("terminated");
        let (guard, ipc) = start_lock_ipc_at(path.clone()).unwrap();
        let mut flow = locked_flow(&ipc);
        step(
            &ipc,
            &mut flow,
            500,
            FlowEvent::Terminate(vigil_flow::Signal::Terminate),
        );
        assert_eq!(join_until_exit(path, &ipc), Ok(true));
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

    const LOCKED_AT_SECOND_58: Duration = Duration::from_millis(1_800_000_058_400);

    #[test]
    fn the_clock_turns_as_each_minute_starts_and_wakes_only_then() {
        for (format, period) in [("%H:%M", 60_000), ("%H:%M:%S", 1_000)] {
            let locked = std::time::UNIX_EPOCH + LOCKED_AT_SECOND_58;
            let mut clock = Clock::new(format.into(), locked);
            let mut now = locked;
            for wake in 0..60 {
                now += clock.wait(now);
                let since_epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap();
                assert!(
                    clock.advance(now).is_some(),
                    "{format}: wake {wake} at {since_epoch:?} changed nothing"
                );
                assert!(
                    since_epoch.as_millis() % period <= 1,
                    "{format}: wake {wake} turned the clock at {since_epoch:?}, not as the period began"
                );
            }
        }
    }

    #[test]
    fn a_minute_read_just_before_it_turned_is_not_slept_through() {
        let minute = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_060);
        let mut clock = Clock::new("%H:%M".into(), minute - Duration::from_secs(30));
        assert!(clock.advance(minute - Duration::from_millis(2)).is_none());
        let wait = clock.wait(minute + Duration::from_millis(3));
        assert!(
            wait <= Duration::from_millis(1),
            "a tick 2 ms before the minute and a wait 3 ms after it slept {wait:?} on the old minute"
        );
    }

    #[test]
    fn an_idle_lock_presents_once_a_minute_while_the_compositor_echoes_the_pointer() {
        const W: u32 = 640;
        const H: u32 = 400;
        const POINTER: vigil_core::InputEvent =
            vigil_core::InputEvent::PointerAbsolute { x: 200.0, y: 120.0 };
        let platform = VigilPlatform::install().unwrap();
        let component = Theme::load_or_default(None).instantiate().unwrap();
        let adapter = platform.claim_last_adapter().unwrap();
        let mut window = OutputWindow::new(OutputId(1), W, H, 1.0, adapter, component).unwrap();
        let locked = std::time::UNIX_EPOCH + LOCKED_AT_SECOND_58;
        let mut clock = Clock::new("%H:%M".into(), locked);
        window.set_clock(clock.shown());
        window.set_panel_visible(true);
        window.set_cursor_visible(true);
        window.show_prompt("Password", true);
        Refusal::default().show("Authentication failure", &mut window);
        window.dispatch(POINTER);
        let mut buf = vec![0u8; (W * H * 4) as usize];
        let mut present = |window: &mut OutputWindow| {
            vigil_ui::advance_timers();
            window.scene_needs_present()
                && window.render_if_needed(FrameTarget {
                    buffer: &mut buf,
                    width: W,
                    height: H,
                    stride: (W * 4) as usize,
                    damage: None,
                })
        };
        for _ in 0..100 {
            present(&mut window);
            if !window.slint_window().has_active_animations() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let end = locked + Duration::from_secs(3600);
        let (mut now, mut frames) = (locked, 0);
        while now < end && frames <= 120 {
            if present(&mut window) {
                frames += 1;
                window.dispatch(POINTER);
                now += Duration::from_millis(16);
            } else {
                now += clock.wait(now);
                if let Some(text) = clock.advance(now) {
                    window.set_clock(text);
                }
            }
        }
        assert_eq!(
            frames, 60,
            "an hour locked with the card up should cost one frame per minute"
        );
    }

    #[test]
    fn every_format_that_shows_seconds_turns_each_second() {
        for (format, secs) in [
            ("%H:%M", 60),
            ("%Y-%m-%d", 60),
            ("%I:%M %p", 60),
            ("%%S", 60),
            ("trailing %", 60),
            ("%H:%M:%S", 1),
            ("%-S", 1),
            ("%_S", 1),
            ("%02S", 1),
            ("%OS", 1),
            ("%%%S", 1),
            ("%T", 1),
            ("%r", 1),
            ("%X", 1),
            ("%EX", 1),
            ("%c", 1),
            ("%Ec", 1),
            ("%s", 1),
        ] {
            assert_eq!(
                clock_interval(format),
                Duration::from_secs(secs),
                "format {format:?}"
            );
        }
    }

    // A TZif file of one zone with no transitions.
    fn zone_file(offset_secs: i32, name: &str) -> Vec<u8> {
        let mut file = b"TZif".to_vec();
        file.extend([0_u8; 16]);
        for count in [0, 0, 0, 0, 1, name.len() as u32 + 1] {
            file.extend(u32::to_be_bytes(count));
        }
        file.extend(offset_secs.to_be_bytes());
        file.extend([0, 0]);
        file.extend(name.as_bytes());
        file.push(0);
        file
    }

    #[test]
    fn the_clock_follows_a_timezone_change_made_while_locked() {
        const TEST: &str = "tests::the_clock_follows_a_timezone_change_made_while_locked";
        let at = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        if let Some(zones) = std::env::var_os("VIGIL_LOCK_ZONE_PROBE") {
            let zones = PathBuf::from(zones);
            let point_at = |zone: &str| {
                let status = std::process::Command::new("mount")
                    .arg("--bind")
                    .arg(zones.join(zone))
                    .arg("/etc/localtime")
                    .status()
                    .unwrap();
                assert!(status.success(), "could not point /etc/localtime at {zone}");
            };
            point_at("before");
            let before = clock_text("%H:%M %Z", at);
            point_at("after");
            let after = clock_text("%H:%M %Z", at);
            assert_eq!(
                (before.as_str(), after.as_str()),
                ("08:00 AAA", "17:00 BBB"),
                "the clock kept the zone it read first"
            );
            return;
        }
        let namespace = || {
            let mut unshare = std::process::Command::new("unshare");
            unshare.args([
                "--user",
                "--map-root-user",
                "--mount",
                "--propagation",
                "private",
            ]);
            unshare
        };
        let usable = std::path::Path::new("/etc/localtime").exists()
            && namespace()
                .arg("true")
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
        if !usable {
            vigil_log::say!("SKIP: needs /etc/localtime and an unprivileged mount namespace");
            return;
        }
        let zones = std::env::temp_dir().join(format!("vigil-lock-zones-{}", std::process::id()));
        std::fs::create_dir_all(&zones).unwrap();
        std::fs::write(zones.join("before"), zone_file(0, "AAA")).unwrap();
        std::fs::write(zones.join("after"), zone_file(9 * 3600, "BBB")).unwrap();
        let output = namespace()
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env("VIGIL_LOCK_ZONE_PROBE", &zones)
            .env_remove("TZ")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&zones);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
#[test]
fn static_clock_uses_minute_deadline() {
    assert_eq!(clock_interval("%H:%M"), Duration::from_secs(60));
    assert_eq!(clock_interval("%H:%M:%S"), Duration::from_secs(1));
}
