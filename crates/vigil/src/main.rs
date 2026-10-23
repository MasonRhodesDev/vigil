//! The vigil binary: calloop wiring ONLY (DESIGN.md §5). Every subsystem
//! lives in its crate; this file assembles them behind vigil-core seams.
//!
//! Config file support is M2; for M1 everything is CLI flags (greetd's
//! `command =` line carries them).

mod hold;
mod layout;
mod scene;
mod sessions;
mod splash;
mod users;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use monitor_profiles::{ConnectedOutput, Profile, ResolvedOutput};
use slint_idle_runtime::{DirtySet, IdleScheduler, Metrics, WaitDecision, WakeHandle};
use vigil_auth::AuthMachine;
use vigil_config::Config;
use vigil_core::{
    AuthUi, BackgroundFit, Canvas, FrameTarget, InputEvent, LoginEvent, OutputEvent, OutputId,
    PowerAction, PresentError, Presenter, SessionEvent, UiMessage,
};
use vigil_input::InputSystem;
use vigil_login::LoginSession;
use vigil_outputs::OutputManager;
use vigil_present_dumb::DumbBufferPresenter;
use vigil_session::SessionManager;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, UiSnapshot, VigilPlatform, apply_kit_tokens_from_system};

/// How often the banner file is re-read. Host integrations update it at
/// human timescale; a 1s poll costs one small read and needs no watcher.
const BANNER_POLL: Duration = Duration::from_secs(1);
/// Cap so a runaway file cannot break the theme's layout.
const BANNER_MAX: usize = 200;

/// Cycler slot that returns the panel to typing a name.
/// Present failures are retried every frame; log one in this many so a stuck
/// output leaves a trail without flooding the journal.
const PRESENT_LOG_EVERY: u32 = 60;

const OTHER_USER: &str = "Other…";

