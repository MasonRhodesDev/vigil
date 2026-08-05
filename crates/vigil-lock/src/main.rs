//! vigil-lock: session lockscreen sharing vigil's theme and auth seams
//! (DESIGN.md §12). Policy layer only: vigil-wayland owns the protocol,
//! vigil-pam owns authentication, vigil-ui/-theme own the scene.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vigil_core::{
    AuthEvent, AuthUi, BackgroundFit, FrameTarget, InputEvent, OutputId, OutputInfo, UiMessage,
};
use vigil_pam::PamAttempt;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, UiSnapshot, VigilPlatform};
use vigil_wayland::{LockOutcome, LockSession};

struct Cli {
    user: String,
    theme: Option<PathBuf>,
    background: Option<PathBuf>,
    bg_mode: BackgroundFit,
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
        theme: None,
        background: None,
        bg_mode: BackgroundFit::default(),
        ready_fd: None,
        daemonize: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--user" => cli.user = value("--user")?,
            "--theme" => cli.theme = Some(PathBuf::from(value("--theme")?)),
            "--background" => cli.background = Some(PathBuf::from(value("--background")?)),
            "--bg-mode" => {
                let v = value("--bg-mode")?;
                cli.bg_mode = BackgroundFit::parse(&v).ok_or(format!("unknown bg-mode {v}"))?;
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
    background: Option<PathBuf>,
    bg_mode: BackgroundFit,
    caps_lock: bool,
    queue: Rc<std::cell::RefCell<VecDeque<UiMessage>>>,
    auth_rx: mpsc::Receiver<AuthEvent>,
    auth_tx: mpsc::Sender<AuthEvent>,
    attempt: Option<PamAttempt>,
    unlocked: bool,
    last_clock: (Instant, String),
    ready_fd: Option<i32>,
    /// Auth state to replay onto scenes rebuilt mid-lock (resume/resize
    /// recreates outputs; a fresh theme instance starts blank).
    snapshot: UiSnapshot,
}

impl Locker {
    fn new(cli: Cli) -> Result<Self, String> {
        let platform = VigilPlatform::install().map_err(|e| e.to_string())?;
        let theme = Theme::load_or_default(cli.theme.as_deref());
        let (auth_tx, auth_rx) = mpsc::channel();
        Ok(Self {
            platform,
            theme,
            entries: Vec::new(),
            panel: 0,
            user: cli.user,
            background: cli.background,
            bg_mode: cli.bg_mode,
            caps_lock: false,
            queue: Rc::default(),
            auth_rx,
            auth_tx,
            attempt: None,
            unlocked: false,
            last_clock: (Instant::now(), clock_text()),
            ready_fd: cli.ready_fd,
            snapshot: UiSnapshot::default(),
        })
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
                    self.unlocked = true;
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
                UiMessage::SelectSession(_) | UiMessage::Power(_) => {}
            }
        }
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
            if let Some(path) = &self.background {
                match vigil_ui::background(path, self.bg_mode, info.width, info.height) {
                    Ok(rgba) => window.set_background(rgba, info.width, info.height),
                    Err(e) => eprintln!("vigil-lock: background: {e}"),
                }
            }
            window.set_clock(&self.last_clock.1);
            window.set_caps_lock(self.caps_lock);
            window.set_panel_visible(false);
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
    }

    fn tick(&mut self) {
        vigil_ui::advance_timers();
        if self.last_clock.0.elapsed() >= Duration::from_secs(1) {
            let text = clock_text();
            if text != self.last_clock.1 {
                self.each_window(|w| w.set_clock(&text));
            }
            self.last_clock = (Instant::now(), text);
        }
        self.pump_auth();
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

fn clock_text() -> String {
    std::process::Command::new("date")
        .arg("+%H:%M")
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
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("vigil-lock: {e}");
            std::process::exit(2);
        }
    };
    if cli.daemonize {
        daemonize();
    }
    let mut locker = match Locker::new(cli) {
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
