#![forbid(unsafe_code)]

use std::time::{Duration, Instant, SystemTime};

pub mod greet;
pub use greet::{
    GreetCmd, GreetConfig, GreetEvent, GreetFlow, GreetOutcome, GreetPhase, GreetdReply,
    SessionChoice, USERNAME_PROMPT,
};

use vigil_config::{Lock, LockTransition};
use vigil_core::InputEvent;
pub use vigil_warning::{ElementSample, WALLPAPER_READY_DEFAULT};
use vigil_warning::{Phase as RampPhase, Reveal, Timeline};

pub const REVEAL_MAP_DEADLINE: Duration = Duration::from_millis(250);
pub const REVEAL_HARD_DEADLINE: Duration = Duration::from_millis(2_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    Unlocked,
    Denied,
    Invalidated,
    Cancelled,
    Terminated(Signal),
}

impl LockOutcome {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Unlocked => 0,
            Self::Invalidated => 1,
            Self::Denied => 2,
            Self::Cancelled => 3,
            Self::Terminated(signal) => 128 + signal.number(),
        }
    }
}

impl std::fmt::Display for LockOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unlocked => f.write_str("Unlocked"),
            Self::Denied => f.write_str("Denied"),
            Self::Invalidated => f.write_str("Invalidated"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Terminated(signal) => write!(f, "Terminated:{}", signal.name()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Interrupt,
    Terminate,
}

impl Signal {
    pub const fn number(self) -> i32 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }

    pub const fn from_number(number: i32) -> Option<Self> {
        match number {
            1 => Some(Self::Hangup),
            2 => Some(Self::Interrupt),
            15 => Some(Self::Terminate),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub elapsed: Duration,
    pub mono: Instant,
    pub wall: SystemTime,
}

impl Now {
    pub fn at(elapsed: Duration) -> Self {
        Self {
            elapsed,
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FlowEvent {
    Input(InputEvent),
    PointerEnter { x: f64, y: f64 },
    OutputAdded,
    OutputGone,
    WallpaperReady(bool),
    CommitRequested,
    LockConfirmed,
    LockDenied,
    LockInvalidated,
    OverlayClosed,
    RevealOverlaysMapped,
    AuthOk,
    AuthErr(String),
    AuthConversationLost,
    AuthLocked { remaining: Option<Duration> },
    LogindUnlock,
    PrepareForSleep(bool),
    Terminate(Signal),
    Tick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FlowCmd {
    RequestSessionLock,
    OverlayProgress { frost: f32, wallpaper: f32 },
    OverlayElements(Vec<ElementSample>),
    ShowPanel(bool),
    DispatchInput(InputEvent),
    StartAuth,
    ShowAuthError(String),
    ShowAccountLocked { remaining: Option<Duration> },
    DetachAuth,
    SetLockedHint(bool),
    SignalReady,
    CreateRevealOverlays,
    ReleaseSessionLock,
    DestroyRevealOverlays,
    Journal(FlowNote),
    PhaseChanged { from: FlowPhase, to: FlowPhase },
    Exit(LockOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowNote {
    WallpaperHoldExpired { after_ms: u64 },
}

impl std::fmt::Display for FlowNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WallpaperHoldExpired { after_ms } => write!(
                f,
                "wallpaper never became ready {after_ms} ms past the scheduled commit; \
                 locking with the scene as-is"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowPhase {
    PreLock,
    Committing,
    Locked,
    RevealPending,
    Revealing,
    Done(LockOutcome),
}

impl std::fmt::Display for FlowPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreLock => f.write_str("PreLock"),
            Self::Committing => f.write_str("Committing"),
            Self::Locked => f.write_str("Locked"),
            Self::RevealPending => f.write_str("RevealPending"),
            Self::Revealing => f.write_str("Revealing"),
            Self::Done(outcome) => write!(f, "Done:{outcome}"),
        }
    }
}

pub struct LockFlow {
    phase: FlowPhase,
    timeline: Option<Timeline>,
    reveal: Option<Reveal>,
    transition: LockTransition,
    grace_secs: u64,
    grace: Option<Grace>,
    progress: (f32, f32),
    elements: Vec<ElementSample>,
    wallpaper_hold_max_ms: u64,
    reveal_entered: Option<Duration>,
    reveal_started: Option<Duration>,
    grace_forbidden: bool,
    unlock_latched: bool,
    wait: Option<Duration>,
    lockout: Option<Lockout>,
}

#[derive(Debug, Clone, Copy)]
struct Lockout {
    clears_at: Instant,
    shown: Duration,
}

fn ceil_secs(remaining: Duration) -> Duration {
    Duration::from_secs(remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0))
}

impl LockFlow {
    pub fn new(now: Now, lock: &Lock, warning_ms_override: Option<u64>) -> (Self, Vec<FlowCmd>) {
        let mut warning = lock.warning.clone();
        if let Some(ms) = warning_ms_override {
            warning.duration_ms = ms;
        }
        let transition = lock.transition.clone();
        let cancelable = warning.duration_ms > 0;
        let warning_hold_max_ms = warning.wallpaper_hold_max_ms;
        let timeline = if cancelable {
            Some(Timeline::new(warning))
        } else if transition.ramps_in() {
            Some(Timeline::new_transition(
                transition.as_warning(warning.frost_alpha, warning.gui.clone()),
            ))
        } else {
            None
        };
        let mut flow = Self {
            phase: if timeline.is_some() {
                FlowPhase::PreLock
            } else {
                FlowPhase::Committing
            },
            timeline,
            reveal: None,
            transition,
            grace_secs: lock.grace_secs,
            grace: None,
            progress: (0.0, 0.0),
            elements: Vec::new(),
            wallpaper_hold_max_ms: warning_hold_max_ms,
            reveal_entered: None,
            reveal_started: None,
            grace_forbidden: false,
            unlock_latched: false,
            wait: None,
            lockout: None,
        };
        let mut cmds = Vec::new();
        if let Some(timeline) = flow.timeline.as_mut() {
            timeline.start(now.elapsed);
            cmds.push(FlowCmd::ShowPanel(false));
            flow.advance(now, &mut cmds);
        } else {
            cmds.push(FlowCmd::RequestSessionLock);
        }
        (flow, cmds)
    }

    pub fn phase(&self) -> FlowPhase {
        self.phase
    }

    pub fn next_wake(&self) -> Option<Duration> {
        self.wait
    }

    pub fn settled(&self) -> bool {
        self.phase == FlowPhase::Locked && self.timeline.is_none() && self.lockout.is_none()
    }

    pub fn step(&mut self, now: Now, event: FlowEvent) -> Vec<FlowCmd> {
        let mut cmds = Vec::new();
        if matches!(self.phase, FlowPhase::Done(_)) {
            return cmds;
        }
        match event {
            FlowEvent::Input(input) => self.on_input(now, input, &mut cmds),
            FlowEvent::PointerEnter { x, y } => {
                if self.phase == FlowPhase::PreLock
                    && let Some(timeline) = self.timeline.as_mut()
                {
                    timeline.pointer_enter(x, y);
                }
            }
            FlowEvent::OutputAdded | FlowEvent::OutputGone => {
                if self.phase == FlowPhase::PreLock
                    && let Some(timeline) = self.timeline.as_mut()
                {
                    timeline.hotplug();
                }
            }
            FlowEvent::WallpaperReady(ready) => {
                if let Some(timeline) = self.timeline.as_mut() {
                    timeline.set_wallpaper_ready(ready, now.elapsed);
                }
            }
            FlowEvent::CommitRequested => {
                if self.phase == FlowPhase::PreLock
                    && let Some(timeline) = self.timeline.as_mut()
                {
                    timeline.request_commit();
                }
            }
            FlowEvent::OverlayClosed => {
                if self.phase == FlowPhase::PreLock
                    && let Some(timeline) = self.timeline.as_mut()
                {
                    if timeline.cancelable() {
                        self.finish(LockOutcome::Cancelled, &mut cmds);
                        return cmds;
                    }
                    timeline.request_commit();
                }
            }
            FlowEvent::LockConfirmed => {
                if matches!(self.phase, FlowPhase::PreLock | FlowPhase::Committing) {
                    self.set_phase(FlowPhase::Locked, &mut cmds);
                    if let Some(timeline) = self.timeline.as_mut() {
                        timeline.locked(now.elapsed);
                    }
                    if self.grace_secs > 0 && !self.grace_forbidden {
                        self.grace = Some(Grace::new(now.mono, now.wall, self.grace_secs));
                    }
                    cmds.push(FlowCmd::SignalReady);
                    if self.unlock_latched {
                        self.unlock_authorized(now, &mut cmds);
                    } else {
                        cmds.push(FlowCmd::ShowPanel(true));
                        cmds.push(FlowCmd::SetLockedHint(true));
                        cmds.push(FlowCmd::StartAuth);
                    }
                }
            }
            FlowEvent::LockDenied => self.finish(LockOutcome::Denied, &mut cmds),
            FlowEvent::LockInvalidated => match self.phase {
                FlowPhase::RevealPending | FlowPhase::Revealing => {
                    cmds.push(FlowCmd::DestroyRevealOverlays);
                    self.finish(LockOutcome::Unlocked, &mut cmds);
                }
                _ => self.finish(LockOutcome::Invalidated, &mut cmds),
            },
            FlowEvent::RevealOverlaysMapped => {
                if self.phase == FlowPhase::RevealPending {
                    self.release_and_start_reveal(now, &mut cmds);
                }
            }
            FlowEvent::AuthOk => {
                if self.phase == FlowPhase::Locked {
                    self.lockout = None;
                    self.unlock_authorized(now, &mut cmds);
                }
            }
            FlowEvent::AuthErr(message) => {
                if self.phase == FlowPhase::Locked {
                    self.lockout = None;
                    cmds.push(FlowCmd::ShowAuthError(message));
                    cmds.push(FlowCmd::StartAuth);
                }
            }
            FlowEvent::AuthLocked { remaining } => {
                if self.phase == FlowPhase::Locked {
                    let shown = remaining.map(ceil_secs);
                    cmds.push(FlowCmd::ShowAccountLocked { remaining: shown });
                    self.lockout = remaining.map(|remaining| Lockout {
                        clears_at: now.mono + remaining,
                        shown: ceil_secs(remaining),
                    });
                    cmds.push(FlowCmd::StartAuth);
                }
            }
            FlowEvent::AuthConversationLost => {
                if self.phase == FlowPhase::Locked {
                    self.lockout = None;
                    cmds.push(FlowCmd::StartAuth);
                }
            }
            FlowEvent::LogindUnlock => match self.phase {
                FlowPhase::Locked => self.unlock_authorized(now, &mut cmds),
                FlowPhase::PreLock | FlowPhase::Committing => self.unlock_latched = true,
                _ => {}
            },
            FlowEvent::PrepareForSleep(true) => {
                self.grace = None;
                self.grace_forbidden = true;
                if self.phase == FlowPhase::PreLock
                    && let Some(timeline) = self.timeline.as_mut()
                {
                    timeline.request_commit();
                }
            }
            FlowEvent::PrepareForSleep(false) => {}
            FlowEvent::Terminate(signal) => {
                self.finish(LockOutcome::Terminated(signal), &mut cmds);
            }
            FlowEvent::Tick => {}
        }
        if matches!(self.phase, FlowPhase::Done(_)) {
            return cmds;
        }
        self.advance(now, &mut cmds);
        cmds
    }

    fn tick_lockout(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) -> Option<Duration> {
        let lockout = self.lockout.as_mut()?;
        let remaining = lockout.clears_at.saturating_duration_since(now.mono);
        let shown = ceil_secs(remaining);
        if shown != lockout.shown {
            lockout.shown = shown;
            cmds.push(FlowCmd::ShowAccountLocked {
                remaining: Some(shown),
            });
        }
        if remaining.is_zero() {
            self.lockout = None;
            return None;
        }
        Some(remaining - Duration::from_secs(shown.as_secs() - 1))
    }

    fn advance(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) {
        match self.phase {
            FlowPhase::PreLock | FlowPhase::Committing | FlowPhase::Locked => {
                let lockout_wake = self.tick_lockout(now, cmds);
                let Some((sample, gui_wake, elements, gui_done)) =
                    self.timeline.as_mut().map(|timeline| {
                        (
                            timeline.sample(now.elapsed),
                            timeline.next_gui_wake(now.elapsed),
                            timeline.element_samples(now.elapsed),
                            timeline.gui_complete(now.elapsed),
                        )
                    })
                else {
                    self.wait = lockout_wake;
                    return;
                };
                self.wait = [sample.next_frame, gui_wake, lockout_wake]
                    .into_iter()
                    .flatten()
                    .min();

                let progress = (sample.frost, sample.wallpaper);
                if self.progress != progress {
                    self.progress = progress;
                    cmds.push(FlowCmd::OverlayProgress {
                        frost: sample.frost,
                        wallpaper: sample.wallpaper,
                    });
                }
                if self.elements != elements {
                    self.elements = elements.clone();
                    cmds.push(FlowCmd::OverlayElements(elements));
                }
                if sample.forced_commit {
                    cmds.push(FlowCmd::Journal(FlowNote::WallpaperHoldExpired {
                        after_ms: self.wallpaper_hold_max_ms,
                    }));
                }
                match sample.phase {
                    RampPhase::Cancelled => {
                        self.finish(LockOutcome::Cancelled, cmds);
                    }
                    _ if sample.should_commit && self.phase == FlowPhase::PreLock => {
                        self.set_phase(FlowPhase::Committing, cmds);
                        cmds.push(FlowCmd::RequestSessionLock);
                    }
                    _ => {}
                }
                if self.phase == FlowPhase::Locked && gui_done {
                    self.timeline = None;
                    self.wait = lockout_wake;
                }
            }
            FlowPhase::RevealPending => {
                let entered = self.reveal_entered.unwrap_or(now.elapsed);
                let waited = now.elapsed.saturating_sub(entered);
                if waited >= REVEAL_MAP_DEADLINE {
                    self.release_and_start_reveal(now, cmds);
                } else {
                    self.wait = Some(REVEAL_MAP_DEADLINE - waited);
                }
            }
            FlowPhase::Revealing => {
                let Some(reveal) = self.reveal.as_ref() else {
                    self.finish(LockOutcome::Unlocked, cmds);
                    return;
                };
                let sample = reveal.sample(now.elapsed);
                let overdue = self
                    .reveal_started
                    .is_some_and(|at| now.elapsed.saturating_sub(at) >= REVEAL_HARD_DEADLINE);
                if sample.done || overdue {
                    cmds.push(FlowCmd::DestroyRevealOverlays);
                    self.finish(LockOutcome::Unlocked, cmds);
                    return;
                }
                self.wait = sample.next_frame;
                let progress = (sample.frost, sample.wallpaper);
                if self.progress != progress {
                    self.progress = progress;
                    cmds.push(FlowCmd::OverlayProgress {
                        frost: sample.frost,
                        wallpaper: sample.wallpaper,
                    });
                }
            }
            FlowPhase::Done(_) => {}
        }
    }

    fn on_input(&mut self, now: Now, input: InputEvent, cmds: &mut Vec<FlowCmd>) {
        match self.phase {
            FlowPhase::PreLock => {
                if let Some(timeline) = self.timeline.as_mut() {
                    timeline.input(&input);
                }
            }
            FlowPhase::Locked => {
                if let Some(grace) = &self.grace
                    && grace.dismisses(&input, now.mono, now.wall)
                {
                    self.unlock_authorized(now, cmds);
                    return;
                }
                cmds.push(FlowCmd::DispatchInput(input));
            }
            _ => {}
        }
    }

    fn unlock_authorized(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) {
        if self.transition.reveals() {
            self.set_phase(FlowPhase::RevealPending, cmds);
        }
        cmds.push(FlowCmd::DetachAuth);
        cmds.push(FlowCmd::SetLockedHint(false));
        if self.transition.reveals() {
            self.reveal = Some(Reveal::new(
                self.transition.reveal_ms(),
                self.transition.reveal_frost_ms(),
                self.transition.easing,
            ));
            self.reveal_entered = Some(now.elapsed);
            let frost = if self.transition.reveal_blurs() {
                1.0
            } else {
                0.0
            };
            self.progress = (frost, 1.0);
            cmds.push(FlowCmd::ShowPanel(false));
            cmds.push(FlowCmd::OverlayProgress {
                frost,
                wallpaper: 1.0,
            });
            cmds.push(FlowCmd::CreateRevealOverlays);
        } else {
            cmds.push(FlowCmd::ReleaseSessionLock);
            self.finish(LockOutcome::Unlocked, cmds);
        }
    }

    fn release_and_start_reveal(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) {
        self.set_phase(FlowPhase::Revealing, cmds);
        cmds.push(FlowCmd::ReleaseSessionLock);
        if let Some(reveal) = self.reveal.as_mut() {
            reveal.start(now.elapsed);
        }
        self.reveal_started = Some(now.elapsed);
        self.wait = Some(Duration::from_millis(vigil_warning::FRAME_INTERVAL_MS));
    }

    fn set_phase(&mut self, to: FlowPhase, cmds: &mut Vec<FlowCmd>) {
        let from = self.phase;
        debug_assert_ne!(from, to, "set_phase called with an unchanged phase");
        self.phase = to;
        cmds.push(FlowCmd::PhaseChanged { from, to });
    }

    fn finish(&mut self, outcome: LockOutcome, cmds: &mut Vec<FlowCmd>) {
        self.set_phase(FlowPhase::Done(outcome), cmds);
        self.timeline = None;
        self.reveal = None;
        self.wait = None;
        cmds.push(FlowCmd::Exit(outcome));
    }
}

pub struct Grace {
    deadline_mono: Instant,
    deadline_wall: SystemTime,
}

impl Grace {
    pub fn new(now_mono: Instant, now_wall: SystemTime, secs: u64) -> Self {
        let secs = Duration::from_secs(secs);
        Self {
            deadline_mono: now_mono + secs,
            deadline_wall: now_wall + secs,
        }
    }

    pub fn dismisses(&self, event: &InputEvent, now_mono: Instant, now_wall: SystemTime) -> bool {
        let live = now_mono < self.deadline_mono && now_wall < self.deadline_wall;
        live && matches!(
            event,
            InputEvent::Key { pressed: true, .. } | InputEvent::PointerButton { pressed: true, .. }
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use vigil_config::{Lock, LockTransition, LockWarning};

    pub(crate) fn key() -> InputEvent {
        InputEvent::Key {
            keysym: 1,
            utf8: None,
            pressed: true,
        }
    }

    pub(crate) fn manual_lock() -> Lock {
        Lock::default()
    }

    pub(crate) fn revealing_lock() -> Lock {
        let mut lock = Lock::default();
        lock.transition.wallpaper_out_ms = 250;
        lock
    }

    pub(crate) fn blurring_reveal_lock() -> Lock {
        let mut lock = revealing_lock();
        lock.transition.frost_out_ms = 200;
        lock
    }

    pub(crate) fn warning_lock(duration_ms: u64) -> Lock {
        Lock {
            warning: LockWarning {
                duration_ms,
                frost_in_ms: 1_500,
                wallpaper_in_ms: 1_500,
                ..LockWarning::default()
            },
            ..Lock::default()
        }
    }

    pub(crate) fn at(ms: u64) -> Now {
        Now::at(Duration::from_millis(ms))
    }

    pub(crate) fn has(cmds: &[FlowCmd], wanted: &FlowCmd) -> bool {
        cmds.contains(wanted)
    }

    fn transitions(cmds: &[FlowCmd]) -> Vec<(FlowPhase, FlowPhase)> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                FlowCmd::PhaseChanged { from, to } => Some((*from, *to)),
                _ => None,
            })
            .collect()
    }

    fn position(cmds: &[FlowCmd], wanted: &FlowCmd) -> usize {
        cmds.iter()
            .position(|cmd| cmd == wanted)
            .unwrap_or_else(|| panic!("{wanted:?} not in {cmds:?}"))
    }

    fn locked_flow(lock: &Lock) -> LockFlow {
        let (mut flow, _) = LockFlow::new(at(0), lock, Some(0));
        let total = lock.transition.in_ms();
        let cmds = flow.step(at(total), FlowEvent::Tick);
        if lock.transition.ramps_in() {
            assert!(has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        }
        let cmds = flow.step(at(total + 10), FlowEvent::LockConfirmed);
        assert!(has(&cmds, &FlowCmd::StartAuth));
        assert!(has(&cmds, &FlowCmd::SignalReady));
        assert_eq!(flow.phase(), FlowPhase::Locked);
        flow
    }

    #[test]
    fn a_settled_lock_arms_no_wake_and_does_no_work() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(3_000), None);
        flow.step(at(3_000), FlowEvent::LockConfirmed);
        flow.step(at(10_000), FlowEvent::Tick);
        assert!(flow.settled(), "phase={:?}", flow.phase());
        assert_eq!(flow.next_wake(), None);
        for minute in 1..=10 {
            let cmds = flow.step(at(10_000 + minute * 60_000), FlowEvent::Tick);
            assert!(
                cmds.is_empty(),
                "a settled lock produced work at minute {minute}: {cmds:?}"
            );
            assert_eq!(flow.next_wake(), None, "re-armed a wake at minute {minute}");
        }
    }

    #[test]
    fn settled_implies_no_wake_on_every_path_into_locked() {
        for (label, lock, warn) in [
            ("manual", manual_lock(), None),
            ("warned", warning_lock(3_000), None),
            ("no-warn override", warning_lock(3_000), Some(0)),
        ] {
            let (mut flow, _) = LockFlow::new(at(0), &lock, warn);
            flow.step(at(3_000), FlowEvent::LockConfirmed);
            flow.step(at(30_000), FlowEvent::Tick);
            assert!(flow.settled(), "{label}: phase={:?}", flow.phase());
            assert_eq!(flow.next_wake(), None, "{label} armed a wake when settled");
        }
    }

    #[test]
    fn manual_lock_ramps_then_commits() {
        let (mut flow, boot) = LockFlow::new(at(0), &manual_lock(), Some(0));
        assert!(has(&boot, &FlowCmd::ShowPanel(false)));
        assert!(!has(&boot, &FlowCmd::RequestSessionLock));
        assert_eq!(flow.phase(), FlowPhase::PreLock);
        let mid = flow.step(at(200), FlowEvent::Tick);
        assert!(
            mid.iter()
                .any(|cmd| matches!(cmd, FlowCmd::OverlayProgress { .. }))
        );
        assert!(!has(&mid, &FlowCmd::RequestSessionLock));
        let end = flow.step(at(400), FlowEvent::Tick);
        assert!(has(&end, &FlowCmd::RequestSessionLock));
        assert_eq!(flow.phase(), FlowPhase::Committing);
    }

    #[test]
    fn unlock_is_instant_by_default_with_no_reveal() {
        let lock = manual_lock();
        let mut flow = locked_flow(&lock);
        let cmds = flow.step(at(10_000), FlowEvent::AuthOk);
        assert!(has(&cmds, &FlowCmd::ReleaseSessionLock), "{cmds:?}");
        assert!(
            has(&cmds, &FlowCmd::Exit(LockOutcome::Unlocked)),
            "unlock exits immediately: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, FlowCmd::CreateRevealOverlays)),
            "no reveal overlays on an instant unlock: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, FlowCmd::OverlayProgress { .. })),
            "no reveal animation frames on an instant unlock: {cmds:?}"
        );
        assert!(
            !transitions(&cmds)
                .iter()
                .any(|(_, to)| *to == FlowPhase::RevealPending),
            "no RevealPending phase: {cmds:?}"
        );
        assert_eq!(flow.phase(), FlowPhase::Done(LockOutcome::Unlocked));
    }

    #[test]
    fn immediate_lock_skips_the_ramp() {
        let lock = Lock {
            transition: LockTransition::immediate(),
            ..Lock::default()
        };
        let (flow, boot) = LockFlow::new(at(0), &lock, Some(0));
        assert!(has(&boot, &FlowCmd::RequestSessionLock));
        assert_eq!(flow.phase(), FlowPhase::Committing);
    }

    #[test]
    fn idle_warning_cancels_on_input_in_any_frame_window() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(10_000), None);
        flow.step(at(100), FlowEvent::Tick);
        let cmds = flow.step(at(105), FlowEvent::Input(key()));
        assert!(
            has(&cmds, &FlowCmd::Exit(LockOutcome::Cancelled)),
            "{cmds:?}"
        );
        assert_eq!(flow.phase(), FlowPhase::Done(LockOutcome::Cancelled));
    }

    #[test]
    fn wallpaper_ready_wake_always_rearms_a_deadline() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(3_000), None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        flow.step(at(2_500), FlowEvent::Tick);
        let cmds = flow.step(at(2_505), FlowEvent::WallpaperReady(true));
        assert!(flow.next_wake().is_some(), "{cmds:?}");
        let cmds = flow.step(at(2_505 + 1_500), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn a_wedged_wallpaper_still_locks_by_the_cap() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 3_000,
                wallpaper_hold_max_ms: 5_000,
                ..warning_lock(3_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        let cmds = flow.step(at(3_500), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock));
        assert!(flow.next_wake().is_some(), "the cap must arm a wake");
        assert_eq!(flow.phase(), FlowPhase::PreLock);
        let cmds = flow.step(at(8_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))),
            "the degradation must not be silent: {cmds:?}"
        );
    }

    #[test]
    fn a_fade_longer_than_the_warning_is_not_cut_short() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 2_000,
                wallpaper_in_ms: 10_000,
                wallpaper_hold_max_ms: 5_000,
                ..warning_lock(2_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        let cmds = flow.step(at(7_000), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        assert!(!cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))));
        let cmds = flow.step(at(10_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn a_late_wallpaper_still_gets_its_full_fade() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 3_000,
                wallpaper_in_ms: 1_500,
                wallpaper_hold_max_ms: 5_000,
                ..warning_lock(3_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        flow.step(at(7_500), FlowEvent::WallpaperReady(true));
        let cmds = flow.step(at(8_000), FlowEvent::Tick);
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))),
            "the wallpaper did arrive: {cmds:?}"
        );
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock));
        let cmds = flow.step(at(9_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn a_cancel_racing_the_cap_does_not_claim_a_degraded_lock() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 3_000,
                wallpaper_hold_max_ms: 5_000,
                ..warning_lock(3_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        let cmds = flow.step(at(8_000), FlowEvent::Input(key()));
        assert!(has(&cmds, &FlowCmd::Exit(LockOutcome::Cancelled)));
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))),
            "no lock was taken, degraded or otherwise: {cmds:?}"
        );
    }

    #[test]
    fn an_absurd_config_neither_panics_nor_instant_commits() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: u64::MAX,
                wallpaper_hold_max_ms: u64::MAX,
                ..warning_lock(1_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        let cmds = flow.step(at(10_000), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
    }

    #[test]
    fn an_early_commit_disarms_the_hold_wake() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 10_000,
                wallpaper_hold_max_ms: 5_000,
                ..warning_lock(10_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        let cmds = flow.step(at(5_000), FlowEvent::CommitRequested);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
        assert_eq!(flow.phase(), FlowPhase::Committing);
        assert_eq!(
            flow.next_wake(),
            None,
            "the hold deadline is meaningless once committed"
        );
    }

    #[test]
    fn the_warn_override_moves_the_cap_with_it() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 3_000,
                wallpaper_in_ms: 1_500,
                wallpaper_hold_max_ms: 5_000,
                ..warning_lock(3_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, Some(20_000));
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        let cmds = flow.step(at(8_000), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        let cmds = flow.step(at(25_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn a_ready_wallpaper_is_unaffected_by_the_cap() {
        let lock = warning_lock(3_000);
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        let cmds = flow.step(at(2_999), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock));
        let cmds = flow.step(at(3_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
        assert!(!cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))));
    }

    #[test]
    fn a_zero_cap_waits_forever_as_before() {
        let lock = Lock {
            warning: LockWarning {
                duration_ms: 3_000,
                wallpaper_hold_max_ms: 0,
                ..warning_lock(3_000).warning
            },
            ..Lock::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        let cmds = flow.step(at(60_000), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock));
        assert_eq!(flow.phase(), FlowPhase::PreLock);
    }

    #[test]
    fn join_request_snaps_transition_to_commit() {
        let (mut flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        flow.step(at(50), FlowEvent::Tick);
        let cmds = flow.step(at(60), FlowEvent::CommitRequested);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn hotplug_cancels_warning_but_commits_transition() {
        let (mut warning, _) = LockFlow::new(at(0), &warning_lock(10_000), None);
        warning.step(at(100), FlowEvent::Tick);
        let cmds = warning.step(at(200), FlowEvent::OutputAdded);
        assert!(has(&cmds, &FlowCmd::Exit(LockOutcome::Cancelled)));

        let (mut transition, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        transition.step(at(50), FlowEvent::Tick);
        let cmds = transition.step(at(60), FlowEvent::OutputAdded);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
    }

    #[test]
    fn denied_lock_never_starts_auth() {
        let (mut flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        flow.step(at(400), FlowEvent::Tick);
        let cmds = flow.step(at(410), FlowEvent::LockDenied);
        assert!(has(&cmds, &FlowCmd::Exit(LockOutcome::Denied)));
        assert!(!cmds.iter().any(|cmd| matches!(cmd, FlowCmd::StartAuth)));
    }

    #[test]
    fn auth_failure_reprompts_auth_success_exits() {
        let mut flow = locked_flow(&revealing_lock());
        let cmds = flow.step(at(600), FlowEvent::AuthErr("denied".into()));
        assert!(has(&cmds, &FlowCmd::ShowAuthError("denied".into())));
        assert!(has(&cmds, &FlowCmd::StartAuth));
        let cmds = flow.step(at(700), FlowEvent::AuthOk);
        assert!(has(&cmds, &FlowCmd::DetachAuth));
        assert!(has(&cmds, &FlowCmd::SetLockedHint(false)));
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays));
        assert_eq!(flow.phase(), FlowPhase::RevealPending);
    }

    #[test]
    fn a_wrong_password_reprompts_exactly_once() {
        let mut flow = locked_flow(&revealing_lock());
        let cmds = flow.step(at(600), FlowEvent::AuthErr("denied".into()));
        assert_eq!(
            cmds.iter()
                .filter(|cmd| matches!(cmd, FlowCmd::ShowAuthError(_)))
                .count(),
            1,
            "{cmds:?}"
        );
        assert_eq!(
            cmds.iter()
                .filter(|cmd| matches!(cmd, FlowCmd::StartAuth))
                .count(),
            1,
            "{cmds:?}"
        );
        let cmds = flow.step(at(700), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");
    }

    #[test]
    fn a_lost_conversation_reopens_without_showing_a_failure() {
        let mut flow = locked_flow(&revealing_lock());
        let cmds = flow.step(at(600), FlowEvent::AuthConversationLost);
        assert!(has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::ShowAuthError(_))),
            "a broken conversation was shown to the user as an auth failure: {cmds:?}"
        );
    }

    #[test]
    fn a_lost_conversation_after_unlock_reopens_nothing() {
        let mut flow = locked_flow(&revealing_lock());
        flow.step(at(700), FlowEvent::AuthOk);
        let cmds = flow.step(at(800), FlowEvent::AuthConversationLost);
        assert!(!has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");
    }

    fn settled_locked_flow(base: Instant) -> LockFlow {
        let mut flow = locked_flow(&revealing_lock());
        flow.step(
            Now {
                elapsed: Duration::from_millis(10_000),
                mono: base,
                wall: SystemTime::now(),
            },
            FlowEvent::Tick,
        );
        assert!(flow.settled(), "the fixture must start frame-quiet");
        flow
    }

    #[test]
    fn a_faillock_lockout_counts_down_instead_of_blaming_the_password() {
        let base = Instant::now();
        let now = |ms: u64| Now {
            elapsed: Duration::from_millis(10_000 + ms),
            mono: base + Duration::from_millis(ms),
            wall: SystemTime::now(),
        };
        let mut flow = settled_locked_flow(base);

        let cmds = flow.step(
            now(0),
            FlowEvent::AuthLocked {
                remaining: Some(Duration::from_millis(9_400)),
            },
        );
        assert!(
            has(
                &cmds,
                &FlowCmd::ShowAccountLocked {
                    remaining: Some(Duration::from_secs(10))
                }
            ),
            "{cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::ShowAuthError(_))),
            "a lockout was shown as an authentication failure: {cmds:?}"
        );
        assert!(has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");

        assert!(!flow.settled());
        assert_eq!(flow.next_wake(), Some(Duration::from_millis(400)));

        let cmds = flow.step(now(400), FlowEvent::Tick);
        assert!(
            has(
                &cmds,
                &FlowCmd::ShowAccountLocked {
                    remaining: Some(Duration::from_secs(9))
                }
            ),
            "{cmds:?}"
        );
        assert_eq!(flow.next_wake(), Some(Duration::from_secs(1)));

        let cmds = flow.step(now(600), FlowEvent::Tick);
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::ShowAccountLocked { .. })),
            "{cmds:?}"
        );

        let cmds = flow.step(now(9_400), FlowEvent::Tick);
        assert!(
            has(
                &cmds,
                &FlowCmd::ShowAccountLocked {
                    remaining: Some(Duration::ZERO)
                }
            ),
            "{cmds:?}"
        );
        assert!(flow.settled(), "a cleared lockout kept the flow awake");
        assert_eq!(flow.next_wake(), None);
        let cmds = flow.step(now(12_000), FlowEvent::Tick);
        assert!(cmds.is_empty(), "{cmds:?}");
    }

    #[test]
    fn an_indefinite_lockout_arms_no_countdown() {
        let base = Instant::now();
        let mut flow = settled_locked_flow(base);
        let cmds = flow.step(
            Now {
                elapsed: Duration::from_millis(10_000),
                mono: base,
                wall: SystemTime::now(),
            },
            FlowEvent::AuthLocked { remaining: None },
        );
        assert!(
            has(&cmds, &FlowCmd::ShowAccountLocked { remaining: None }),
            "{cmds:?}"
        );
        assert!(flow.settled());
        assert_eq!(flow.next_wake(), None);
    }

    #[test]
    fn a_later_verdict_retires_the_countdown() {
        for event in [
            FlowEvent::AuthErr("denied".into()),
            FlowEvent::AuthConversationLost,
        ] {
            let base = Instant::now();
            let now = |ms: u64| Now {
                elapsed: Duration::from_millis(10_000 + ms),
                mono: base + Duration::from_millis(ms),
                wall: SystemTime::now(),
            };
            let mut flow = settled_locked_flow(base);
            flow.step(
                now(0),
                FlowEvent::AuthLocked {
                    remaining: Some(Duration::from_secs(60)),
                },
            );
            assert!(!flow.settled());
            flow.step(now(100), event.clone());
            assert!(flow.settled(), "{event:?} left the countdown running");
            assert_eq!(flow.next_wake(), None);
        }
    }

    #[test]
    fn unlock_during_gui_window_still_reveals_and_exits_by_deadline() {
        let mut flow = locked_flow(&revealing_lock());
        let cmds = flow.step(at(500), FlowEvent::AuthOk);
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays));
        let cmds = flow.step(at(520), FlowEvent::RevealOverlaysMapped);
        assert!(has(&cmds, &FlowCmd::ReleaseSessionLock));
        assert_eq!(flow.phase(), FlowPhase::Revealing);
        let cmds = flow.step(at(520 + 3_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::DestroyRevealOverlays));
        assert!(has(&cmds, &FlowCmd::Exit(LockOutcome::Unlocked)));
    }

    #[test]
    fn reveal_map_timeout_releases_lock_anyway() {
        let mut flow = locked_flow(&revealing_lock());
        flow.step(at(500), FlowEvent::AuthOk);
        assert_eq!(flow.phase(), FlowPhase::RevealPending);
        let cmds = flow.step(
            at(500 + REVEAL_MAP_DEADLINE.as_millis() as u64),
            FlowEvent::Tick,
        );
        assert!(has(&cmds, &FlowCmd::ReleaseSessionLock), "{cmds:?}");
        assert_eq!(flow.phase(), FlowPhase::Revealing);
    }

    #[test]
    fn grace_dismissal_reveals_without_auth() {
        let lock = Lock {
            grace_secs: 5,
            ..revealing_lock()
        };
        let mut flow = locked_flow(&lock);
        let cmds = flow.step(at(600), FlowEvent::Input(key()));
        assert!(has(&cmds, &FlowCmd::DetachAuth));
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays));
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::DispatchInput(_)))
        );
    }

    #[test]
    fn sleep_kills_grace_and_locked_input_reaches_the_ui() {
        let lock = Lock {
            grace_secs: 300,
            ..manual_lock()
        };
        let mut flow = locked_flow(&lock);
        flow.step(at(600), FlowEvent::PrepareForSleep(true));
        let cmds = flow.step(at(700), FlowEvent::Input(key()));
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, FlowCmd::DispatchInput(_))),
            "{cmds:?}"
        );
        assert_eq!(flow.phase(), FlowPhase::Locked);
    }

    #[test]
    fn a_latched_unlock_never_opens_a_pam_conversation() {
        let (mut flow, _) = LockFlow::new(at(0), &revealing_lock(), Some(0));
        flow.step(at(100), FlowEvent::LogindUnlock);
        flow.step(at(400), FlowEvent::Tick);
        let cmds = flow.step(at(410), FlowEvent::LockConfirmed);
        assert!(has(&cmds, &FlowCmd::SignalReady), "readiness is still owed");
        assert!(!has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");
        assert!(!has(&cmds, &FlowCmd::ShowPanel(true)), "{cmds:?}");
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays));
    }

    #[test]
    fn sleep_forbids_a_grace_window_armed_after_resume() {
        let lock = Lock {
            grace_secs: 5,
            ..manual_lock()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, Some(0));
        flow.step(at(100), FlowEvent::PrepareForSleep(true));
        flow.step(at(400), FlowEvent::Tick);
        flow.step(at(410), FlowEvent::LockConfirmed);
        assert_eq!(flow.phase(), FlowPhase::Locked);
        let cmds = flow.step(at(500), FlowEvent::Input(key()));
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, FlowCmd::DispatchInput(_))),
            "keypress must reach PAM, not dismiss a grace window: {cmds:?}"
        );
        assert!(!has(&cmds, &FlowCmd::DetachAuth));
    }

    #[test]
    fn logind_unlock_racing_the_ramp_is_honored_at_confirmation() {
        let (mut flow, _) = LockFlow::new(at(0), &revealing_lock(), Some(0));
        flow.step(at(100), FlowEvent::LogindUnlock);
        assert_eq!(flow.phase(), FlowPhase::PreLock);
        flow.step(at(400), FlowEvent::Tick);
        let cmds = flow.step(at(410), FlowEvent::LockConfirmed);
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays), "{cmds:?}");
        assert_eq!(flow.phase(), FlowPhase::RevealPending);
    }

    #[test]
    fn late_invalidation_after_authorization_still_exits_unlocked() {
        let mut flow = locked_flow(&revealing_lock());
        flow.step(at(500), FlowEvent::AuthOk);
        flow.step(at(520), FlowEvent::RevealOverlaysMapped);
        assert_eq!(flow.phase(), FlowPhase::Revealing);
        let cmds = flow.step(at(530), FlowEvent::LockInvalidated);
        assert!(has(&cmds, &FlowCmd::DestroyRevealOverlays));
        assert!(
            has(&cmds, &FlowCmd::Exit(LockOutcome::Unlocked)),
            "{cmds:?}"
        );
    }

    #[test]
    fn a_terminal_flow_ignores_further_events() {
        let (mut flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        flow.step(at(400), FlowEvent::Tick);
        let first = flow.step(at(410), FlowEvent::LockDenied);
        assert!(has(&first, &FlowCmd::Exit(LockOutcome::Denied)));
        let second = flow.step(at(420), FlowEvent::LockInvalidated);
        assert!(second.is_empty(), "{second:?}");
        assert_eq!(flow.phase(), FlowPhase::Done(LockOutcome::Denied));
    }

    #[test]
    fn exit_codes_are_the_contract_lock_cmd_reads() {
        assert_eq!(LockOutcome::Unlocked.exit_code(), 0);
        assert_eq!(LockOutcome::Invalidated.exit_code(), 1);
        assert_eq!(LockOutcome::Denied.exit_code(), 2);
        assert_eq!(LockOutcome::Cancelled.exit_code(), 3);
        assert_eq!(LockOutcome::Terminated(Signal::Hangup).exit_code(), 129);
        assert_eq!(LockOutcome::Terminated(Signal::Interrupt).exit_code(), 130);
        assert_eq!(LockOutcome::Terminated(Signal::Terminate).exit_code(), 143);
    }

    #[test]
    fn signal_numbers_and_names_follow_signal_7() {
        for (signal, number, name) in [
            (Signal::Hangup, 1, "SIGHUP"),
            (Signal::Interrupt, 2, "SIGINT"),
            (Signal::Terminate, 15, "SIGTERM"),
        ] {
            assert_eq!(signal.number(), number);
            assert_eq!(signal.name(), name);
            assert_eq!(Signal::from_number(number), Some(signal));
        }
        assert_eq!(Signal::from_number(9), None, "SIGKILL cannot be handled");
    }

    #[test]
    fn a_signal_while_locked_exits_without_releasing_the_lock() {
        let mut flow = locked_flow(&manual_lock());
        let cmds = flow.step(at(600), FlowEvent::Terminate(Signal::Terminate));
        let outcome = LockOutcome::Terminated(Signal::Terminate);
        assert!(has(&cmds, &FlowCmd::Exit(outcome)), "{cmds:?}");
        assert!(
            !cmds.iter().any(|cmd| matches!(
                cmd,
                FlowCmd::ReleaseSessionLock
                    | FlowCmd::SetLockedHint(false)
                    | FlowCmd::CreateRevealOverlays
                    | FlowCmd::DetachAuth
            )),
            "a signal must never unlock: {cmds:?}"
        );
        assert_eq!(flow.phase(), FlowPhase::Done(outcome));
        assert!(flow.next_wake().is_none(), "nothing is left to wake for");
        let later = flow.step(at(700), FlowEvent::AuthOk);
        assert!(later.is_empty(), "terminal: {later:?}");
    }

    #[test]
    fn a_signal_during_the_warning_never_requests_the_lock() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(3_000), None);
        let cmds = flow.step(at(500), FlowEvent::Terminate(Signal::Interrupt));
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        assert!(has(
            &cmds,
            &FlowCmd::Exit(LockOutcome::Terminated(Signal::Interrupt))
        ));
        let later = flow.step(at(4_000), FlowEvent::Tick);
        assert!(later.is_empty(), "{later:?}");
    }

    #[test]
    fn terminal_names_stay_inert() {
        let all = [
            LockOutcome::Unlocked,
            LockOutcome::Denied,
            LockOutcome::Invalidated,
            LockOutcome::Cancelled,
            LockOutcome::Terminated(Signal::Hangup),
            LockOutcome::Terminated(Signal::Interrupt),
            LockOutcome::Terminated(Signal::Terminate),
        ];
        for outcome in all {
            for text in [outcome.to_string(), FlowPhase::Done(outcome).to_string()] {
                assert!(
                    !text.contains(char::is_whitespace) && !text.contains('='),
                    "{text:?}"
                );
            }
        }
        assert_eq!(LockOutcome::Unlocked.to_string(), "Unlocked");
        assert_eq!(
            FlowPhase::Done(LockOutcome::Cancelled).to_string(),
            "Done:Cancelled"
        );
        assert_eq!(
            LockOutcome::Terminated(Signal::Terminate).to_string(),
            "Terminated:SIGTERM"
        );
        assert_eq!(
            FlowPhase::Done(LockOutcome::Terminated(Signal::Hangup)).to_string(),
            "Done:Terminated:SIGHUP"
        );
    }

    #[test]
    fn construction_arms_a_wake_for_the_ramp() {
        let (flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        assert!(flow.next_wake().is_some());
    }

    fn grace_key() -> InputEvent {
        InputEvent::Key {
            keysym: 'a' as u32,
            utf8: Some("a".into()),
            pressed: true,
        }
    }

    fn grace(secs: u64) -> Grace {
        Grace::new(Instant::now(), SystemTime::now(), secs)
    }

    #[test]
    fn press_inside_window_dismisses() {
        assert!(grace(5).dismisses(&grace_key(), Instant::now(), SystemTime::now()));
        assert!(grace(5).dismisses(
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
        assert!(!grace(5).dismisses(
            &grace_key(),
            Instant::now() + Duration::from_secs(6),
            SystemTime::now() + Duration::from_secs(6),
        ));
    }

    #[test]
    fn wall_clock_jump_kills_grace() {
        assert!(!grace(5).dismisses(
            &grace_key(),
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
            assert!(!grace(5).dismisses(&event, Instant::now(), SystemTime::now()));
        }
    }

    #[test]
    fn zero_grace_never_dismisses() {
        assert!(!grace(0).dismisses(&grace_key(), Instant::now(), SystemTime::now()));
    }

    #[test]
    fn unlock_signal_releases_without_auth() {
        let mut flow = locked_flow(&manual_lock());
        let cmds = flow.step(at(600), FlowEvent::LogindUnlock);
        assert!(has(&cmds, &FlowCmd::DetachAuth));
        assert!(has(&cmds, &FlowCmd::SetLockedHint(false)));
    }

    #[test]
    fn lock_signal_while_locked_leaves_grace_alone() {
        let lock = Lock {
            grace_secs: 5,
            ..manual_lock()
        };
        let mut flow = locked_flow(&lock);
        flow.step(at(600), FlowEvent::CommitRequested);
        let cmds = flow.step(at(650), FlowEvent::Input(grace_key()));
        assert!(has(&cmds, &FlowCmd::DetachAuth));
    }

    #[test]
    fn resume_leaves_a_running_flow_alone() {
        let lock = Lock {
            grace_secs: 5,
            ..revealing_lock()
        };
        let mut flow = locked_flow(&lock);
        flow.step(at(600), FlowEvent::PrepareForSleep(false));
        let cmds = flow.step(at(650), FlowEvent::Input(grace_key()));
        assert!(has(&cmds, &FlowCmd::DetachAuth));
        assert_eq!(flow.phase(), FlowPhase::RevealPending);
    }

    #[test]
    fn overlay_progress_is_emitted_at_most_once_per_frame() {
        let (mut flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        let first = flow.step(at(66), FlowEvent::Tick);
        assert!(
            first
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::OverlayProgress { .. }))
        );
        let second = flow.step(at(76), FlowEvent::Tick);
        assert!(
            !second
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::OverlayProgress { .. })),
            "{second:?}"
        );
    }

    #[test]
    fn a_transition_is_reported_where_it_happens() {
        let lock = manual_lock();
        let (mut flow, _) = LockFlow::new(at(0), &lock, Some(0));

        let cmds = flow.step(at(lock.transition.in_ms()), FlowEvent::Tick);
        assert_eq!(
            transitions(&cmds),
            [(FlowPhase::PreLock, FlowPhase::Committing)],
            "{cmds:?}"
        );
        assert!(
            position(
                &cmds,
                &FlowCmd::PhaseChanged {
                    from: FlowPhase::PreLock,
                    to: FlowPhase::Committing,
                }
            ) < position(&cmds, &FlowCmd::RequestSessionLock),
            "the transition must precede the command it caused: {cmds:?}"
        );

        let cmds = flow.step(at(lock.transition.in_ms() + 10), FlowEvent::LockConfirmed);
        assert_eq!(
            transitions(&cmds),
            [(FlowPhase::Committing, FlowPhase::Locked)],
            "{cmds:?}"
        );
        assert!(
            position(
                &cmds,
                &FlowCmd::PhaseChanged {
                    from: FlowPhase::Committing,
                    to: FlowPhase::Locked,
                }
            ) < position(&cmds, &FlowCmd::SignalReady),
            "readiness is a consequence of being locked: {cmds:?}"
        );
    }

    #[test]
    fn a_reported_transition_always_matches_the_phase_that_follows() {
        let lock = manual_lock();
        let (mut flow, _) = LockFlow::new(at(0), &lock, Some(0));
        let mut seen = FlowPhase::PreLock;
        let mut steps = 0;

        for (ms, event) in [
            (lock.transition.in_ms(), FlowEvent::Tick),
            (lock.transition.in_ms() + 10, FlowEvent::LockConfirmed),
            (lock.transition.in_ms() + 20, FlowEvent::AuthOk),
            (
                lock.transition.in_ms() + 30,
                FlowEvent::RevealOverlaysMapped,
            ),
        ] {
            let cmds = flow.step(at(ms), event);
            for (from, to) in transitions(&cmds) {
                assert_eq!(from, seen, "a transition skipped a phase: {cmds:?}");
                seen = to;
                steps += 1;
            }
            assert_eq!(flow.phase(), seen, "phase() disagrees with the stream");
        }
        assert!(steps >= 3, "expected several transitions, saw {steps}");
    }

    #[test]
    fn entering_done_is_reported_like_any_other_transition() {
        for (label, drive) in [
            (
                "cancelled",
                Box::new(|flow: &mut LockFlow| flow.step(at(10), FlowEvent::Input(key())))
                    as Box<dyn Fn(&mut LockFlow) -> Vec<FlowCmd>>,
            ),
            (
                "denied",
                Box::new(|flow: &mut LockFlow| flow.step(at(10), FlowEvent::LockDenied)),
            ),
            (
                "invalidated",
                Box::new(|flow: &mut LockFlow| flow.step(at(10), FlowEvent::LockInvalidated)),
            ),
            (
                "terminated",
                Box::new(|flow: &mut LockFlow| {
                    flow.step(at(10), FlowEvent::Terminate(Signal::Terminate))
                }),
            ),
        ] {
            let (mut flow, _) = LockFlow::new(at(0), &warning_lock(3_000), None);
            let before = flow.phase();
            let cmds = drive(&mut flow);
            let reported = transitions(&cmds);
            assert_eq!(
                reported.len(),
                1,
                "{label}: expected one transition: {cmds:?}"
            );
            let (from, to) = reported[0];
            assert_eq!(from, before, "{label}: wrong `from`");
            assert!(
                matches!(to, FlowPhase::Done(_)),
                "{label}: expected a terminal phase, got {to:?}"
            );
            assert_eq!(
                flow.phase(),
                to,
                "{label}: phase() disagrees with the stream"
            );
            let exit = cmds
                .iter()
                .position(|cmd| matches!(cmd, FlowCmd::Exit(_)))
                .unwrap_or_else(|| panic!("{label}: no Exit in {cmds:?}"));
            let changed = cmds
                .iter()
                .position(|cmd| matches!(cmd, FlowCmd::PhaseChanged { .. }))
                .expect("checked above");
            assert!(
                changed < exit,
                "{label}: Exit must follow the transition: {cmds:?}"
            );
        }
    }

    #[test]
    fn the_unlock_path_reports_its_transitions_before_what_they_cause() {
        let lock = revealing_lock();
        let mut flow = locked_flow(&lock);
        let cmds = flow.step(at(10_000), FlowEvent::AuthOk);
        let changed = cmds
            .iter()
            .position(|cmd| matches!(cmd, FlowCmd::PhaseChanged { .. }))
            .unwrap_or_else(|| panic!("no transition in {cmds:?}"));
        for caused in [FlowCmd::DetachAuth, FlowCmd::SetLockedHint(false)] {
            assert!(
                changed < position(&cmds, &caused),
                "{caused:?} is a consequence of the transition: {cmds:?}"
            );
        }

        let cmds = flow.step(at(10_010), FlowEvent::RevealOverlaysMapped);
        assert_eq!(
            transitions(&cmds),
            [(FlowPhase::RevealPending, FlowPhase::Revealing)],
            "{cmds:?}"
        );
        let changed = cmds
            .iter()
            .position(|cmd| matches!(cmd, FlowCmd::PhaseChanged { .. }))
            .expect("checked above");
        assert!(
            changed < position(&cmds, &FlowCmd::ReleaseSessionLock),
            "releasing the lock is what entering Revealing means: {cmds:?}"
        );
    }

    #[test]
    fn blur_free_reveal_starts_at_zero_frost() {
        let lock = revealing_lock();
        let mut flow = locked_flow(&lock);
        let cmds = flow.step(at(10_000), FlowEvent::AuthOk);
        assert!(
            has(
                &cmds,
                &FlowCmd::OverlayProgress {
                    frost: 0.0,
                    wallpaper: 1.0
                }
            ),
            "{cmds:?}"
        );
    }

    #[test]
    fn blurring_reveal_starts_at_full_frost() {
        let lock = blurring_reveal_lock();
        let mut flow = locked_flow(&lock);
        let cmds = flow.step(at(10_000), FlowEvent::AuthOk);
        assert!(
            has(
                &cmds,
                &FlowCmd::OverlayProgress {
                    frost: 1.0,
                    wallpaper: 1.0
                }
            ),
            "{cmds:?}"
        );
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays), "{cmds:?}");
    }
}

