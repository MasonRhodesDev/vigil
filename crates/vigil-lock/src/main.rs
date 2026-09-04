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

use slint_idle_runtime::{DirtySet, IdleScheduler, Metrics, WaitDecision, WakeHandle};
use vigil_config::{Config, LockTransition};
use vigil_core::{
    AppearanceEvent, AuthEvent, AuthUi, BackgroundFit, ColorScheme, FrameTarget, LoginEvent,
    OutputId, OutputInfo, UiMessage,
};
use vigil_flow::{FlowCmd, FlowEvent};
use vigil_login::{AppearanceWatcher, LoginSession};
use vigil_pam::PamAttempt;
use vigil_theme::Theme;
use vigil_ui::{Looks, OutputWindow, UiSnapshot, VigilPlatform, apply_kit_tokens_from_disk};
use vigil_wayland::{LockOutcome, LockSession};

struct Cli {
    /// None until resolved by `parse_cli` — parsing itself never reads the
    /// environment, so tests can exercise flags with USER/LOGNAME unset.
    user: Option<String>,
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
    wait: bool,
    warning_ms: Option<u64>,
    /// Skip the frost transition on lock and the reveal on unlock
    /// (`[lock.transition]`): today's instant commit, for scripts and tests.
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
            // `--wait` is the user-facing readiness contract: detach the
            // locker and return only after the compositor confirms K5. Keep
            // the old spelling for callers upgraded independently.
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

/// Fold the flags that override `vigil.toml` into the loaded config.
/// `--warn`/`--no-warn` set the cancelable warning; `--immediate` removes
/// the non-cancelable transition, and every ramp is clamped so a bad config
/// delays a lock but never prevents it.
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

/// The user to authenticate: an explicit `--user` wins; otherwise the
/// session environment. Pure, so the error path is unit-testable without
/// mutating the process environment (unsafe in edition 2024).
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

/// Whether a warning→lock rebind at `configured` pixels can keep the
/// retained window, or has to rebuild it.
///
/// "Rebind" means the same output's scene moving from the warning layer
/// surface to the session-lock surface. It is only a rebind while the
/// geometry holds: the software backend refuses a target whose dimensions
/// disagree with the window, so a mismatched rebind renders nothing,
/// forever — the output would stay black for the whole locked session
/// (issue #40's mixed-fractional-scale case, found while fixing issue #86).
///
/// Both arguments are *panel* pixels. `OutputInfo` is what the compositor
/// configured, which is a scanout size; the window's scene size is the same
/// number only while the transform is 0, and comparing against it would be
/// correct by coincidence until the day an output is rotated.
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
    /// Controller events produced by the async pumps this tick.
    pending: Vec<FlowEvent>,
    /// Last wallpaper readiness reported to the controller (edge-triggered).
    /// Seeded to `Timeline`'s own default (`true`) so a first poll that finds
    /// assets outstanding actually reports the change — seeded `false`, the
    /// two agreed by accident and the commit was never held for a slow
    /// wallpaper.
    wallpaper_ready: bool,
    /// Auth state to replay onto scenes rebuilt mid-lock (resume/resize
    /// recreates outputs; a fresh theme instance starts blank).
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

/// logind event → locker action. `Unlock` is honored without auth on
/// purpose: logind only accepts `UnlockSession` from root or the session's
/// own user, it is the escape hatch every locker implements, and a working
/// `loginctl unlock-session` is worth having when a locker misbehaves.
/// Translate a logind signal into a controller event. The grace window is
/// the controller's (a grace must never survive a suspend, #9 residual —
/// `PrepareForSleep(true)` bans one for the rest of the run).
fn login_flow_event(event: LoginEvent) -> Option<FlowEvent> {
    match event {
        LoginEvent::Unlock => Some(FlowEvent::LogindUnlock),
        // We ARE the locker; a Lock request while running means "commit now".
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
        // Either pre-lock overlay phase: out-of-band lock requests (logind,
        // sleep, a joining locker) must commit it instead of queueing.
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
            pending: Vec::new(),
            wallpaper_ready: vigil_flow::WALLPAPER_READY_DEFAULT,
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
        // Auth belongs on a HELD lock (issue #36): a denied locker that
        // opens a PAM conversation logs a failed auth on every hypridle
        // re-fire. locked() flips ipc.locked before its start_attempt call,
        // so this holds on both call sites (grant and post-failure retry).
        debug_assert!(
            self.lock_ipc.lock().expect("lock IPC poisoned").locked,
            "PAM conversation must not start before the compositor grants the lock"
        );
        let tx = self.auth_tx.clone();
        let waker = self.event_waker.clone();
        self.attempt = Some(PamAttempt::start(&self.user, move |event| {
            let _ = tx.send(event);
            if let Some(wake) = waker.lock().expect("event waker poisoned").as_ref() {
                wake.wake();
            }
        }));
    }

    /// Executor for [`FlowCmd::SignalReady`]: the compositor holds the lock
    /// from this moment — it will never reveal the session again without
    /// unlock_and_destroy, even if we have not painted yet. Safe to let a
    /// suspend proceed.
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
            // Dropping closes the fd; a --wait parent unblocks on
            // either the byte or the EOF.
        }
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
                    self.pending.push(FlowEvent::AuthOk);
                    return;
                }
                AuthEvent::Done(Err(_)) if self.unlocked => {
                    // A detached conversation (grace / loginctl unlock)
                    // finishing late is not an auth failure: starting
                    // another PAM transaction here opened a conversation
                    // nobody answers — a logged failure per unlock, and a
                    // pam_faillock strike against the user.
                }
                AuthEvent::Done(Err(message)) => {
                    // Retire the dead conversation here, not when the
                    // controller's reply arrives: pump_ui runs later in this
                    // same tick and would otherwise respond() into it and
                    // silently lose the submission.
                    self.attempt = None;
                    // The controller decides the retry (a fresh PAM
                    // transaction per attempt, hyprlock's model).
                    self.pending.push(FlowEvent::AuthErr(message));
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
            if let Some(event) = login_flow_event(event) {
                self.pending.push(event);
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

    /// Executor for [`FlowCmd::DetachAuth`]: stop caring about PAM and arm
    /// the teardown watchdog. The controller drives the release itself.
    fn detach_auth(&mut self) {
        // Unlock must never wait on PAM. Grace and loginctl unlocks can fire
        // while a conversation is mid-flight (or a module is wedged outside
        // it — pam_fprintd waiting on a finger), and the teardown drop of a
        // joining attempt was exactly issue #49's immortal-locker hang.
        if let Some(attempt) = &mut self.attempt {
            attempt.detach();
        }
        self.unlocked = true;
        // Belt and braces: teardown after this point should be milliseconds
        // (roundtrip + drops). If anything else wedges — a stuck D-Bus
        // connection, a slow thread drop — the locker must still die rather
        // than survive the session (issue #49). The screen is already
        // released when this fires, so _exit is safe.
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

    fn output_rebound(&mut self, id: OutputId, info: &OutputInfo) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if rebound_needs_resize(entry.window.panel_size(), (info.width, info.height)) {
            // The rebind is only free at unchanged geometry. Under mixed
            // fractional scale (issue #40) the lock surface's configure can
            // land at a different pixel size than the warning's, and the
            // retained window cannot fill that buffer: SoftwareBackend::render
            // rejects a target whose dimensions disagree with the panel, so
            // every present for this output answers false and the output
            // stays black for the whole session. Rebuild at the size the
            // compositor actually acked.
            //
            // Known and NOT fixed here: output_resized drops the entry and
            // calls output_ready, which early-returns on a theme
            // instantiation or adapter-capture failure and leaves the output
            // with no scene at all — black by a different route. That hole
            // predates this branch (every resize and every hotplug already
            // goes through it); the rebind just makes it reachable from one
            // more place. Fixing it means output_ready reporting failure
            // instead of returning, which is a separate change.
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
        // Deferred: arms the backend's copy-out without a Slint redraw
        // request. Called from the configure callback during the handoff,
        // where the scene is already correct and scene work is forbidden.
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
        // Surface opacity and compositor frost are owned by vigil-wayland.
        // The lock scene contributes only its wallpaper during a ramp.
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
        // Edge-triggered: the controller holds the commit while assets are
        // outstanding, so it only needs the transitions.
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
                if self.attempt.is_none() {
                    self.start_attempt();
                }
            }
            FlowCmd::ShowAuthError(message) => {
                self.snapshot.error = message.clone();
                self.snapshot.busy = false;
                let message = message.clone();
                self.each_window(move |window| {
                    window.show_error(&message);
                    window.set_busy(false);
                });
                // The controller pairs this with StartAuth: a fresh PAM
                // transaction per attempt re-prompts.
                self.attempt = None;
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
        // `false` for an unknown output deliberately contradicts the
        // trait's conservative default: it is correct only because
        // `render` above answers `false` for the same unknown output, so
        // the probe stays a strict over-approximation of "render would
        // draw". Change one and you must change the other, or an output
        // can be skipped while a render would have painted it.
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
    std::process::Command::new("date")
        .arg(format!("+{format}"))
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The blocking half of `--wait`: spawn the real locker as a child
/// whose stdout is our socketpair, and return only when it reports locked
/// (one byte) or dies first (EOF). Exit 0 here == the session IS locked.
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
        // Command still holds its copy of the child's stdout socketpair end;
        // if it lives past read_exact, a child that dies before the ready
        // byte never produces EOF and --wait hangs forever (hypridle's
        // before_sleep_cmd would wait on a crashed locker).
        drop(command);
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

/// The singleton name: one locker per user runtime dir per compositor.
/// Keyed by WAYLAND_DISPLAY so nested/test compositors never collide with
/// the real session's locker.
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

/// Join an already-running warning. Success means the compositor has
/// confirmed session-lock, not merely that the commit request was accepted.
fn join_lock() -> Result<bool, String> {
    let Some(path) = lock_socket_path() else {
        return Ok(false);
    };
    join_lock_at(&path)
}

/// How long a joiner waits for the owner to confirm the lock. A commit
/// request forces an in-flight warning to commit immediately, so a healthy
/// owner answers within scene-build time; a hung owner must not hang every
/// subsequent lock attempt with it (issue #50 interaction with #49).
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
    // Callers hold the singleton-guard flock, so any existing socket file is
    // a leftover from a dead owner (span_lines::exit skips Drop): bind
    // first, and only unlink + retry when the address is genuinely in use.
    // The old probe-then-unlink order let two racing starts both unlink and
    // both bind, stacking lockers (issue #50 TOCTOU).
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

fn main() {
    // Before anything else, so no span is opened without somewhere to go.
    // `install` rather than `layer`: it registers the layer with
    // `span_lines::exit`, so anything still open at a terminal path is
    // closed and marked `status=exit` instead of silently lost. Today that
    // is insurance, not the active mechanism: every span lives inside
    // `run_with_lock`, which returns before any of the exit calls below
    // run, so their open set is empty - no captured trace has ever carried
    // a `status=exit` record. It pays off on future exit paths, and for
    // signal handling (vigil#79), where spans genuinely are still open.
    //
    // The allowlist is "vigil" and nothing else. Installing a subscriber
    // makes this process collect every instrumented crate in its tree, and
    // zbus and calloop are in that tree carrying D-Bus paths and error
    // strings into a journal readable by adm.
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
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
    // `apply_cli_to_config` already folded --warn/--no-warn/--immediate in,
    // so the policy the controller runs is exactly the resolved config.
    let mut lock_policy = config.lock.clone();
    cli.theme = cli.theme.or(config.look.theme.clone());
    // Singleton guard (issue #50): exactly one locker per seat. flock
    // ownership is kernel-released on any death — SIGKILL and Drop-skipping
    // exits included — so only a LIVE owner blocks us; the socket is the
    // join RPC (commit / locked). Concurrent invocations either defer to
    // the owner (waiting for compositor-confirmed lock) or refuse to stack.
    let (_singleton, lock_ipc_socket, lock_ipc) = loop {
        match singleton_guard::try_acquire(&lock_instance_name()) {
            Ok(Some(guard)) => match start_lock_ipc() {
                Ok((socket, state)) => break (guard, Some(socket), state),
                Err(error) => {
                    // Owned but not joinable: still safe to lock (the flock
                    // alone prevents stacking), but a cancelable warning
                    // whose join contract can't be honored must not run.
                    eprintln!("vigil-lock: lock IPC unavailable: {error}; locking immediately");
                    lock_policy.warning.duration_ms = 0;
                    break (guard, None, Arc::new(Mutex::new(LockIpcState::default())));
                }
            },
            Ok(None) => {
                // Another live locker owns the seat. Ask it to commit and
                // succeed only once the compositor confirms the lock. Retry
                // briefly (the owner binds its socket just after taking the
                // flock) and re-try ownership each round: if the owner dies
                // mid-join its stale socket refuses connections while the
                // flock is free, and refusing to stack would leave the
                // session unlocked.
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
                        // Owner gone; take over from the top of the loop.
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
    match vigil_wayland::run_with_lock(locker, &lock_policy, None) {
        Ok(LockOutcome::Unlocked) => span_lines::exit(0),
        Ok(LockOutcome::Denied) => {
            eprintln!("vigil-lock: lock denied (another locker running?)");
            span_lines::exit(2);
        }
        Ok(LockOutcome::Invalidated) => {
            eprintln!("vigil-lock: lock invalidated by the compositor");
            span_lines::exit(1);
        }
        Ok(LockOutcome::Cancelled) => span_lines::exit(3),
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            span_lines::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // parse_cli_from must stay pure: package CI runs with USER/LOGNAME
        // unset, and these tests must not depend on builder identity.
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
        // Unchanged geometry is the whole point of the rebind: the decoded
        // wallpaper and the laid-out scene survive the move from the warning
        // layer surface to the lock surface.
        assert!(!rebound_needs_resize((3840, 2160), (3840, 2160)));
        // A configure at any other size is not a rebind. vigil-ui's software
        // backend refuses a target that disagrees with the panel (asserted
        // over there), so keeping the old window means every present for this
        // output answers false — black for the whole locked session, not one
        // frame. Mixed fractional scale is how the sizes come to disagree
        // (issue #40).
        assert!(rebound_needs_resize((3840, 2160), (3200, 1800)));
        assert!(rebound_needs_resize((3840, 2160), (2160, 3840)));
        assert!(rebound_needs_resize((3840, 2160), (3841, 2160)));
        // Both sides are panel pixels. A rotated output's scene is the
        // transposed pair, so feeding the scene in here would call an
        // unchanged configure a resize and rebuild every rotated output's
        // scene on every rebind — the comparison has to be panel-to-panel.
        // vigil-ui pins that `panel_size` and `scene_size` are in fact
        // different quantities under a quarter turn.
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
        // span_lines::exit skips Drop, so a killed owner always leaves its
        // socket file behind. Under the singleton flock that leftover is
        // provably dead: bind must reclaim it instead of refusing to start.
        let path = std::env::temp_dir().join(format!(
            "vigil-lock-ipc-stale-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        drop(UnixListener::bind(&path).unwrap()); // dead owner's leftover file
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
