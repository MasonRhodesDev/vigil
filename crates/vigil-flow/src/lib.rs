//! Pure product controllers for vigil's two surfaces.
//!
//! **Admission rule for anything added here:** a controller in this crate
//! is pure — events in, commands out, no I/O, and every clock injected.
//! It may name a protocol's vocabulary (`LockConfirmed`, `GreetdReply`)
//! but must never take a host-effect or protocol *dependency*: no PAM, no
//! logind, no greetd codec, no Wayland. That is what lets `vigil-sim` link
//! this crate and preview production behaviour without being able to reach
//! a real session lock or a real authentication (ADR 0004), and it is
//! enforced by `tests/check-sim-safety.sh` — this comment is the reason
//! that gate exists.
//!
//! [`LockFlow`] is the locker's lifecycle (issue #43); [`greet::GreetFlow`]
//! is the greeter's login stages (issue #61). They deliberately do not
//! share a machine — greetd versus PAM-direct, exec a session versus
//! release a lock — only this contract and the [`Now`] clock.
//!
//! ---
//!
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

pub mod greet;
pub use greet::{
    GreetCmd, GreetConfig, GreetEvent, GreetFlow, GreetOutcome, GreetPhase, GreetdReply,
    SessionChoice, USERNAME_PROMPT,
};

use vigil_config::{Lock, LockTransition};
use vigil_core::InputEvent;
pub use vigil_warning::{ElementSample, WALLPAPER_READY_DEFAULT};
use vigil_warning::{Phase as RampPhase, Reveal, Timeline};