#[cfg(test)]
mod late_wallpaper_replay {
    use super::tests::*;
    use super::*;

    #[test]
    fn a_wallpaper_ready_after_the_hold_cap_still_commits() {
        replay(LockFlow::new(at(0), &warning_lock(4_000), None).0);
    }

    #[test]
    fn the_cli_warn_override_commits_like_a_configured_warning() {
        replay(LockFlow::new(at(0), &Lock::default(), Some(4_000)).0);
    }

    fn replay(mut flow: LockFlow) {
        flow.step(at(100), FlowEvent::WallpaperReady(false));
        for ms in [1_000, 2_000, 2_850, 2_851] {
            flow.step(at(ms), FlowEvent::Tick);
        }
        flow.step(at(8_308), FlowEvent::Tick);
        let cmds = flow.step(at(8_309), FlowEvent::WallpaperReady(true));
        assert_eq!(flow.phase(), FlowPhase::PreLock, "{cmds:?}");
        assert!(
            flow.next_wake().is_some(),
            "ready must leave a wake armed: {cmds:?}"
        );
        let mut committed = false;
        for ms in [8_310, 9_000, 9_500, 9_900, 60_891] {
            let cmds = flow.step(at(ms), FlowEvent::Tick);
            if has(&cmds, &FlowCmd::RequestSessionLock) {
                committed = true;
                break;
            }
        }
        assert!(
            committed,
            "the lock never committed; phase={:?}",
            flow.phase()
        );
    }

    #[test]
    fn a_wallpaper_ready_after_the_cap_expired_still_commits() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(4_000), None);
        flow.step(at(100), FlowEvent::WallpaperReady(false));
        flow.step(at(2_851), FlowEvent::Tick);
        let cmds = flow.step(at(9_500), FlowEvent::WallpaperReady(true));
        let mut committed = has(&cmds, &FlowCmd::RequestSessionLock);
        for ms in [9_501, 10_000, 60_891] {
            let cmds = flow.step(at(ms), FlowEvent::Tick);
            if has(&cmds, &FlowCmd::RequestSessionLock) {
                committed = true;
            }
        }
        assert!(
            committed,
            "phase={:?} wake={:?}",
            flow.phase(),
            flow.next_wake()
        );
    }
}