/// Fingerprint of `*.toml` under a profiles dir (path + len + mtime).
fn profiles_dir_fingerprint(dir: &std::path::Path) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    dir.hash(&mut hasher);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return hasher.finish();
    };
    let mut entries: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    entries.sort();
    for path in entries {
        path.hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(&path) {
            meta.len().hash(&mut hasher);
            if let Ok(modified) = meta.modified() {
                modified.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
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

struct Cli {
    user: Option<String>,
    socket: Option<String>,
    config: Option<PathBuf>,
    theme: Option<PathBuf>,
    theme_check: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: Option<BackgroundFit>,
    splash_config: Option<PathBuf>,
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
        splash_config: None,
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
            "--splash-config" => cli.splash_config = Some(PathBuf::from(value("--splash-config")?)),
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

fn appearance_fit(fit: appearance_profiles::Fit) -> BackgroundFit {
    match fit {
        appearance_profiles::Fit::Fill => BackgroundFit::Fill,
        appearance_profiles::Fit::Fit => BackgroundFit::Fit,
        appearance_profiles::Fit::Stretch => BackgroundFit::Stretch,
        appearance_profiles::Fit::Center => BackgroundFit::Center,
        appearance_profiles::Fit::Tile => BackgroundFit::Tile,
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
/// Run `compose` on a CPU-side frame and write the result into `target` with
/// one forward copy. A presenter's buffer is a mapping of GPU memory,
/// typically write-combined: writes are cheap, but every read is an
/// uncached fetch, on the order of tens of MB/s. A mix that reads its
/// destination (`Splash::blend_over`) must therefore never be pointed at
/// the mapping itself; a 4K frame read back that way takes seconds.
fn composed(
    scratch: &mut Vec<u32>,
    target: &mut FrameTarget<'_>,
    compose: impl FnOnce(&mut FrameTarget<'_>) -> bool,
) -> bool {
    let len = target.stride * target.height as usize;
    if target.buffer.len() < len {
        return false;
    }
    scratch.resize(len.div_ceil(4), 0);
    let bytes = &mut bytemuck::cast_slice_mut::<u32, u8>(&mut scratch[..])[..len];
    let mut frame = FrameTarget {
        buffer: &mut bytes[..],
        width: target.width,
        height: target.height,
        stride: target.stride,
    };
    if !compose(&mut frame) {
        return false;
    }
    splash::copy_out(&bytes[..], target);
    true
}

struct Entry {
    id: OutputId,
    connector: String,
    description: Option<String>,
    width: u32,
    height: u32,
    presenter: Box<dyn Presenter>,
    window: OutputWindow,
    /// The presenter shows the pointer on a cursor plane; the scene must
    /// not composite one.
    hw_cursor: bool,
    /// Consecutive failed presents, for throttling the log. Reset on success.
    present_failures: u32,
    /// The splash for this output (software outputs only), kept for the
    /// post-login hand-off after the greeter owns the screen.
    splash: Option<splash::Splash>,
    /// Present while the splash still covers, or is fading off, this output.
    cover: Option<splash::Cover>,
    /// The stage tag drawn on every frame, when debug tags are on.
    tag: Option<splash::Tag>,
    /// A greeter frame has been presented on this output.
    shown_once: bool,
    /// CPU-side frame for the passes that mix two pictures (the fade and
    /// the hand-off), see [`composed`]. u32-backed so the bytes are
    /// 4-aligned, as the renderer's rotate path requires.
    scratch: Vec<u32>,
}

/// An output the splash already owns: claimed, drawn and shown before the
/// toolkit loaded, waiting for its greeter window.
struct Early {
    presenter: DumbBufferPresenter,
    splash: splash::Splash,
    tag: Option<splash::Tag>,
    /// The first frame reached the CRTC.
    shown: bool,
}

/// What the splash bring-up hands to the app.
#[derive(Default)]
struct EarlySplash {
    /// Hotplug events the bring-up scan consumed; the first rescan replays them.
    events: Vec<OutputEvent>,
    outputs: BTreeMap<OutputId, Early>,
}

#[derive(Clone)]
enum BackgroundBitmap {
    Rgba(Vec<u8>, u32, u32),
    Xrgb(std::sync::Arc<[u8]>, u32, u32),
}

type DecodedBackground = Result<Option<BackgroundBitmap>, String>;

struct BackgroundResult {
    generation: u64,
    cache_key: String,
    outputs: Vec<(OutputId, DecodedBackground)>,
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
    /// One manager per GPU; the index IS the `OutputId` namespace, so a
    /// card that vanishes at runtime is tombstoned (`None`), never removed.
    outputs: Vec<Option<OutputManager>>,
    profiles_dir: Option<PathBuf>,
    profiles_fingerprint: u64,
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
    /// Loaded once the toolkit is up, which is after the splash is on screen.
    theme: Option<Theme>,
    platform: VigilPlatform,
    entries: Vec<Entry>,
    row: layout::Row,
    cursor: (f64, f64),
    panel: usize,
    queue: Rc<RefCell<VecDeque<UiMessage>>>,
    looks: vigil_ui::Looks,
    appearance_registry: appearance_profiles::Registry,
    appearance_bundle: Option<appearance_profiles::PreparedBundle>,
    background_tx: std::sync::mpsc::Sender<BackgroundResult>,
    background_rx: std::sync::mpsc::Receiver<BackgroundResult>,
    background_generation: u64,
    background_cache: std::collections::HashMap<String, Vec<(OutputId, DecodedBackground)>>,
    background_cache_key: String,
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
    /// logind sleep signals. Suspend never revokes DRM master, so no session
    /// event fires across it and this is the greeter's only notice that the
    /// kernel may have dropped its display state.
    login_rx: std::sync::mpsc::Receiver<LoginEvent>,
    #[allow(dead_code)]
    signal: LoopSignal,
    wake: WakeHandle,
    dirty: std::sync::Arc<DirtySet<OutputId>>,
    metrics: std::sync::Arc<Metrics>,
    scheduler: IdleScheduler,
    running: bool,
    present_retry: std::collections::BTreeMap<OutputId, (Instant, u32)>,
    exit_code: i32,
    splash_cfg: vigil_config::Splash,
    /// The shared scene spec the splash follows.
    scene: scene::Scene,
    /// Draw the stage tags.
    debug_tags: bool,
    /// The one timeline every covered output follows, until the greeter owns
    /// the screens.
    timeline: Option<splash::Timeline>,
    /// Outputs the splash claimed before the toolkit was up.
    early: BTreeMap<OutputId, Early>,
    /// Events the splash bring-up consumed, replayed by the first rescan.
    pending_events: Vec<OutputEvent>,
    /// `[splash] on_first_frame` has been considered (run or empty).
    first_frame_hook_done: bool,
    started: Instant,
    /// Set when start_session was accepted: the screens cross-fade to the
    /// rest frame and the process exits as soon as every one has shown it.
    leaving: Option<Instant>,
    /// Scaled-wallpaper cache directory, if one can be used.
    bg_cache_dir: Option<PathBuf>,
    /// `[render] hold_last_frame_on_exit`: how long the exit holder waits
    /// for the next owner, or None when the holder is off.
    hold_timeout: Option<Duration>,
}

/// Match a profile against the connected outputs and choose each output's
/// mode. Returns the resolved layout and the outputs to skip (profile says
/// disabled).
fn apply_profile(
    gpus: &mut [Option<OutputManager>],
    profiles: &[Profile],
) -> (Vec<ResolvedOutput>, Vec<OutputId>) {
    if profiles.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut connected = Vec::new();
    for gpu in gpus.iter().flatten() {
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
    let Some(profile) = monitor_profiles::select(&signature, profiles) else {
        eprintln!("vigil: no monitor profile matches {signature:?}");
        return (Vec::new(), Vec::new());
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
            if let Some(gpu) = gpus.get_mut((id.0 >> 24) as usize).and_then(Option::as_mut)
                && let Err(e) = gpu.set_mode(*id, want)
            {
                eprintln!("vigil: profile {}: {e}; using preferred mode", profile.name);
            }
        }
    }
    (resolved.outputs, disabled)
}

/// Claim every connected output for the splash and show its first frame on
/// all of them at once.
///
/// This runs before anything slow: loading the toolkit alone blocks for about
/// a second, and the greeter windows come after it. Every output is claimed
/// and every first frame drawn (in parallel) before any is committed, so the
/// screens light within a few milliseconds of each other instead of one
/// output's startup cost after the next's.
fn splash_bringup(
    gpus: &mut [Option<OutputManager>],
    profiles: &[Profile],
    cfg: &vigil_config::Splash,
    spec: &scene::Scene,
    debug_tags: bool,
    keep_state: bool,
    claim: bool,
) -> EarlySplash {
    let mut events = Vec::new();
    for gpu in gpus.iter_mut().flatten() {
        match gpu.scan() {
            Ok(batch) => events.extend(batch),
            // Left for the first rescan, which also knows how to retire a
            // vanished card.
            Err(e) => eprintln!("vigil: initial scan failed: {e}"),
        }
    }
    let mut out = EarlySplash::default();
    if !cfg.enabled || !claim {
        out.events = events;
        return out;
    }
    let (layout, disabled) = apply_profile(gpus, profiles);
    struct Claimed {
        id: OutputId,
        ns: usize,
        connector: String,
        scene: (u32, u32),
        transform: u8,
        presenter: DumbBufferPresenter,
        shown: bool,
        /// Whether the first commit changes the CRTC's mode, as read before
        /// it (None: unreadable).
        modeset: Option<bool>,
    }
    let mut claimed = Vec::new();
    for event in &events {
        let OutputEvent::Added(id, _) = event else {
            continue;
        };
        if disabled.contains(id) {
            continue;
        }
        let ns = (id.0 >> 24) as usize;
        let Some(gpu) = gpus.get_mut(ns).and_then(Option::as_mut) else {
            continue;
        };
        let Some(info) = gpu.info(*id).cloned() else {
            continue;
        };
        let transform = layout
            .iter()
            .find(|o| o.name == info.connector)
            .map_or(0, |o| o.transform)
            % 4;
        let modeset = gpu.first_commit_is_modeset(*id);
        let presenter = match gpu
            .create_surface(*id)
            .map_err(|e| e.to_string())
            .and_then(|surface| DumbBufferPresenter::new(surface).map_err(|e| e.to_string()))
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("vigil: {}: splash claim failed: {e}", info.connector);
                continue;
            }
        };
        let (pw, ph) = presenter.size();
        let scene = if transform % 2 == 1 {
            (ph, pw)
        } else {
            (pw, ph)
        };
        claimed.push(Claimed {
            id: *id,
            ns,
            connector: info.connector.clone(),
            scene,
            transform,
            presenter,
            shown: false,
            modeset,
        });
    }
    out.events = events;
    if claimed.is_empty() {
        return out;
    }

    // Every splash is built at once, off the one thread that would otherwise
    // pay for them in turn.
    let t_build = vigil_core::uptime_ms();
    let splashes: Vec<splash::Splash> = std::thread::scope(|scope| {
        claimed
            .iter()
            .map(|c| scope.spawn(|| splash::Splash::new(cfg, spec, c.scene, c.transform)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().expect("splash builder panicked"))
            .collect()
    });
    eprintln!(
        "vigil: [t+{}ms] splash built for {} output(s) in {} ms",
        vigil_core::uptime_ms(),
        claimed.len(),
        vigil_core::uptime_ms() - t_build
    );

    // Draw every first frame into its back buffer, then commit them all.
    let mut parts: Vec<(Claimed, splash::Splash, Option<splash::Tag>)> = claimed
        .into_iter()
        .zip(splashes)
        .map(|(c, sp)| {
            let tag =
                debug_tags.then(|| splash::Tag::new(spec, &c.connector, c.scene, c.transform));
            (c, sp, tag)
        })
        .collect();
    let draw =
        |presenter: &mut DumbBufferPresenter, sp: &splash::Splash, tag: &Option<splash::Tag>| {
            presenter.stage_frame(&mut |canvas| match canvas {
                Canvas::Cpu(mut target) => {
                    let ok = sp.draw_full(&mut target, 0.0);
                    if let (true, Some(tag)) = (ok, tag) {
                        tag.draw(&mut target, splash::Stage::GreeterSplash);
                    }
                    ok
                }
                _ => false,
            })
        };
    let mut failed_gpus = Vec::new();
    for (c, sp, tag) in parts.iter_mut() {
        if let Err(e) = draw(&mut c.presenter, sp, tag) {
            eprintln!("vigil: {}: splash first frame: {e}", c.connector);
        }
    }
    for (c, _, _) in parts.iter_mut() {
        match c.presenter.commit_staged() {
            Ok(true) => {
                c.shown = true;
                eprintln!(
                    "vigil: [t+{}ms] splash first frame on {}x{} ({})",
                    vigil_core::uptime_ms(),
                    c.presenter.size().0,
                    c.presenter.size().1,
                    match (keep_state, c.modeset) {
                        (false, _) => "modeset: display state was reset at open",
                        (true, Some(true)) => "modeset: the CRTC runs another mode",
                        (true, Some(false)) => "no modeset: the CRTC already runs this mode",
                        (true, None) => "modeset unknown",
                    }
                );
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("vigil: {}: splash first frame: {e}", c.connector);
                if keep_state && !failed_gpus.contains(&c.ns) {
                    failed_gpus.push(c.ns);
                }
            }
        }
    }
    // The state the boot splash left was refused. Start from nothing, as
    // vigil always did, and show every frame again.
    for ns in failed_gpus {
        eprintln!("vigil: gpu {ns}: kept display state refused; resetting it");
        if let Some(gpu) = gpus.get_mut(ns).and_then(Option::as_mut)
            && let Err(e) = gpu.reset_display_state()
        {
            eprintln!("vigil: gpu {ns}: reset failed: {e}");
        }
        for (c, sp, tag) in parts.iter_mut().filter(|(c, _, _)| c.ns == ns) {
            c.presenter.invalidate();
            let shown = draw(&mut c.presenter, sp, tag).and_then(|staged| {
                if staged {
                    c.presenter.commit_staged()
                } else {
                    Ok(false)
                }
            });
            match shown {
                Ok(done) => c.shown = done,
                Err(e) => eprintln!("vigil: {}: splash first frame: {e}", c.connector),
            }
        }
    }
    for (c, sp, tag) in parts {
        out.outputs.insert(
            c.id,
            Early {
                presenter: c.presenter,
                splash: sp,
                tag,
                shown: c.shown,
            },
        );
    }
    out
}

impl App {
    fn wait_timeout(&self) -> Option<Duration> {
        let slint = match self
            .scheduler
            .from_slint(self.entries.iter().map(|entry| entry.window.slint_window()))
        {
            WaitDecision::Frame(delay) | WaitDecision::Timer(delay) => Some(delay),
            WaitDecision::Indefinite => None,
        };
        // These are real application deadlines, not a frame clock. The clock
        // and host banner are sampled only when their displayed value may change.
        let clock = clock_interval(&self.clock_format).saturating_sub(self.last_clock.0.elapsed());
        let banner = self
            .banner_file
            .as_ref()
            .map(|_| BANNER_POLL.saturating_sub(self.last_banner.elapsed()));
        let retry = self
            .present_retry
            .values()
            .map(|(deadline, _)| deadline.saturating_duration_since(Instant::now()))
            .min();
        let now = Instant::now();
        let cover = if self.leaving.is_some() {
            Some(Duration::from_millis(5))
        } else if let Some(timeline) = &self.timeline {
            if timeline.is_ready() {
                // Fading: frames come off the greeter's own render path.
                Some(splash::FRAME_EVERY)
            } else {
                self.entries
                    .iter()
                    .filter_map(|e| e.cover.as_ref())
                    .map(|c| c.next_in(now))
                    .min()
            }
        } else {
            None
        };
        [slint, Some(clock), banner, retry, cover]
            .into_iter()
            .flatten()
            .min()
    }

    fn on_session(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Pause => {
                self.active = false;
                self.input.suspend();
                for gpu in self.outputs.iter_mut().flatten() {
                    gpu.pause();
                }
            }
            SessionEvent::Activate => {
                self.active = true;
                if let Err(e) = self.input.resume() {
                    eprintln!("vigil: {e}");
                }
                for gpu in self.outputs.iter_mut().flatten() {
                    let _ = gpu.activate();
                }
                // A render racing the pause can hit DeviceInactive and drop
                // its entry, and no udev event replays a VT switch — so any
                // output a manager still knows but we lost gets rebuilt.
                let known: Vec<OutputId> = self
                    .outputs
                    .iter()
                    .flatten()
                    .flat_map(|gpu| gpu.ids())
                    .collect();
                for id in known {
                    if !self.entries.iter().any(|e| e.id == id)
                        && let Err(e) = self.add_output(id)
                    {
                        eprintln!("vigil: rebuilding output {id:?}: {e}");
                    }
                }
                self.rebuild_row();
                // Reclaiming the device does not restore our CRTC state, so
                // the presenters must be told to modeset rather than flip.
                for e in self.entries.iter_mut() {
                    e.presenter.invalidate();
                    e.window.request_present();
                    e.window.set_panel_visible(false);
                    if let Some(c) = e.cover.as_mut() {
                        c.reset_slots();
                    }
                }
                self.apply_panel();
                // Rebuilt outputs lost their bitmaps with their windows;
                // the per-user cache (or the scaled-wallpaper cache) refills
                // them without decoding again.
                self.refresh_backgrounds_async();
            }
        }
    }

    /// Drain logind's sleep signals.
    fn pump_login(&mut self) {
        while let Ok(event) = self.login_rx.try_recv() {
            match event {
                LoginEvent::PrepareForSleep(true) => {}
                LoginEvent::PrepareForSleep(false) => self.on_resume(),
                // The greeter has no session to lock or unlock.
                LoginEvent::Lock | LoginEvent::Unlock => {}
            }
        }
    }

    /// Rebuild the scanout state after a system resume.
    ///
    /// Nothing pauses the greeter across suspend — logind revokes DRM master
    /// on a VT switch, not on sleep — so the presenters still believe their
    /// CRTCs hold the mode they set before the machine went down. Flipping
    /// onto that either fails or scans out nothing, and a greeter showing a
    /// black screen has no way to notice. Force a full modeset and a present
    /// that does not trust the surviving buffer contents.
    fn on_resume(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.presenter.invalidate();
            entry.window.request_present();
            if let Some(c) = entry.cover.as_mut() {
                c.reset_slots();
            }
        }
        // The clock is now wrong by however long the machine slept.
        let text = clock_text(&self.clock_format);
        for entry in self.entries.iter_mut() {
            entry.window.set_clock(&text);
        }
        self.last_clock = (Instant::now(), text);
    }

    /// Match a profile against the connected outputs and cache its resolved
    /// geometry. Returns the outputs to skip (profile says disabled).
    fn resolve_profile(&mut self) -> Vec<OutputId> {
        self.reload_profiles_from_disk();
        let (layout, disabled) = apply_profile(&mut self.outputs, &self.profiles);
        self.layout = layout;
        disabled
    }

    /// Re-read `profiles_dir` when its TOML set changes (#31). No-op when
    /// the fingerprint matches; degrades to an empty set if the dir is gone.
    fn reload_profiles_from_disk(&mut self) -> bool {
        let Some(dir) = &self.profiles_dir else {
            return false;
        };
        let fingerprint = profiles_dir_fingerprint(dir);
        if fingerprint == self.profiles_fingerprint {
            return false;
        }
        let (profiles, diags) = monitor_profiles::load_dir(dir);
        for d in diags {
            eprintln!("vigil: profile {}: {}", d.source, d.message);
        }
        self.profiles = profiles;
        self.profiles_fingerprint = fingerprint;
        true
    }

    /// Route a page-flip completion to the presenter it belongs to. CRTC
    /// ids are per-device, so the namespace disambiguates across GPUs.
    fn vblank(&mut self, namespace: u32, crtc: u32) {
        for e in self.entries.iter_mut() {
            if e.id.0 >> 24 == namespace && e.presenter.crtc_id() == Some(crtc) {
                e.presenter.vblank();
            }
        }
    }

    fn rescan(&mut self) {
        let mut events = std::mem::take(&mut self.pending_events);
        for slot in self.outputs.iter_mut() {
            let Some(gpu) = slot else { continue };
            match gpu.scan() {
                Ok(batch) => events.extend(batch),
                Err(e) if gpu.alive() => eprintln!("vigil: hotplug scan failed: {e}"),
                Err(_) => {
                    // The whole card is gone (a dock GPU unplugged at the
                    // greeter, #6). Tombstone the manager — the namespace is
                    // the vec index, so it can never be removed — and retire
                    // its outputs; a re-plugged card comes back as a device
                    // this process has no manager for and stays ignored
                    // until the next greeter start.
                    let ns = gpu.namespace();
                    eprintln!("vigil: gpu {ns} vanished; dropping its outputs");
                    events.extend(
                        self.entries
                            .iter()
                            .filter(|e| e.id.0 >> 24 == ns)
                            .map(|e| OutputEvent::Removed(e.id)),
                    );
                    *slot = None;
                }
            }
        }
        let disabled = self.resolve_profile();
        let mut added = false;
        for event in events {
            match event {
                OutputEvent::Added(id, _) => {
                    if disabled.contains(&id) {
                        eprintln!("vigil: output {id:?} disabled by profile");
                        continue;
                    }
                    match self.add_output(id) {
                        Ok(()) => added = true,
                        Err(e) => eprintln!("vigil: skipping output {id:?}: {e}"),
                    }
                }
                OutputEvent::Removed(id) => self.remove_output(id),
                OutputEvent::NeedsRedraw(id) => {
                    if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
                        entry.window.request_present();
                    }
                }
            }
        }
        self.rebuild_row();
        if added {
            // The cached set of per-output bitmaps predates these outputs.
            self.background_cache.remove(&self.background_cache_key);
            self.refresh_backgrounds_async();
        }
    }

    /// The manager owning `id` (its namespace is the vec index).
    fn gpu_for(&mut self, id: OutputId) -> Result<&mut OutputManager, String> {
        let index = (id.0 >> 24) as usize;
        self.outputs
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| format!("no GPU {index} for output {id:?}"))
    }

    /// Build a GL window and presenter for one output.
    ///
    /// Absent the `gl` feature this always fails, so a config asking for GL
    /// on a software build says so once per output and carries on.
    #[cfg(feature = "gl")]
    fn gl_output(
        &mut self,
        id: OutputId,
        scale: f32,
        transform: u8,
        atomic: bool,
        surface_slot: &mut Option<vigil_outputs::DrmSurface>,
        device_fd: vigil_outputs::DrmDeviceFd,
    ) -> Result<(OutputWindow, Box<dyn Presenter>), String> {
        use std::sync::Arc;
        // Flipped variants rotate-without-the-flip, same as software.
        let transform = transform % 4;
        // The cursor rides a DRM cursor plane, which needs atomic
        // commits — smithay's legacy path drives only the primary plane.
        if !atomic {
            return Err("legacy modesetting has no cursor plane".into());
        }
        // No plane, no GL: a mouse-driven greeter without a pointer is not
        // deployable (#25), and software composites a perfectly good one.
        // Probed BEFORE the surface is consumed so the fallback can have it.
        if !surface_slot
            .as_ref()
            .is_some_and(|s| vigil_present_gl::GbmPresenter::probe_cursor(s, transform))
        {
            return Err("no usable ARGB8888 cursor plane".into());
        }
        // Duplicate the device's own descriptor: DRM master rides on the
        // open file description, so re-opening the node would not be master.
        let fd = Arc::new(
            std::os::fd::AsFd::as_fd(&device_fd)
                .try_clone_to_owned()
                .map_err(|e| format!("dup drm fd: {e}"))?,
        );
        let context = Rc::new(vigil_gl::GlContext::from_fd(fd).map_err(|e| e.to_string())?);
        // Ask the hardware whether it will rotate a scanout buffer before
        // committing to GL at all: a TEST-ONLY atomic commit on the borrowed
        // surface. Refusal (virtio-gpu has no rotation property; some
        // hardware rejects some angles) falls back to software, which
        // rotates correctly — a rendering preference must never be why
        // someone cannot log in (#26).
        if let Some(s) = surface_slot.as_ref()
            && let Err(e) =
                vigil_present_gl::GbmPresenter::test_transform(s, &device_fd, &context, transform)
        {
            return Err(format!("rotation test: {e}"));
        }
        let surface = surface_slot.take().ok_or("no DRM surface")?;
        let (presenter, gl_surface) =
            vigil_present_gl::GbmPresenter::new(surface, device_fd, context, scale, transform)
                .map_err(|e| e.to_string())?;
        // The probe above said yes, so construction claiming the plane is
        // the invariant this asserts, not a fallback path.
        if !presenter.has_cursor() {
            return Err("cursor plane vanished between probe and claim".into());
        }
        let (w, h) = presenter.size();
        let gl_window =
            vigil_gl::GlWindow::with_surface(gl_surface, vigil_gl::PhysicalSizeExport::new(w, h))
                .map_err(|e| e.to_string())?;
        gl_window.set_redraw_handle(self.wake.redraw_handle(
            self.dirty.clone(),
            self.metrics.clone(),
            id,
        ));
        gl_window.set_size(vigil_gl::PhysicalSizeExport::new(w, h));

        // One instantiation asks for an adapter more than once, so the
        // override is held across it and cleared after.
        // Count across the instantiation rather than checking for any
        // unclaimed adapter: earlier outputs leave their own behind, and a
        // leftover is not evidence about this one.
        let before = self.platform.adapters_created();
        self.platform.set_next_output(id);
        self.platform.use_next_adapter(gl_window.clone());
        let component = self.theme.as_ref().ok_or("theme not loaded")?.instantiate();
        self.platform.clear_adapter_override();
        self.platform.clear_next_output();
        let component = component.map_err(|e| e.to_string())?;
        if self.platform.adapters_created() > before {
            return Err("theme bound to a software adapter".into());
        }

        let window = OutputWindow::with_backend(
            id,
            w,
            h,
            scale,
            component,
            Box::new(vigil_gl::GlBackend::new(gl_window)),
        )
        .map_err(|e| e.to_string())?;
        Ok((window, Box::new(presenter)))
    }

    #[cfg(not(feature = "gl"))]
    fn gl_output(
        &mut self,
        _id: OutputId,
        _scale: f32,
        _transform: u8,
        _atomic: bool,
        _surface_slot: &mut Option<vigil_outputs::DrmSurface>,
        _device_fd: vigil_outputs::DrmDeviceFd,
    ) -> Result<(OutputWindow, Box<dyn Presenter>), String> {
        Err("built without the gl feature".into())
    }

    fn add_output(&mut self, id: OutputId) -> Result<(), String> {
        // The splash may already own this output (and its surface).
        let early = self.early.remove(&id);
        let gpu = self.gpu_for(id)?;
        let info = gpu.info(id).cloned().ok_or("no info for output")?;
        let surface = match early {
            Some(_) => None,
            None => Some(gpu.create_surface(id).map_err(|e| e.to_string())?),
        };
        let device_fd = gpu.device_fd();
        let atomic = gpu.is_atomic();
        let want_gl =
            early.is_none() && self.looks.config.render.backend.eq_ignore_ascii_case("gl");
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
        // A rendering preference must never be why someone cannot log in, so
        // a GL failure degrades to software with a log line, per output.
        let mut surface_slot = surface;
        let resolved_background = self.appearance_registry.resolve(
            &appearance_profiles::OutputIdentity::new(&info.connector, info.description()),
            None,
        );
        let (background, _fit) = self.looks.for_connector_with_fallback(
            &info.connector,
            resolved_background.path,
            Some(appearance_fit(resolved_background.fit)),
        );
        let mut splash_slot: Option<(splash::Splash, Option<splash::Tag>, bool)> = None;
        let gl = if want_gl {
            match self.gl_output(id, scale, transform, atomic, &mut surface_slot, device_fd) {
                Ok(built) => Some(built),
                Err(e) => {
                    eprintln!(
                        "vigil: {}: GL unavailable ({e}); using software",
                        info.connector
                    );
                    None
                }
            }
        } else {
            None
        };
        let hw_cursor = gl.is_some();

        let (mut window, presenter) = match gl {
            Some(built) => {
                eprintln!(
                    "vigil: {}: rendering with GL (hardware cursor)",
                    info.connector
                );
                built
            }
            None => {
                let presenter: Box<dyn Presenter> = match early {
                    Some(Early {
                        presenter,
                        splash,
                        tag,
                        shown,
                    }) => {
                        splash_slot = Some((splash, tag, shown));
                        Box::new(presenter)
                    }
                    None => {
                        let surface = surface_slot
                            .take()
                            .ok_or("DRM surface consumed by a failed GL attempt")?;
                        Box::new(DumbBufferPresenter::new(surface).map_err(|e| e.to_string())?)
                    }
                };
                self.platform.set_next_output(id);
                let component = self.theme.as_ref().ok_or("theme not loaded")?.instantiate();
                self.platform.clear_next_output();
                let component = component.map_err(|e| e.to_string())?;
                let adapter = self
                    .platform
                    .claim_last_adapter()
                    .ok_or("no window adapter captured for theme instance")?;
                let window = OutputWindow::with_transform(
                    id,
                    info.width,
                    info.height,
                    scale,
                    transform,
                    adapter,
                    component,
                )
                .map_err(|e| e.to_string())?;
                (window, presenter)
            }
        };
        let (scene_width, scene_height) = window.scene_size();

        // The background itself arrives from the async job rescan() starts
        // (decoded once, cached, scaled off this thread). Decoding it here,
        // once per output, was most of the sequential output bring-up.
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
        apply_kit_tokens_from_system(&mut window, "");
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
        let (splash, mut cover, tag) = match splash_slot {
            Some((splash, tag, shown)) => {
                let mut cover = splash::Cover::new();
                if shown {
                    cover.presented(Instant::now(), 0.0);
                }
                (Some(splash), Some(cover), tag)
            }
            // No splash claimed it (a hotplug, or the splash is off): the
            // greeter alone, tagged when asked.
            None => (
                None,
                None,
                (self.debug_tags && !hw_cursor).then(|| {
                    splash::Tag::new(
                        &self.scene,
                        &info.connector,
                        (scene_width, scene_height),
                        transform,
                    )
                }),
            ),
        };
        if background.is_none()
            && let Some(cover) = cover.as_mut()
        {
            // Nothing to wait for: the greeter is ready as it stands.
            cover.mark_ready();
        }
        self.entries.push(Entry {
            id,
            connector: info.connector.clone(),
            description: info.description(),
            // Scene dimensions: pointer routing works in the space the user
            // sees, so a portrait monitor must present as portrait here.
            width: scene_width,
            height: scene_height,
            hw_cursor,
            present_failures: 0,
            presenter,
            window,
            splash,
            cover,
            tag,
            shown_once: false,
            scratch: Vec::new(),
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
                        .and_then(Option::as_ref)
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
        if let Some(origin) = self.layout.iter().find(|output| output.position == (0, 0))
            && let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.connector == origin.name)
        {
            self.panel = index;
            let (width, height) = self.entries[index].window.scene_size();
            self.cursor = self.row.clamp(
                f64::from(origin.position.0) + f64::from(width) / 2.0,
                f64::from(origin.position.1) + f64::from(height) / 2.0,
            );
        }
        self.apply_panel();
    }

    fn apply_panel(&mut self) {
        for (i, e) in self.entries.iter_mut().enumerate() {
            let on_panel = i == self.panel;
            e.window.set_panel_visible(on_panel);
            // The panel output is by construction the one under the pointer.
            if e.hw_cursor {
                let (px, py) = e.window.pointer();
                e.presenter
                    .set_cursor(on_panel.then_some((px as i32, py as i32)));
                e.window.set_cursor_visible(false);
            } else {
                e.window.set_cursor_visible(on_panel);
            }
        }
    }

    fn route(&mut self, events: Vec<InputEvent>) {
        // XF86Switch_VT_1..=XF86Switch_VT_12: the greeter owns Ctrl+Alt+Fn
        // because taking libinput disabled the kernel's handling. Without
        // this the greeter VT is a roach motel.
        const VT_FIRST: u32 = 0x1008_FE01;
        const VT_LAST: u32 = 0x1008_FE0C;
        const ESCAPE: u32 = 0xff1b;
        if self.leaving.is_some() {
            return;
        }
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
            if e.hw_cursor {
                e.presenter.set_cursor(Some((lx as i32, ly as i32)));
            }
        }
    }

    fn pump_messages(&mut self) {
        if self.leaving.is_some() {
            // Signed in: nothing typed from here on can matter.
            self.queue.borrow_mut().clear();
            return;
        }
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
                self.begin_leaving();
                return;
            }
        }
    }

    /// start_session was accepted. greetd runs the session only once this
    /// process exits, so the one job left is to hand every screen back to
    /// the rest frame (the next stage starts on it) and get out of the way,
    /// not to tear down gracefully.
    fn begin_leaving(&mut self) {
        let now = Instant::now();
        self.leaving = Some(now);
        self.timeline = None;
        eprintln!(
            "vigil: [t+{}ms] login accepted; easing to the rest frame, exiting once shown",
            vigil_core::uptime_ms()
        );
        for entry in self.entries.iter_mut() {
            if entry.splash.is_some() {
                entry.cover = Some(splash::Cover::new());
            }
        }
    }

    /// Drive the hand-off to the rest frame; exit the process as soon as
    /// every screen that has a splash shows it and the flip had a vblank to
    /// land. One timeline for all outputs: every frame in a pass uses the
    /// same `now`.
    fn render_leaving(&mut self) {
        let now = Instant::now();
        let started = self.leaving.unwrap_or(now);
        let waited = now.duration_since(started);
        let fade = self.scene.stage_fade_s.max(0.0);
        let progress = if fade <= 0.0 {
            1.0
        } else {
            scene::ease(waited.as_secs_f64() / fade)
        };
        let last_frame = progress >= 1.0;
        let mut pending = false;
        let deadline = Duration::from_millis(500) + Duration::from_secs_f64(fade);
        let mut expired = false;
        for entry in self.entries.iter_mut() {
            // A slow output must not stretch the exit: the deadline is
            // measured on the clock before every entry, not once per pass.
            if started.elapsed() >= deadline {
                expired = true;
                break;
            }
            let Entry {
                presenter,
                window,
                splash,
                cover,
                tag,
                scratch,
                connector,
                ..
            } = entry;
            let (Some(sp), Some(cover)) = (splash.as_ref(), cover.as_mut()) else {
                continue;
            };
            if !cover.final_shown() && cover.due(now) {
                // The greeter as it stands, with the rest frame mixed in.
                window.request_present();
                let t0 = Instant::now();
                let mut drawn = None;
                let shown = presenter.with_frame(&mut |canvas| match canvas {
                    Canvas::Cpu(mut target) => {
                        let ok = if last_frame {
                            // Write-only: the rest frame replaces the buffer.
                            sp.draw_full(&mut target, 0.0)
                        } else {
                            composed(scratch, &mut target, |frame| {
                                window.render_if_needed(FrameTarget {
                                    buffer: &mut frame.buffer[..],
                                    width: frame.width,
                                    height: frame.height,
                                    stride: frame.stride,
                                }) && {
                                    sp.blend_over(frame, progress as f32);
                                    true
                                }
                            })
                        };
                        if !ok {
                            return false;
                        }
                        if let Some(tag) = tag {
                            tag.draw(&mut target, splash::Stage::Greeter);
                        }
                        drawn = Some(t0.elapsed());
                        true
                    }
                    _ => false,
                });
                let total = t0.elapsed();
                match shown {
                    Ok(true) => {
                        let draw = drawn.unwrap_or_default();
                        eprintln!(
                            "vigil: [t+{}ms] leave frame on {connector}: draw {} ms, commit {} ms, progress {progress:.2}",
                            vigil_core::uptime_ms(),
                            draw.as_millis(),
                            total.saturating_sub(draw).as_millis()
                        );
                        cover.presented(now, progress);
                        if last_frame {
                            cover.set_final_shown();
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("vigil: hand-off to rest: {e}"),
                }
            }
            if cover.final_shown() {
                // Shown means on the display, not just handed to the kernel:
                // the process must not exit while the last flip is in flight.
                pending |= presenter.flip_pending();
            } else {
                pending = true;
            }
        }
        let waited = started.elapsed();
        if expired || !pending || waited >= deadline {
            eprintln!(
                "vigil: [t+{}ms] exiting (rest frame shown, {} ms after login)",
                vigil_core::uptime_ms(),
                waited.as_millis()
            );
            self.hold_last_frame();
            // No teardown: the kernel closes the DRM and input fds, and
            // every millisecond here is a millisecond of the session not
            // starting.
            std::process::exit(self.exit_code);
        }
    }

    /// `[render] hold_last_frame_on_exit`: leave a detached child holding the
    /// DRM fds so the rest frame stays up after this process exits. Every
    /// failure is a log line and a normal exit.
    fn hold_last_frame(&self) {
        let Some(timeout) = self.hold_timeout else {
            return;
        };
        let devices: Vec<hold::Device> = self
            .outputs
            .iter()
            .flatten()
            .filter(|gpu| gpu.alive())
            .map(|gpu| {
                let (fd, crtcs) = gpu.hold_snapshot();
                hold::Device { fd, crtcs }
            })
            .collect();
        let t0 = vigil_core::uptime_ms();
        match hold::start(&devices, timeout) {
            Ok(pid) => {
                // One write, so the child's first line cannot land inside it.
                use std::io::Write as _;
                let line = format!(
                    "vigil: [t+{}ms] hold: child {pid} keeps the last frame on {} screen(s) for up to {} ms ({} ms to set up)\n",
                    vigil_core::uptime_ms(),
                    devices.iter().map(|d| d.crtcs.len()).sum::<usize>(),
                    timeout.as_millis(),
                    vigil_core::uptime_ms().saturating_sub(t0)
                );
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            Err(e) => eprintln!("vigil: hold: not holding the last frame, exiting normally: {e}"),
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
        self.appearance_registry = default
            .as_deref()
            .map(appearance_profiles::Registry::load_published)
            .transpose()
            .unwrap_or_else(|e| {
                eprintln!("vigil: appearance registry: {e}");
                None
            })
            .unwrap_or_default();
        self.appearance_bundle = default
            .as_deref()
            .and_then(|name| appearance_profiles::PreparedBundle::load_published(name).ok())
            .flatten();
        self.background_cache_key = default.clone().unwrap_or_else(|| "<theme>".to_owned());
        self.refresh_backgrounds_async();
        let label = default.clone().unwrap_or_default();
        for e in self.entries.iter_mut() {
            e.window.set_user_index(index);
            e.window.set_user_name(&label);
        }
        let mut fan = FanUi {
            entries: &mut self.entries,
            snapshot: &mut self.snapshot,
        };
        if let Err(e) = self.auth.select_user(default.as_deref(), &mut fan) {
            eprintln!("vigil: select user: {e}");
        }
    }

    fn refresh_backgrounds_async(&mut self) {
        self.background_generation = self.background_generation.wrapping_add(1);
        let generation = self.background_generation;
        let cache_key = self.background_cache_key.clone();
        if let Some(outputs) = self.background_cache.get(&cache_key).cloned() {
            self.apply_backgrounds(outputs);
            return;
        }
        let mut requests = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let identity = appearance_profiles::OutputIdentity::new(
                &entry.connector,
                entry.description.clone(),
            );
            let resolved = self.appearance_registry.resolve(&identity, None);
            let resolved_path = resolved.path.clone();
            let resolved_fit = resolved.fit;
            let (background, fit) = self.looks.for_connector_with_fallback(
                &entry.connector,
                resolved.path,
                Some(appearance_fit(resolved.fit)),
            );
            let (width, height) = entry.window.scene_size();
            let prepared = (background.as_ref() == resolved_path.as_ref()
                && fit == appearance_fit(resolved_fit))
            .then(|| {
                self.appearance_bundle
                    .as_ref()
                    .and_then(|bundle| bundle.resolve(&identity, width, height, resolved_fit))
                    .cloned()
            })
            .flatten()
            .filter(|prepared| {
                prepared.format != appearance_profiles::PixelFormat::Xrgb8888Le
                    || entry.window.supports_native_background()
            });
            requests.push((entry.id, background, fit, width, height, prepared));
        }
        let tx = self.background_tx.clone();
        let wake = self.wake.clone();
        let cache_dir = self.bg_cache_dir.clone();
        std::thread::spawn(move || {
            use std::collections::HashMap;
            use std::sync::{Arc, OnceLock};
            // One lazily decoded source per file, shared by every output
            // that needs it. Lazy so a full cache hit never decodes at all.
            type Source = OnceLock<Result<Arc<vigil_ui::BackgroundImage>, String>>;
            let mut sources: HashMap<PathBuf, Source> = HashMap::new();
            for (_, path, _, _, _, prepared) in &requests {
                if prepared.is_some() {
                    continue;
                }
                if let Some(path) = path {
                    sources.entry(path.clone()).or_default();
                }
            }
            let started = Instant::now();
            let cache_dir = cache_dir.as_deref();
            let sources = &sources;
            let outputs = std::thread::scope(|scope| {
                requests
                    .iter()
                    .map(|(id, path, fit, width, height, prepared)| {
                        let (id, fit, width, height) = (*id, *fit, *width, *height);
                        let prepared = prepared.clone();
                        let source = path.as_ref().map(|path| (path, &sources[path]));
                        scope.spawn(move || {
                            let decoded = if let Some(prepared) = prepared {
                                appearance_profiles::read_prepared_asset(&prepared)
                                    .map(|asset| {
                                        Some(match asset.format {
                                            appearance_profiles::PixelFormat::Rgba8 => {
                                                BackgroundBitmap::Rgba(asset.bytes, width, height)
                                            }
                                            appearance_profiles::PixelFormat::Xrgb8888Le => {
                                                BackgroundBitmap::Xrgb(
                                                    asset.bytes.into(),
                                                    width,
                                                    height,
                                                )
                                            }
                                        })
                                    })
                                    .map_err(|error| error.to_string())
                            } else {
                                source
                                    .map(|(path, source)| {
                                        let entry = cache_dir.and_then(|dir| {
                                            vigil_ui::bgcache::entry_path(
                                                dir, path, fit, width, height,
                                            )
                                        });
                                        if let Some(rgba) = entry
                                            .as_deref()
                                            .and_then(|p| vigil_ui::bgcache::read(p, width, height))
                                        {
                                            eprintln!(
                                                "vigil: [t+{}ms] background cache hit {width}x{height}",
                                                vigil_core::uptime_ms()
                                            );
                                            return Ok(BackgroundBitmap::Rgba(rgba, width, height));
                                        }
                                        let source = source.get_or_init(|| {
                                            vigil_ui::load_background(path).map(Arc::new)
                                        });
                                        source
                                            .as_ref()
                                            .map_err(Clone::clone)
                                            .and_then(|source| {
                                                vigil_ui::render_background(
                                                    source, fit, width, height,
                                                )
                                            })
                                            .map(|rgba| {
                                                if let Some(p) = entry.as_deref() {
                                                    vigil_ui::bgcache::write(p, &rgba);
                                                }
                                                BackgroundBitmap::Rgba(rgba, width, height)
                                            })
                                    })
                                    .transpose()
                            };
                            (id, decoded)
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().expect("background renderer panicked"))
                    .collect()
            });
            eprintln!(
                "vigil: [t+{}ms] backgrounds ready ({} ms of work)",
                vigil_core::uptime_ms(),
                started.elapsed().as_millis()
            );
            let _ = tx.send(BackgroundResult {
                generation,
                cache_key,
                outputs,
            });
            wake.wake();
        });
    }

    fn pump_backgrounds(&mut self) {
        while let Ok(result) = self.background_rx.try_recv() {
            self.background_cache
                .insert(result.cache_key.clone(), result.outputs.clone());
            if result.generation != self.background_generation
                || result.cache_key != self.background_cache_key
            {
                continue;
            }
            self.apply_backgrounds(result.outputs);
        }
    }

    fn apply_backgrounds(&mut self, outputs: Vec<(OutputId, DecodedBackground)>) {
        for (id, decoded) in outputs {
            let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
                continue;
            };
            // Whatever the outcome, the greeter has stopped waiting for it.
            if let Some(cover) = entry.cover.as_mut() {
                cover.mark_ready();
            }
            match decoded {
                Ok(Some(BackgroundBitmap::Rgba(rgba, width, height))) => {
                    eprintln!("vigil: background applied to {:?}", id);
                    entry.window.set_background(rgba, width, height)
                }
                Ok(Some(BackgroundBitmap::Xrgb(xrgb, width, height))) => {
                    if entry.window.set_native_background_xrgb(xrgb, width, height) {
                        eprintln!("vigil: native background applied to {:?}", id);
                    } else {
                        eprintln!("vigil: native background unsupported for {:?}", id);
                        entry.window.clear_background();
                    }
                }
                Ok(None) => {
                    eprintln!("vigil: no background resolved for {:?}; using theme", id);
                    entry.window.clear_background()
                }
                Err(error) => {
                    eprintln!("vigil: background: {error}");
                    entry.window.clear_background();
                }
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        for (&id, &(deadline, _)) in &self.present_retry {
            if deadline <= now {
                self.dirty.mark(id);
            }
        }
        vigil_ui::advance_timers();
        self.pump_backgrounds();
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
        if self.last_clock.0.elapsed() >= clock_interval(&self.clock_format) {
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
            // Shared profile TOML can change while the greeter is up (#31).
            if self.reload_profiles_from_disk() {
                let _ = self.resolve_profile();
                self.rebuild_row();
            }
        }
        self.pump_login();
        self.pump_messages();
        let dirty = self.dirty.take_all();
        self.render(&dirty);
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

    /// The shared timeline's phase at `now`, or None once no output is
    /// covered any more. The timeline is ready when every covered output's
    /// greeter is.
    fn cover_phase(&mut self, now: Instant) -> Option<splash::Phase> {
        let timeline = self.timeline.as_mut()?;
        let mut covered = self
            .entries
            .iter()
            .filter_map(|e| e.cover.as_ref())
            .peekable();
        if covered.peek().is_none() {
            self.timeline = None;
            return None;
        }
        if !timeline.is_ready() && covered.all(splash::Cover::is_ready) {
            timeline.mark_ready(now);
            eprintln!(
                "vigil: [t+{}ms] greeter ready on every output; easing the glow out",
                vigil_core::uptime_ms()
            );
        }
        let phase = timeline.phase(now, scene::monotonic_s(), &self.scene);
        if phase == splash::Phase::Done {
            eprintln!(
                "vigil: [t+{}ms] splash to greeter fade done",
                vigil_core::uptime_ms()
            );
            self.timeline = None;
        }
        Some(phase)
    }

    fn render(&mut self, dirty: &[OutputId]) {
        if !self.active {
            return;
        }
        if self.leaving.is_some() {
            self.render_leaving();
            return;
        }
        let mut dead = Vec::new();
        // One timeline for every covered output, sampled once per pass: no
        // screen runs ahead of another because its neighbour was slow to draw.
        let now = Instant::now();
        let phase = self.cover_phase(now);
        // `dirty` is advisory (see vigil-wayland: the software adapter cannot
        // intercept Slint's request_redraw, so gating on it froze animations
        // and input on metal). Every entry is offered a frame, but the scene
        // is probed first (ADR 0007) so a clean output costs a shadow
        // repaint instead of a buffer map that is then discarded.
        let _ = dirty;
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let Entry {
                presenter,
                window,
                present_failures,
                splash,
                cover,
                tag,
                shown_once,
                scratch,
                ..
            } = entry;
            // The splash covers a fresh output until the greeter is ready on
            // every output, then the whole set cross-fades off together.
            let mut fade: Option<f32> = None;
            if let (Some(c), Some(phase)) = (cover.as_mut(), phase) {
                match phase {
                    splash::Phase::Glow(level) => {
                        if let Some(sp) = splash.as_ref()
                            && c.due(now)
                            && c.changes(level)
                        {
                            let full = c.needs_full();
                            let shown = presenter.with_frame(&mut |canvas| match canvas {
                                Canvas::Cpu(mut target) => {
                                    if full {
                                        let ok = sp.draw_full(&mut target, level);
                                        if let (true, Some(tag)) = (ok, tag.as_ref()) {
                                            tag.draw(&mut target, splash::Stage::GreeterSplash);
                                        }
                                        ok
                                    } else {
                                        sp.draw_glow(&mut target, level)
                                    }
                                }
                                _ => false,
                            });
                            match shown {
                                Ok(true) => c.presented(now, level),
                                Ok(false) => {}
                                Err(PresentError::DeviceLost) => dead.push(i),
                                Err(e) => eprintln!("vigil: splash present: {e}"),
                            }
                        }
                        // While the glow eases out, the greeter's first
                        // frame is drawn off screen so the fade starts on it.
                        if self
                            .timeline
                            .as_ref()
                            .is_some_and(|t| t.easing_out(now, &self.scene))
                        {
                            let _ = window.scene_needs_present();
                        }
                        continue;
                    }
                    splash::Phase::Fade(alpha) => {
                        fade = Some(alpha);
                        window.request_present();
                    }
                    splash::Phase::Done => {
                        *cover = None;
                        window.request_present();
                    }
                }
            }
            let debug_frames = std::env::var_os("VIGIL_DEBUG_FRAMES").is_some();
            // Probe before asking the presenter for a frame. On DRM that
            // costs a dumb-buffer map (ioctl + mmap) or a GBM surface
            // round; on a clean scene the draw closure would then return
            // false and all of it is thrown away. The lock binary had the
            // same shape against Wayland, where the discarded buffer's
            // destroy is answered with delete_id and wakes the loop that
            // acquired it -- desktop-commons ADR 0007. There is no
            // compositor here to reply, so this is waste rather than a
            // spin, but the rule is the same for every presenter.
            if !window.scene_needs_present() {
                continue;
            }
            self.metrics.record_buffer_acquire();
            match presenter.with_frame(&mut |canvas| {
                // Either canvas: the window knows which backend it has, and
                // the presenter hands out the matching kind.
                let Canvas::Cpu(mut target) = canvas else {
                    return window.render(canvas);
                };
                let (mid, row_len) = (
                    target.stride * (target.height as usize / 2),
                    target.width as usize * 4,
                );
                // The fade reads the greeter back to mix the rest frame
                // over it, so it composes on a CPU-side frame (see
                // `composed`) and only the finished picture reaches the
                // mapping.
                let drew = match (fade, splash.as_ref()) {
                    (Some(alpha), Some(sp)) => composed(scratch, &mut target, |frame| {
                        let drew = window.render_if_needed(FrameTarget {
                            buffer: &mut frame.buffer[..],
                            width: frame.width,
                            height: frame.height,
                            stride: frame.stride,
                        });
                        if drew {
                            sp.blend_over(frame, alpha);
                            if let Some(tag) = tag.as_ref() {
                                // Whichever stage holds more of the picture
                                // owns the tag.
                                let stage = if alpha > 0.5 {
                                    splash::Stage::GreeterSplash
                                } else {
                                    splash::Stage::Greeter
                                };
                                tag.draw(frame, stage);
                            }
                        }
                        drew
                    }),
                    _ => {
                        let drew = window.render_if_needed(FrameTarget {
                            buffer: &mut target.buffer[..],
                            width: target.width,
                            height: target.height,
                            stride: target.stride,
                        });
                        if drew && let Some(tag) = tag.as_ref() {
                            tag.draw(&mut target, splash::Stage::Greeter);
                        }
                        drew
                    }
                };
                if drew && debug_frames {
                    let row = &target.buffer[mid..mid + row_len];
                    let sum: u64 = row.iter().map(|&b| b as u64).sum();
                    eprintln!("vigil: frame drawn, mid-row byte sum {sum}");
                }
                drew
            }) {
                Ok(drew) => {
                    if drew {
                        *shown_once = true;
                        self.metrics.record_render();
                        self.metrics.record_commit();
                    }
                    self.present_retry.remove(&entry.id);
                    *present_failures = 0
                }
                Err(PresentError::DeviceLost) => dead.push(i),
                Err(e) => {
                    // The frame was drawn and the scene's dirty flag consumed,
                    // but nothing reached the CRTC. Re-arm the scene, while
                    // preserving the presenter's modeset state: turning a
                    // failed page flip into a full modeset can race an in-flight
                    // flip and produces an endless ENOMEM loop on amdgpu.
                    // Resume/VT activation explicitly invalidate presenters at
                    // the point where a modeset is actually required.
                    window.request_present_deferred();
                    let exponent = (*present_failures).min(5);
                    let delay = Duration::from_millis(100 * (1_u64 << exponent));
                    self.present_retry
                        .insert(entry.id, (Instant::now() + delay, *present_failures));
                    // A persistent failure retries every frame; do not narrate
                    // it every frame.
                    if *present_failures % PRESENT_LOG_EVERY == 0 {
                        eprintln!("vigil: present: {e} (retrying)");
                    }
                    *present_failures = present_failures.saturating_add(1);
                }
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
        self.maybe_first_frame_hook();
    }

    /// Run `[splash] on_first_frame` once, when every output shows a frame
    /// of ours and no flip is still in flight.
    fn maybe_first_frame_hook(&mut self) {
        if self.first_frame_hook_done || self.entries.is_empty() {
            return;
        }
        let up = self.entries.iter().all(|e| {
            let shown = match &e.cover {
                Some(c) => c.shown(),
                None => e.shown_once,
            };
            shown && !e.presenter.flip_pending()
        });
        if !up {
            return;
        }
        self.first_frame_hook_done = true;
        eprintln!(
            "vigil: [t+{}ms] first frame presented on every output",
            vigil_core::uptime_ms()
        );
        let cmd = self.splash_cfg.on_first_frame.trim().to_owned();
        if cmd.is_empty() {
            return;
        }
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                // Reaped off the render path.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => eprintln!("vigil: on_first_frame: {e}"),
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
    vigil_core::uptime_ms();
    let process_start = Instant::now();
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
    let profiles_dir = config.profiles.dir.clone();
    let (profiles, profiles_fingerprint) = match &profiles_dir {
        Some(dir) => {
            let (profiles, diags) = monitor_profiles::load_dir(dir);
            for d in diags {
                eprintln!("vigil: profile {}: {}", d.source, d.message);
            }
            let fingerprint = profiles_dir_fingerprint(dir);
            (profiles, fingerprint)
        }
        None => (Vec::new(), 0),
    };

    eprintln!(
        "vigil: renderer requested: {} (per-output result logged below)",
        if config.render.backend.is_empty() {
            "software"
        } else {
            &config.render.backend
        }
    );

    let (session, notifier) = SessionManager::new().map_err(|e| e.to_string())?;
    let seat = session.seat_name();
    let mut session = session;

    // One manager per GPU on the seat, primary first — outputs can span
    // cards (found on the founding machine: the dock's DP hangs off the
    // dGPU while boot_vga drives the panel). A card that fails to open or
    // init is skipped with a log line, not fatal: a greeter with fewer
    // monitors beats no greeter.
    let mut outputs = Vec::new();
    let mut drm_notifiers = Vec::new();
    for path in vigil_outputs::all_gpu_paths(&seat).map_err(|e| e.to_string())? {
        let namespace = outputs.len() as u32;
        let manager = session
            .open_device(&path)
            .map_err(|e| e.to_string())
            .and_then(|fd| {
                OutputManager::with_display_state(fd, namespace, config.render.keep_display_state)
                    .map_err(|e| e.to_string())
            });
        match manager {
            Ok((manager, drm_notifier)) => {
                outputs.push(Some(manager));
                drm_notifiers.push((namespace, drm_notifier));
            }
            Err(e) => eprintln!("vigil: skipping GPU {}: {e}", path.display()),
        }
    }
    if outputs.is_empty() {
        return Err("no usable GPU on the seat".into());
    }
    let udev = vigil_outputs::udev_monitor(&seat).map_err(|e| e.to_string())?;

    // The splash goes up first, on every output at once: registering the
    // toolkit below blocks for about a second, and the greeter windows need
    // it. The screens sit on the rest frame meanwhile.
    let splash_cfg = match &cli.splash_config {
        Some(path) => vigil_config::Splash::load_file(path),
        None => config.splash.clone(),
    };
    let scene = splash::resolve_scene(&splash_cfg);
    let debug_tags = splash_cfg.debug_tags || scene.debug_from_env();
    let want_gl = config.render.backend.eq_ignore_ascii_case("gl");
    let early = splash_bringup(
        &mut outputs,
        &profiles,
        &splash_cfg,
        &scene,
        debug_tags,
        config.render.keep_display_state,
        !want_gl,
    );
    if !early.outputs.is_empty() {
        eprintln!(
            "vigil: [t+{}ms] splash up on {} output(s)",
            vigil_core::uptime_ms(),
            early.outputs.len()
        );
    }

    let platform = VigilPlatform::new();

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
    let signal = event_loop.get_signal();
    let wake_signal = signal.clone();
    let wake = WakeHandle::new(move || wake_signal.wakeup());
    let dirty = std::sync::Arc::new(DirtySet::new());
    let metrics = std::sync::Arc::new(Metrics::new());

    // Sleep signals on a worker thread (the vigil-pam pattern: the event loop
    // is calloop-driven and must never block on D-Bus). logind being absent
    // is survivable — the greeter then behaves exactly as it did before, so
    // this must never be fatal.
    let (login_tx, login_worker_rx) = std::sync::mpsc::channel();
    let (login_app_tx, login_rx) = std::sync::mpsc::channel();
    let login_wake = wake.clone();
    std::thread::spawn(move || {
        while let Ok(event) = login_worker_rx.recv() {
            if login_app_tx.send(event).is_err() {
                break;
            }
            login_wake.wake();
        }
    });
    let login = LoginSession::connect_for("vigil");
    if let Some(login) = &login {
        login.spawn_sleep_signals(login_tx);
    }

    let appearance_user = resolved.user.as_deref().or_else(|| {
        user_list
            .get(selected_user)
            .filter(|name| name.as_str() != OTHER_USER)
            .map(String::as_str)
    });
    let appearance_registry = appearance_user
        .and_then(|name| appearance_profiles::Registry::load_published(name).ok())
        .unwrap_or_default();
    let appearance_bundle = appearance_user
        .and_then(|name| appearance_profiles::PreparedBundle::load_published(name).ok())
        .flatten();
    let background_cache_key = appearance_user.unwrap_or("<theme>").to_owned();
    let bg_cache_dir = config
        .sessions
        .state_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("bg-cache"));
    let (background_tx, background_rx) = std::sync::mpsc::channel();
    let mut app = App {
        session,
        outputs,
        profiles_dir,
        profiles_fingerprint,
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
        theme: None,
        platform,
        entries: Vec::new(),
        row: layout::Row::default(),
        cursor: (0.0, 0.0),
        panel: 0,
        queue: Rc::new(RefCell::new(VecDeque::new())),
        looks: vigil_ui::Looks {
            cli_background: cli.background.clone(),
            fallback_background: None,
            cli_fit: cli.bg_mode,
            config: config.clone(),
        },
        appearance_registry,
        appearance_bundle,
        background_tx,
        background_rx,
        background_generation: 0,
        background_cache: Default::default(),
        background_cache_key,
        power_enabled: resolved.power_enabled,
        clock_format: resolved.clock_format.clone(),
        banner_file: config.greeter.banner_file.clone(),
        banner: String::new(),
        last_banner: Instant::now(),
        caps_lock: false,
        last_clock: (Instant::now(), clock_text(&resolved.clock_format)),
        snapshot: UiSnapshot::default(),
        active: true,
        login_rx,
        signal,
        wake,
        dirty,
        metrics,
        scheduler: IdleScheduler::default(),
        running: true,
        present_retry: Default::default(),
        exit_code: 1,
        splash_cfg,
        scene,
        debug_tags,
        timeline: None,
        early: early.outputs,
        pending_events: early.events,
        first_frame_hook_done: false,
        started: process_start,
        leaving: None,
        bg_cache_dir,
        hold_timeout: config
            .render
            .hold_last_frame_on_exit
            .then(|| config.render.hold_timeout()),
    };

    if !app.early.is_empty() {
        app.timeline = Some(splash::Timeline::new(app.started, &app.splash_cfg));
    }
    // The slow part, with the splash already on every screen.
    app.platform.register().map_err(|e| e.to_string())?;
    // Validating the theme instantiates it once, before any runtime exists.
    app.theme = Some(Theme::load_or_default(resolved.theme.as_deref()));
    app.platform
        .set_runtime(app.wake.clone(), app.dirty.clone(), app.metrics.clone());

    // Rescan also starts the background job for the outputs it brought up
    // (decode once, scale in parallel, or read the scaled-wallpaper cache),
    // so the first greeter frame is not held behind an image decode.
    app.rescan();
    app.refresh_banner();
    {
        let mut fan = FanUi {
            entries: &mut app.entries,
            snapshot: &mut app.snapshot,
        };
        if let Some(user) = resolved.user.as_deref() {
            app.auth.begin(Some(user), &mut fan)
        } else if !app.users.is_empty() {
            let selected = app
                .users
                .get(app.selected_user)
                .filter(|name| name.as_str() != OTHER_USER)
                .cloned();
            app.auth.select_user(selected.as_deref(), &mut fan)
        } else {
            app.auth.begin(None, &mut fan)
        }
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
    // Page-flip completion events re-open each presenter's submission gate
    // (Presenter::vblank). Without this, flips under load race the vblank
    // into EBUSY and the recovery modeset dies with ENOMEM on amdgpu —
    // found on metal the moment continuous pointer motion met the cursor
    // plane (#25).
    for (namespace, notifier) in drm_notifiers {
        handle
            .insert_source(notifier, move |event, _, app: &mut App| match event {
                vigil_outputs::DrmEvent::VBlank(crtc) => app.vblank(namespace, crtc.into()),
                vigil_outputs::DrmEvent::Error(e) => eprintln!("vigil: drm event: {e}"),
            })
            .map_err(|e| format!("drm notifier source: {e}"))?;
    }

    handle
        .insert_source(
            Generic::new(input_fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                let events = app.input.dispatch();
                // Never log event contents: keystrokes include the password.
                if std::env::var_os("VIGIL_DEBUG_FRAMES").is_some() && !events.is_empty() {
                    eprintln!("vigil: input ready: {} event(s)", events.len());
                }
                let had_events = !events.is_empty();
                app.route(events);
                // Input must unpause the render loop. The GL path bridges
                // Slint redraw requests into the dirty set via
                // set_redraw_handle, but the software path has no reliable
                // bridge — observed on metal: 49k input callbacks, zero
                // frames, a greeter that paints once and then ignores the
                // user forever. route() leaves self.panel on the output
                // under the cursor/keyboard, so mark that one explicitly.
                if had_events && let Some(entry) = app.entries.get(app.panel) {
                    app.dirty.mark(entry.id);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("input source: {e}"))?;

    // A static scene has no frame timer. calloop blocks until input, DRM,
    // lifecycle/worker wakeups, or a real Slint/application deadline.
    while app.running {
        let timeout = app.wait_timeout();
        event_loop
            .dispatch(timeout, &mut app)
            .map_err(|e| format!("event loop dispatch: {e}"))?;
        app.wake.acknowledge();
        app.metrics.record_wake();
        app.tick();
    }

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
    fn composed_writes_the_finished_frame_and_a_refusal_writes_nothing() {
        let (w, h, stride) = (3u32, 2u32, 16usize);
        let mut buf = vec![7u8; stride * h as usize];
        let mut target = FrameTarget {
            buffer: &mut buf[..],
            width: w,
            height: h,
            stride,
        };
        let mut scratch = Vec::new();
        // Refused: the mapping is untouched.
        assert!(!composed(&mut scratch, &mut target, |f| {
            f.buffer.fill(1);
            false
        }));
        assert!(target.buffer.iter().all(|&b| b == 7));
        // Accepted: the scratch frame has the target's stride and lands whole.
        assert!(composed(&mut scratch, &mut target, |f| {
            assert_eq!((f.stride, f.buffer.len()), (stride, stride * 2));
            f.buffer.fill(9);
            true
        }));
        assert!(target.buffer.iter().all(|&b| b == 9));
        // A target too small for its own geometry is refused, not indexed.
        let mut short = [0u8; 8];
        let mut small = FrameTarget {
            buffer: &mut short[..],
            width: w,
            height: h,
            stride,
        };
        assert!(!composed(&mut scratch, &mut small, |_| true));
    }

    #[test]
    fn static_clock_uses_minute_deadline() {
        assert_eq!(clock_interval("%H:%M"), Duration::from_secs(60));
        assert_eq!(clock_interval("%H:%M:%S"), Duration::from_secs(1));
    }

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
            splash_config: None,
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
            splash_config: None,
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
            splash_config: None,
            cmd: Vec::new(),
        };
        let config = Config::default();
        let resolved = resolve(&cli, &config);
        assert!(resolved.power_enabled);
        assert_eq!(resolved.clock_format, "%H:%M");
        assert_eq!(resolved.user, None);
    }
}