/// Before releasing the session lock, wait this long for the reveal
/// overlays' first buffer commit so the desktop is never exposed before an
/// overlay is mapped over it. A compositor that refuses to configure a layer
/// surface while locked falls through to the plain unlock.
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
    /// The conversation broke before PAM could judge the credential — the
    /// locker's own transport, not a wrong password (issue #91). Reopen the
    /// conversation, but do NOT run it through the failure path: what the
    /// user reads as "wrong password" is [`FlowCmd::ShowAuthError`], and
    /// teaching them to retype a password that was never wrong is how a
    /// self-inflicted glitch turns into a faillock lockout.
    AuthConversationLost,
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
    /// A degradation worth journaling. Typed rather than prose: the
    /// controller stays free of presentation policy, tests can assert
    /// *which* degradation, and the note can carry its own numbers.
    Journal(FlowNote),
    /// The phase moved.
    ///
    /// Ordering rule: a `PhaseChanged` precedes every command that entering
    /// the new phase causes. The one shape that reads backwards is the
    /// terminal one, and only because `Exit` is documented to run last:
    /// `finish` emits `PhaseChanged` then `Exit`, while a command caused by
    /// the *event* rather than by the new phase - `ReleaseSessionLock` on
    /// unlock - still precedes both.
    ///
    /// The starting phase is not reported, because nothing transitions into
    /// it. An adapter mirroring the phase from this stream must seed itself
    /// from [`LockFlow::phase`] first; on the immediate path the flow is
    /// already in `Committing` when `new` returns, so a mirror initialised
    /// to `PreLock` would disagree with the first transition\'s `from`.
    ///
    /// This crate is pure, so it cannot report a transition by writing one
    /// out; and a `phase()` getter alone forced every adapter to snapshot
    /// the phase before each `step()` and compare afterwards, at three call
    /// sites in the Wayland executor and once more in the simulator. That
    /// is the boilerplate `Journal` exists to avoid for notes, and it stays
    /// correct only until someone adds a fourth `step()` call.
    ///
    /// Putting transitions in the command stream also means `vigil-sim` can
    /// assert transition *ordering* against everything else the controller
    /// emitted, which nothing outside a live adapter could observe before.
    PhaseChanged {
        from: FlowPhase,
        to: FlowPhase,
    },
    /// Terminal: the process should exit with this outcome after executing
    /// prior commands.
    Exit(LockOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowNote {
    /// The wallpaper never arrived, so the warning locked with the scene
    /// as-is this long after its scheduled commit.
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

impl std::fmt::Display for FlowPhase {
    /// Stable, inert names. These reach a journal record as an attribute
    /// value, so they avoid whitespace and `=` deliberately.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreLock => f.write_str("PreLock"),
            Self::Committing => f.write_str("Committing"),
            Self::Locked => f.write_str("Locked"),
            Self::RevealPending => f.write_str("RevealPending"),
            Self::Revealing => f.write_str("Revealing"),
            // Matched rather than `{outcome:?}`: Debug is not a stability
            // guarantee, so a future `Denied { reason: String }` would
            // silently start emitting braces, quotes and spaces into a
            // value this impl promises is inert.
            Self::Done(LockOutcome::Unlocked) => f.write_str("Done:Unlocked"),
            Self::Done(LockOutcome::Denied) => f.write_str("Done:Denied"),
            Self::Done(LockOutcome::Invalidated) => f.write_str("Done:Invalidated"),
            Self::Done(LockOutcome::Cancelled) => f.write_str("Done:Cancelled"),
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
    /// Last values pushed to overlays; commands are emitted only on change.
    progress: (f32, f32),
    elements: Vec<ElementSample>,
    /// How long past its scheduled commit a held warning waits before
    /// locking anyway; owned by `Timeline`, kept here only to label the
    /// journal note it produces.
    wallpaper_hold_max_ms: u64,
    /// App-clock time the reveal overlays were requested.
    reveal_entered: Option<Duration>,
    reveal_started: Option<Duration>,
    /// Sleep was announced: no grace window may be armed for the rest of
    /// this run, including one whose lock confirmation lands after resume.
    grace_forbidden: bool,
    /// logind asked to unlock before the lock was confirmed; honored the
    /// moment it is (an edge-triggered order must not be dropped).
    unlock_latched: bool,
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
        };
        let mut cmds = Vec::new();
        if let Some(timeline) = flow.timeline.as_mut() {
            timeline.start(now.elapsed);
            cmds.push(FlowCmd::ShowPanel(false));
            // Prime the first frame and, critically, `wait`: an adapter that
            // sleeps until next_wake() would otherwise block forever with no
            // timer armed and the ramp never sampled.
            flow.advance(now, &mut cmds);
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

    /// Whether the lock is settled: locked, with every ramp retired.
    ///
    /// Names the invariant `settled() => next_wake().is_none()`, which is
    /// what lets the executor block until an external event arrives. No
    /// production caller consumes it yet -- the executor blocks off
    /// `next_wake()` directly -- so it exists to make the invariant
    /// assertable rather than implied.
    pub fn settled(&self) -> bool {
        self.phase == FlowPhase::Locked && self.timeline.is_none()
    }

    pub fn step(&mut self, now: Now, event: FlowEvent) -> Vec<FlowCmd> {
        let mut cmds = Vec::new();
        if matches!(self.phase, FlowPhase::Done(_)) {
            // Terminal: a second teardown event (denial *and* invalidation
            // from one compositor teardown) must not re-run finish and
            // emit a conflicting Exit.
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
                    // Readiness is owed either way — a --wait caller and a
                    // joining locker are both blocked on it.
                    cmds.push(FlowCmd::SignalReady);
                    if self.unlock_latched {
                        // An unlock was ordered before the lock was granted:
                        // do not show the card or open a PAM conversation we
                        // would abandon microseconds later (issue #36's
                        // pattern), just release.
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
                // Past authorization the lock is already released (or about
                // to be) and the run succeeded: a late `finished` must not
                // turn an authenticated unlock into a failure outcome.
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
            FlowEvent::AuthConversationLost => {
                if self.phase == FlowPhase::Locked {
                    // Reopen only. No ShowAuthError: the card keeps whatever
                    // it was showing and the user is never told they failed
                    // an authentication they never attempted.
                    cmds.push(FlowCmd::StartAuth);
                }
            }
            FlowEvent::LogindUnlock => match self.phase {
                FlowPhase::Locked => self.unlock_authorized(now, &mut cmds),
                // An authoritative unlock racing the ramp or the commit is
                // edge-triggered: latch it so confirmation releases at once
                // instead of leaving the session locked against orders.
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
        // The transition comes first, then everything entering the phase
        // entails - DetachAuth and SetLockedHint are consequences of the
        // unlock, not events that precede it.
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
            // The reveal is blur-free by default (frost 0); it opts into a
            // fading blur when frost_out_ms > 0, which starts at full frost.
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
            // ReleaseSessionLock is a consequence of the unlock, not of
            // entering Done, and `Exit` is documented to run last - so it
            // stays ahead of `finish`, which emits PhaseChanged then Exit.
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

    /// Move to `to`, recording the transition in the command stream.
    ///
    /// Every phase assignment goes through here. Diffing `phase()` around
    /// `step()` would collapse two transitions in one step into one, and
    /// would place the report after every command the step emitted rather
    /// than at the point the phase actually moved.
    fn set_phase(&mut self, to: FlowPhase, cmds: &mut Vec<FlowCmd>) {
        let from = self.phase;
        // No caller can reach this with an unchanged phase today. Asserting
        // that is better than branching on it: a silent early return would
        // be an unreachable arm no test can honestly cover, and a
        // self-transition reaching the stream would make the variant's name
        // untrue rather than merely noisy.
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
        // The hypr-DE production shape: --no-warn (no cancelable warning),
        // default transition.
        Lock::default()
    }

    /// A lock whose transition opts into the reveal slot (unlock fade), for
    /// tests of the reveal machinery. The shipped default is instant unlock
    /// (wallpaper_out_ms = 0); a fade is opt-in and, per fix/no-blur, still
    /// carries no blur.
    pub(crate) fn revealing_lock() -> Lock {
        let mut lock = Lock::default();
        lock.transition.wallpaper_out_ms = 250;
        lock
    }

    /// A reveal that opts into the fading blur (frost_out_ms > 0).
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

    /// Every transition reported by this command batch, in order.
    fn transitions(cmds: &[FlowCmd]) -> Vec<(FlowPhase, FlowPhase)> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                FlowCmd::PhaseChanged { from, to } => Some((*from, *to)),
                _ => None,
            })
            .collect()
    }

    /// Where `wanted` sits in the batch, for ordering assertions.
    fn position(cmds: &[FlowCmd], wanted: &FlowCmd) -> usize {
        cmds.iter()
            .position(|cmd| cmd == wanted)
            .unwrap_or_else(|| panic!("{wanted:?} not in {cmds:?}"))
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

    // These two pin `settled() => next_wake().is_none()`. The invariant
    // holds structurally today (a retired timeline early-returns with no
    // wait), so neither test is red against current main and neither dies
    // to a mutation of the redundant clear in `advance()`. They are here so
    // that a future edit which arms a wake in the settled state -- the
    // shape of #65 -- cannot land silently.
    #[test]
    fn a_settled_lock_arms_no_wake_and_does_no_work() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(3_000), None);
        flow.step(at(3_000), FlowEvent::LockConfirmed);
        // Past every GUI ramp.
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
        // Default transition: wallpaper_out_ms = 0, so reveals() is false.
        // Auth success must release the lock and exit at once - no
        // RevealPending, no reveal overlays, no OverlayProgress. This is the
        // "instantaneous feedback" default; the reveal is an opt-in slot.
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
    fn a_wedged_wallpaper_still_locks_by_the_cap() {
        // A renderer that never reports ready must not leave the machine
        // unlocked (issue #56).
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
        // Past its scheduled commit, still held, still awake for the cap.
        let cmds = flow.step(at(3_500), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock));
        assert!(flow.next_wake().is_some(), "the cap must arm a wake");
        assert_eq!(flow.phase(), FlowPhase::PreLock);
        // At the cap it locks with whatever the scene has, and says so.
        let cmds = flow.step(at(8_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))),
            "the degradation must not be silent: {cmds:?}"
        );
    }

    #[test]
    fn a_fade_longer_than_the_warning_is_not_cut_short() {
        // A held warning commits at max(duration, wallpaper_in), so a
        // deadline derived from `duration` alone fires the cap during a
        // perfectly healthy run and snaps the fade to 1.0 mid-ramp.
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
        // The naive deadline (2000 + 5000) lands here; the wallpaper is
        // ready, so nothing may fire.
        let cmds = flow.step(at(7_000), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        assert!(!cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))));
        let cmds = flow.step(at(10_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn a_late_wallpaper_still_gets_its_full_fade() {
        // ADR 0004: late assets extend the warning. Once one arrives the cap
        // must stand down, and must never claim it "never became ready".
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
        // The deadline (8000) passes while the fade is running.
        let cmds = flow.step(at(8_000), FlowEvent::Tick);
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, FlowCmd::Journal(_))),
            "the wallpaper did arrive: {cmds:?}"
        );
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock));
        // Commits at ready_at + wallpaper_in, fade intact.
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
        // Input lands in the same step as the deadline.
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
        // Must not panic in debug, nor wrap to a near-zero deadline in
        // release and force-commit instantly. A u64::MAX warning is
        // effectively infinite, so not committing is the correct outcome —
        // the operator asked for that.
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
        // --warn N rewrites duration_ms before the timeline is built, so
        // the cap must follow it rather than the config's value.
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
        // The un-overridden deadline (8000) must not fire.
        let cmds = flow.step(at(8_000), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::RequestSessionLock), "{cmds:?}");
        let cmds = flow.step(at(25_000), FlowEvent::Tick);
        assert!(has(&cmds, &FlowCmd::RequestSessionLock));
    }

    #[test]
    fn a_ready_wallpaper_is_unaffected_by_the_cap() {
        // The cap must never cut a healthy warning short.
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

    /// One wrong password re-prompts exactly once — one error shown, one
    /// fresh transaction opened. A second attempt is the user's to make.
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
        // Nothing re-fires on a bare wake.
        let cmds = flow.step(at(700), FlowEvent::Tick);
        assert!(!has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");
    }

    /// Issue #91. A conversation vigil broke reopens the transaction without
    /// ever telling the user they failed an authentication: `ShowAuthError`
    /// is the failure the user reads, and a self-inflicted transport glitch
    /// is not one.
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

    /// Past the lock, a straggling conversation must not reopen anything —
    /// the transaction it would open is one nobody answers.
    #[test]
    fn a_lost_conversation_after_unlock_reopens_nothing() {
        let mut flow = locked_flow(&revealing_lock());
        flow.step(at(700), FlowEvent::AuthOk);
        let cmds = flow.step(at(800), FlowEvent::AuthConversationLost);
        assert!(!has(&cmds, &FlowCmd::StartAuth), "{cmds:?}");
    }

    #[test]
    fn unlock_during_gui_window_still_reveals_and_exits_by_deadline() {
        // Unlock while the post-lock GUI animation is mid-flight (issue #54
        // review: the reveal must never be starved by the warning timeline).
        let mut flow = locked_flow(&revealing_lock());
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
        let mut flow = locked_flow(&revealing_lock());
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
            ..revealing_lock()
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
    fn a_latched_unlock_never_opens_a_pam_conversation() {
        // loginctl unlock racing the ramp: releasing is the whole point, so
        // showing the card and starting an auth we abandon microseconds
        // later is pure churn (and issue #36's failed-conversation pattern).
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
        // The lock-before-sleep guarantee: a confirmation that lands after
        // resume must not arm a fresh grace anchored at wake time, or a
        // keypress unlocks the machine without authentication.
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
    fn construction_arms_a_wake_for_the_ramp() {
        // An adapter that sleeps until next_wake() must never block with no
        // deadline armed, or the ramp never samples and the lock never
        // commits.
        let (flow, _) = LockFlow::new(at(0), &manual_lock(), Some(0));
        assert!(flow.next_wake().is_some());
    }

    // Grace-window tests, moved here with the type (they were in
    // vigil-lock before the extraction).
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
        // Instant freezes across suspend, SystemTime does not: requiring
        // BOTH is what keeps a pre-suspend grace from surviving resume.
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
        // Grace survives: a Lock request against a held lock is a no-op.
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
        // A buffer-release wake 10 ms later inside the same frame window.
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
        // The point of the seam: an adapter learns the phase moved without
        // snapshotting phase() around every step(), and learns it in the
        // right place relative to the commands the move caused.
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
        // The invariant an adapter relies on: after a batch, `to` from the
        // last reported transition is what phase() says. A missed report
        // would desynchronise every observer silently.
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
        // The terminal transition is the one an adapter most needs and was
        // the one no test reached: a mutant turning `finish` back into a
        // raw assignment left all 63 tests green.
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
            // The transition precedes the Exit it causes.
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
        // The three sites that used to push the transition last:
        // RevealPending, Revealing, and the non-reveal unlock.
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

        // ... and the same again one phase later, where the reveal starts.
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
        // The default opt-in reveal (wallpaper_out only): its first overlay
        // frame carries no frost, so unlock has no blur or tint.
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
        // Opt-in blur (frost_out_ms > 0): the first reveal frame starts at
        // full frost, which then clears over the fade.
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

    /// Replay of the 2026-08-30 live run (vigil trace ec1b6c3f…): --warn
    /// 4000, wallpaper ready only at 8.3 s because a portrait output's
    /// background took a 6.9 s software resize. The real locker then armed
    /// no wake and never committed for 123 s.
    #[test]
    fn a_wallpaper_ready_after_the_hold_cap_still_commits() {
        replay(LockFlow::new(at(0), &warning_lock(4_000), None).0);
    }

    /// Identical cadence, but through the CLI path the real run used:
    /// `--warn 4000` is warning_ms_override on a default config, not a
    /// config whose warning block was populated.
    #[test]
    fn the_cli_warn_override_commits_like_a_configured_warning() {
        replay(LockFlow::new(at(0), &Lock::default(), Some(4_000)).0);
    }

    fn replay(mut flow: LockFlow) {
        flow.step(at(100), FlowEvent::WallpaperReady(false));
        for ms in [1_000, 2_000, 2_850, 2_851] {
            flow.step(at(ms), FlowEvent::Tick);
        }
        // Ready arrives AFTER the hold cap (4000 + 5000 = 9000? no: cap is
        // commit_at + hold = 4000+5000 = 9000; ready at 8309 is before) —
        // exercise both sides.
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

    /// Same, but ready arrives after the cap has already fired its
    /// forced-commit path internally.
    #[test]
    fn a_wallpaper_ready_after_the_cap_expired_still_commits() {
        let (mut flow, _) = LockFlow::new(at(0), &warning_lock(4_000), None);
        flow.step(at(100), FlowEvent::WallpaperReady(false));
        flow.step(at(2_851), FlowEvent::Tick);
        // sleep past the cap (9 000) with no tick — the real loop slept too
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
