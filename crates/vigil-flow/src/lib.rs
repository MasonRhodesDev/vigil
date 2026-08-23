//! Pure lock-lifecycle controller (issue #43).
//!
//! `LockFlow` owns every transition the locker makes — warning/transition
//! ramp, session-lock commit, auth/grace, reveal, exit — with **events in,
//! commands out**. It performs no I/O and reads no clocks: callers pass
//! [`Now`] and receive [`FlowCmd`]s to execute. The production adapter
//! (vigil-lock / vigil-wayland) maps commands onto PAM, logind, and Wayland;
//! the simulator and unit tests execute the same controller against fakes,
//! which is the whole point: transition logic exists exactly once.
//!
//! The animation contract inherited from `vigil-warning` holds here too:
//! ramp values are frame-grid quantized, so [`LockFlow::step`] may be driven
//! at any rate (event-loop wakes included) and emits at most one
//! [`FlowCmd::OverlayProgress`] per frame.

use std::time::{Duration, Instant, SystemTime};

use vigil_config::{Lock, LockTransition};
use vigil_core::InputEvent;
use vigil_warning::{ElementSample, Phase as RampPhase, Reveal, Timeline};

/// Before releasing the session lock, wait this long for the reveal
/// overlays' first buffer commit so the desktop is never exposed un-frosted.
/// A compositor that refuses to configure a layer surface while locked falls
/// through to the plain unlock.
pub const REVEAL_MAP_DEADLINE: Duration = Duration::from_millis(250);
/// The reveal fade is cosmetic: whatever state it is in, the flow exits this
/// long after the fade started.
pub const REVEAL_HARD_DEADLINE: Duration = Duration::from_millis(2_000);

/// How a lock run ends. (Authoritative home; vigil-wayland re-exports it.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    /// Auth succeeded; the unlock was round-tripped to the compositor.
    Unlocked,
    /// The compositor refused the lock (another locker running?).
    Denied,
    /// The compositor invalidated the lock after it was held.
    Invalidated,
    /// Input or output topology cancelled the warning before session-lock.
    Cancelled,
}

/// The three clocks a step may consult. `elapsed` is the app-monotonic ramp
/// clock (time since flow construction); `mono`/`wall` exist solely for the
/// grace window's dual-clock deadline (Instant freezes across suspend,
/// SystemTime does not — requiring both keeps a pre-suspend grace from
/// surviving resume).
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub elapsed: Duration,
    pub mono: Instant,
    pub wall: SystemTime,
}

impl Now {
    /// Test/adapter helper for contexts that only advance the ramp clock.
    pub fn at(elapsed: Duration) -> Self {
        Self {
            elapsed,
            mono: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// Everything the outside world can tell the flow.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowEvent {
    /// Seat input. The flow decides whether it cancels a warning, dismisses
    /// grace, or should reach the UI ([`FlowCmd::DispatchInput`]).
    Input(InputEvent),
    /// Pointer entered an output at scene coordinates (warning cancel-motion
    /// bookkeeping).
    PointerEnter {
        x: f64,
        y: f64,
    },
    /// Output topology changed before or after commitment.
    OutputAdded,
    OutputGone,
    WallpaperReady(bool),
    /// Out-of-band request (second locker's join socket, logind `Lock`,
    /// `PrepareForSleep(true)`) to skip the remaining ramp and lock now.
    CommitRequested,
    /// Compositor granted the session lock (`ext_session_lock_v1.locked`).
    LockConfirmed,
    /// Compositor refused the lock before confirming it.
    LockDenied,
    /// Compositor invalidated a held lock.
    LockInvalidated,
    /// A pre-lock overlay was closed by the compositor.
    OverlayClosed,
    /// Every reveal overlay is configured and has committed a buffer.
    RevealOverlaysMapped,
    AuthOk,
    AuthErr(String),
    /// logind `Unlock`: release without authentication.
    LogindUnlock,
    /// logind `PrepareForSleep`. `true` commits a pending ramp (lock before
    /// the machine sleeps) and kills any live grace window; `false` is a
    /// resume and leaves grace alone (the dual-clock deadline already
    /// handled it).
    PrepareForSleep(bool),
    /// Timer wakeup or any event-loop pass with nothing more specific.
    Tick,
}

/// Everything the flow can ask the outside world to do, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowCmd {
    /// Ask the compositor for `ext-session-lock-v1` (and create lock
    /// surfaces per output).
    RequestSessionLock,
    /// Frame-grid-quantized overlay compositing values; emitted only on
    /// change. Adapters forward without further pacing.
    OverlayProgress {
        frost: f32,
        wallpaper: f32,
    },
    /// GUI element animation samples; emitted only on change.
    OverlayElements(Vec<ElementSample>),
    /// Show or hide the password card (hidden during ramps and reveal).
    ShowPanel(bool),
    /// Deliver this input to the UI (it was not consumed by warning
    /// cancellation or grace dismissal).
    DispatchInput(InputEvent),
    StartAuth,
    ShowAuthError(String),
    DetachAuth,
    SetLockedHint(bool),
    /// Compositor-confirmed readiness: write the ready byte and answer
    /// joiners.
    SignalReady,
    /// Map one pointer-transparent reveal overlay per locked output.
    CreateRevealOverlays,
    /// `unlock_and_destroy` + roundtrip.
    ReleaseSessionLock,
    DestroyRevealOverlays,
    /// Terminal: the process should exit with this outcome after executing
    /// prior commands.
    Exit(LockOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowPhase {
    /// Warning or transition ramp on overlays; the session lock is not yet
    /// requested. Absent entirely on the immediate path.
    PreLock,
    /// Session lock requested, awaiting the compositor's answer.
    Committing,
    /// Lock confirmed; authenticating (grace window may be live).
    Locked,
    /// Unlock authorized; waiting for reveal overlays to map.
    RevealPending,
    /// Lock released; fade running.
    Revealing,
    Done(LockOutcome),
}

pub struct LockFlow {
    phase: FlowPhase,
    timeline: Option<Timeline>,
    reveal: Option<Reveal>,
    transition: LockTransition,
    grace_secs: u64,
    grace: Option<Grace>,
    /// Last values pushed to overlays; commands are emitted only on change.
    progress: (f32, f32),
    elements: Vec<ElementSample>,
    /// App-clock time the reveal overlays were requested.
    reveal_entered: Option<Duration>,
    reveal_started: Option<Duration>,
    /// Earliest next wakeup computed by the last step.
    wait: Option<Duration>,
}

impl LockFlow {
    /// `warning_ms_override` is the CLI `--warn`/`--no-warn` value; the
    /// config's warning duration applies otherwise. Returns the flow plus
    /// the commands of the very first instant (an immediate path requests
    /// the session lock at once).
    pub fn new(now: Now, lock: &Lock, warning_ms_override: Option<u64>) -> (Self, Vec<FlowCmd>) {
        let mut warning = lock.warning.clone();
        if let Some(ms) = warning_ms_override {
            warning.duration_ms = ms;
        }
        let transition = lock.transition.clone();
        let cancelable = warning.duration_ms > 0;
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
            reveal_entered: None,
            reveal_started: None,
            wait: None,
        };
        let mut cmds = Vec::new();
        if let Some(timeline) = flow.timeline.as_mut() {
            timeline.start(now.elapsed);
            cmds.push(FlowCmd::ShowPanel(false));
        } else {
            cmds.push(FlowCmd::RequestSessionLock);
        }
        (flow, cmds)
    }

    pub fn phase(&self) -> FlowPhase {
        self.phase
    }

    /// Earliest wakeup the flow needs, as of the last `step`. `None` means
    /// event-driven only (the static locked idle state stays frame-quiet).
    pub fn next_wake(&self) -> Option<Duration> {
        self.wait
    }

    pub fn step(&mut self, now: Now, event: FlowEvent) -> Vec<FlowCmd> {
        let mut cmds = Vec::new();
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
                    self.phase = FlowPhase::Locked;
                    if let Some(timeline) = self.timeline.as_mut() {
                        timeline.locked(now.elapsed);
                    }
                    if self.grace_secs > 0 {
                        self.grace = Some(Grace::new(now.mono, now.wall, self.grace_secs));
                    }
                    cmds.push(FlowCmd::ShowPanel(true));
                    cmds.push(FlowCmd::SignalReady);
                    cmds.push(FlowCmd::SetLockedHint(true));
                    cmds.push(FlowCmd::StartAuth);
                }
            }
            FlowEvent::LockDenied => self.finish(LockOutcome::Denied, &mut cmds),
            FlowEvent::LockInvalidated => self.finish(LockOutcome::Invalidated, &mut cmds),
            FlowEvent::RevealOverlaysMapped => {
                if self.phase == FlowPhase::RevealPending {
                    self.release_and_start_reveal(now, &mut cmds);
                }
            }
            FlowEvent::AuthOk => {
                if self.phase == FlowPhase::Locked {
                    self.unlock_authorized(now, &mut cmds);
                }
            }
            FlowEvent::AuthErr(message) => {
                if self.phase == FlowPhase::Locked {
                    cmds.push(FlowCmd::ShowAuthError(message));
                    // A fresh PAM transaction per attempt (hyprlock's
                    // model): the new conversation re-prompts.
                    cmds.push(FlowCmd::StartAuth);
                }
            }
            FlowEvent::LogindUnlock => {
                if self.phase == FlowPhase::Locked {
                    self.unlock_authorized(now, &mut cmds);
                }
            }
            FlowEvent::PrepareForSleep(true) => {
                self.grace = None;
                if self.phase == FlowPhase::PreLock
                    && let Some(timeline) = self.timeline.as_mut()
                {
                    timeline.request_commit();
                }
            }
            FlowEvent::PrepareForSleep(false) => {}
            FlowEvent::Tick => {}
        }
        if matches!(self.phase, FlowPhase::Done(_)) {
            return cmds;
        }
        self.advance(now, &mut cmds);
        cmds
    }

    /// Time-driven progress: ramp sampling, commit scheduling, reveal
    /// deadlines. Runs after every event so a wake of any kind advances the
    /// machine.
    fn advance(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) {
        match self.phase {
            FlowPhase::PreLock | FlowPhase::Committing | FlowPhase::Locked => {
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
                    self.wait = None;
                    return;
                };
                self.wait = match (sample.next_frame, gui_wake) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (left, right) => left.or(right),
                };
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
                match sample.phase {
                    RampPhase::Cancelled => {
                        self.finish(LockOutcome::Cancelled, cmds);
                    }
                    _ if sample.should_commit && self.phase == FlowPhase::PreLock => {
                        self.phase = FlowPhase::Committing;
                        cmds.push(FlowCmd::RequestSessionLock);
                    }
                    _ => {}
                }
                if self.phase == FlowPhase::Locked && gui_done {
                    self.timeline = None;
                    self.wait = None;
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
                // Cancellation surfaces via the sample in advance().
            }
            FlowPhase::Locked => {
                if let Some(grace) = &self.grace
                    && grace.dismisses(&input, now.mono, now.wall)
                {
                    // Dismissed inside the grace window: unlock and swallow
                    // the event so it never reaches a PAM response.
                    self.unlock_authorized(now, cmds);
                    return;
                }
                cmds.push(FlowCmd::DispatchInput(input));
            }
            // Committing (pre-confirmation) and reveal phases consume input:
            // the session is either about to be secured or already released.
            _ => {}
        }
    }

    /// Auth success, grace dismissal, or logind Unlock.
    fn unlock_authorized(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) {
        cmds.push(FlowCmd::DetachAuth);
        cmds.push(FlowCmd::SetLockedHint(false));
        if self.transition.reveals() {
            self.phase = FlowPhase::RevealPending;
            self.reveal = Some(Reveal::new(
                self.transition.wallpaper_out_ms,
                self.transition.frost_out_ms,
                self.transition.easing,
            ));
            self.reveal_entered = Some(now.elapsed);
            self.progress = (1.0, 1.0);
            cmds.push(FlowCmd::ShowPanel(false));
            cmds.push(FlowCmd::OverlayProgress {
                frost: 1.0,
                wallpaper: 1.0,
            });
            cmds.push(FlowCmd::CreateRevealOverlays);
        } else {
            cmds.push(FlowCmd::ReleaseSessionLock);
            self.finish(LockOutcome::Unlocked, cmds);
        }
    }

    fn release_and_start_reveal(&mut self, now: Now, cmds: &mut Vec<FlowCmd>) {
        cmds.push(FlowCmd::ReleaseSessionLock);
        if let Some(reveal) = self.reveal.as_mut() {
            reveal.start(now.elapsed);
        }
        self.reveal_started = Some(now.elapsed);
        self.phase = FlowPhase::Revealing;
        self.wait = Some(Duration::from_millis(vigil_warning::FRAME_INTERVAL_MS));
    }

    fn finish(&mut self, outcome: LockOutcome, cmds: &mut Vec<FlowCmd>) {
        self.phase = FlowPhase::Done(outcome);
        self.timeline = None;
        self.reveal = None;
        self.wait = None;
        cmds.push(FlowCmd::Exit(outcome));
    }
}

/// Grace window: unlock without auth shortly after locking. Two deadlines
/// on two clocks: Instant freezes during suspend while SystemTime does
/// not, so requiring BOTH keeps a pre-suspend grace from surviving into
/// the resume — the lock-before-sleep guarantee stays intact without
/// logind integration.
pub struct Grace {
    deadline_mono: Instant,
    deadline_wall: SystemTime,
}

impl Grace {
    /// Deadlines anchored at the injected clocks (deterministic in tests).
    pub fn new(now_mono: Instant, now_wall: SystemTime, secs: u64) -> Self {
        let secs = Duration::from_secs(secs);
        Self {
            deadline_mono: now_mono + secs,
            deadline_wall: now_wall + secs,
        }
    }

    /// Presses dismiss; motion and releases never do.
    pub fn dismisses(&self, event: &InputEvent, now_mono: Instant, now_wall: SystemTime) -> bool {
        let live = now_mono < self.deadline_mono && now_wall < self.deadline_wall;
        live && matches!(
            event,
            InputEvent::Key { pressed: true, .. } | InputEvent::PointerButton { pressed: true, .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vigil_config::{Lock, LockTransition, LockWarning};

    fn key() -> InputEvent {
        InputEvent::Key {
            keysym: 1,
            utf8: None,
            pressed: true,
        }
    }

    fn manual_lock() -> Lock {
        // The hypr-DE production shape: --no-warn (no cancelable warning),
        // default transition.
        Lock::default()
    }

    fn warning_lock(duration_ms: u64) -> Lock {
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

    fn at(ms: u64) -> Now {
        Now::at(Duration::from_millis(ms))
    }

    fn has(cmds: &[FlowCmd], wanted: &FlowCmd) -> bool {
        cmds.contains(wanted)
    }

    /// Drive to Locked via the standard handshake; returns the flow.
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
        // Including inside a frame window (issue #54 review): cancellation
        // is phase logic, never paced by the animation grid.
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
        // Cancelable warning, frost done, wallpaper late: the ready event
        // must leave a wake armed so the commit is never orphaned (issue
        // #54 review scenario).
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(3_000), None);
        flow.step(at(0), FlowEvent::WallpaperReady(false));
        flow.step(at(2_500), FlowEvent::Tick);
        let cmds = flow.step(at(2_505), FlowEvent::WallpaperReady(true));
        assert!(flow.next_wake().is_some(), "{cmds:?}");
        let cmds = flow.step(at(2_505 + 1_500), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
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
        let mut flow = locked_flow(&manual_lock());
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
    fn unlock_during_gui_window_still_reveals_and_exits_by_deadline() {
        // Unlock while the post-lock GUI animation is mid-flight (issue #54
        // review: the reveal must never be starved by the warning timeline).
        let mut flow = locked_flow(&manual_lock());
        let cmds = flow.step(at(500), FlowEvent::AuthOk);
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays));
        let cmds = flow.step(at(520), FlowEvent::RevealOverlaysMapped);
        assert!(has(&cmds, &FlowCmd::ReleaseSessionLock));
        assert_eq!(flow.phase(), FlowPhase::Revealing);
        // Fade runs to completion within the hard deadline.
        let cmds = flow.step(at(520 + 3_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::DestroyRevealOverlays));
        assert!(has(&cmds, &FlowCmd::Exit(LockOutcome::Unlocked)));
    }

    #[test]
    fn reveal_map_timeout_releases_lock_anyway() {
        let mut flow = locked_flow(&manual_lock());
        flow.step(at(500), FlowEvent::AuthOk);
        assert_eq!(flow.phase(), FlowPhase::RevealPending);
        // No RevealOverlaysMapped ever arrives; the deadline unlocks.
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
            ..manual_lock()
        };
        let mut flow = locked_flow(&lock);
        let cmds = flow.step(at(600), FlowEvent::Input(key()));
        assert!(has(&cmds, &FlowCmd::DetachAuth));
        assert!(has(&cmds, &FlowCmd::CreateRevealOverlays));
        // The keystroke never reaches the UI (no PAM response).
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
    fn overlay_progress_is_emitted_at_most_once_per_frame() {
        let (mut flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        let first = flow.step(at(66), FlowEvent::Tick);
        assert!(
            first
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::OverlayProgress { .. }))
        );
        // A buffer-release wake 10 ms later inside the same frame window.
        let second = flow.step(at(76), FlowEvent::Tick);
        assert!(
            !second
                .iter()
                .any(|cmd| matches!(cmd, FlowCmd::OverlayProgress { .. })),
            "{second:?}"
        );
    }
}
