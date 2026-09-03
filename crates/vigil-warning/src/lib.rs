//! Pure, deterministic pre-lock warning timeline.

use std::time::Duration;

use vigil_config::{LockWarning, WarningAnimation, WarningEasing, WarningKeyframe};
use vigil_core::InputEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Mapped,
    Running,
    CommitReady,
    Committing,
    Locked,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub phase: Phase,
    pub frost: f32,
    pub wallpaper: f32,
    pub should_commit: bool,
    /// This commit was forced by the wallpaper hold cap rather than reached
    /// normally: the asset never arrived and locking beat waiting. Set on
    /// the sample that forces it, so the caller can journal it once.
    pub forced_commit: bool,
    pub next_frame: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementSample {
    pub selector: String,
    pub progress: f32,
    pub kind: WarningAnimation,
}

pub const DEFAULT_SELECTORS: [&str; 5] = ["clock", "user_selector", "password", "status", "power"];

/// Frame period of every ramp this crate animates. Animated values are
/// quantized to this grid inside `sample()`/`element_samples()`, so a
/// consumer may sample at any rate — event-loop wakes included — and a
/// value-diff dedupes to at most one scene update per frame (issue #53:
/// buffer-release wakes otherwise self-sustain a commit-per-render-time
/// loop). Phase transitions, commit deadlines, and `next_frame` use real
/// elapsed time and are never delayed by the grid.
pub const FRAME_INTERVAL_MS: u64 = 33;

/// What a freshly constructed [`Timeline`] believes about wallpaper
/// readiness. Adapters that report readiness edge-triggered must seed their
/// cache from this, or the first genuine change is never reported and the
/// commit is not held for a slow asset.
pub const WALLPAPER_READY_DEFAULT: bool = true;

/// Floor a duration to the frame grid. Private and never called directly
/// from a ramp: the grid belongs to the timeline, and a ramp that derives
/// its own picks a different origin — see [`Timeline::grid`].
fn quantize(elapsed: Duration) -> Duration {
    Duration::from_millis(elapsed.as_millis() as u64 / FRAME_INTERVAL_MS * FRAME_INTERVAL_MS)
}

/// Eased ramp progress for a ramp starting at `start`, sampled on the
/// timeline's single frame grid: `now` is the real clock (completion is
/// judged on it, so the terminal value is exact even when the deadline is
/// off-grid) and `grid` is `quantize(now)` for the *whole* timeline.
///
/// Every ramp must share one grid. Flooring each ramp's own relative
/// elapsed instead gives each a grid anchored at its own start, and two
/// overlapping ramps then change value at two different phase offsets
/// inside the same frame — two scene updates and two commits per frame,
/// which is the storm issue #53 exists to stop.
fn graded(
    now: Duration,
    grid: Duration,
    start: Duration,
    duration: Duration,
    easing: WarningEasing,
) -> f32 {
    if now.saturating_sub(start) >= duration {
        1.0
    } else {
        ease(ratio(grid.saturating_sub(start), duration), easing)
    }
}

pub struct Timeline {
    config: LockWarning,
    phase: Phase,
    start: Option<Duration>,
    locked_at: Option<Duration>,
    commit_emitted: bool,
    pointer: Option<(f64, f64)>,
    motion: f64,
    wallpaper_ready: bool,
    wallpaper_ready_at: Option<Duration>,
    /// The wallpaper hold cap has already forced a commit; report it once.
    hold_expired: bool,
    /// False for the manual-lock transition (issue #52): input is ignored,
    /// hotplug commits, and the wallpaper never holds the commit.
    cancelable: bool,
}

impl Timeline {
    pub fn new(config: LockWarning) -> Self {
        Self::with_cancelable(config, true)
    }

    /// A frost-in ramp for a manual or before-sleep lock: the warning's
    /// keyframes, but nothing cancels it and nothing waits on it.
    pub fn new_transition(config: LockWarning) -> Self {
        Self::with_cancelable(config, false)
    }

    fn with_cancelable(config: LockWarning, cancelable: bool) -> Self {
        Self {
            config,
            phase: Phase::Mapped,
            start: None,
            locked_at: None,
            commit_emitted: false,
            pointer: None,
            motion: 0.0,
            wallpaper_ready: WALLPAPER_READY_DEFAULT,
            wallpaper_ready_at: None,
            hold_expired: false,
            cancelable,
        }
    }

    pub fn cancelable(&self) -> bool {
        self.cancelable
    }

    pub fn start(&mut self, now: Duration) {
        if self.phase == Phase::Mapped {
            self.start = Some(now);
            self.phase = Phase::Running;
        }
    }

    pub fn request_commit(&mut self) {
        if matches!(
            self.phase,
            Phase::Mapped | Phase::Running | Phase::CommitReady
        ) {
            self.phase = Phase::CommitReady;
        }
    }

    pub fn locked(&mut self, now: Duration) {
        self.phase = Phase::Locked;
        self.locked_at = Some(now);
    }

    /// Output topology changed before commitment. A cancelable warning
    /// cancels rather than risk partial coverage; a transition commits now.
    pub fn hotplug(&mut self) {
        if self.cancelable {
            self.cancel();
        } else {
            self.request_commit();
        }
    }

    pub fn set_wallpaper_ready(&mut self, ready: bool, now: Duration) {
        if ready && !self.wallpaper_ready {
            self.wallpaper_ready_at = Some(now);
        } else if !ready {
            self.wallpaper_ready_at = None;
        }
        self.wallpaper_ready = ready;
    }

    pub fn input(&mut self, event: &InputEvent) {
        if !self.cancelable || !matches!(self.phase, Phase::Mapped | Phase::Running) {
            return;
        }
        match event {
            InputEvent::Key { pressed: true, .. }
            | InputEvent::PointerButton { pressed: true, .. } => self.cancel(),
            InputEvent::PointerAbsolute { x, y } => {
                if let Some((last_x, last_y)) = self.pointer {
                    self.motion += (x - last_x).hypot(y - last_y);
                    if self.motion >= self.config.cancel_on_motion_px {
                        self.cancel();
                    }
                }
                self.pointer = Some((*x, *y));
            }
            InputEvent::PointerMotion { dx, dy } => {
                self.motion += dx.hypot(*dy);
                if self.motion >= self.config.cancel_on_motion_px {
                    self.cancel();
                }
            }
            _ => {}
        }
    }

    pub fn pointer_enter(&mut self, x: f64, y: f64) {
        self.pointer = Some((x, y));
    }

    pub fn sample(&mut self, now: Duration) -> Sample {
        if self.phase == Phase::Cancelled {
            return Sample {
                phase: self.phase,
                frost: 0.0,
                wallpaper: 0.0,
                should_commit: false,
                forced_commit: false,
                next_frame: None,
            };
        }
        let Some(start) = self.start else {
            return Sample {
                phase: self.phase,
                frost: 0.0,
                wallpaper: 0.0,
                should_commit: false,
                forced_commit: false,
                next_frame: None,
            };
        };
        let elapsed = now.saturating_sub(start);
        let total = Duration::from_millis(self.config.duration_ms);
        let frost_duration = Duration::from_millis(self.config.frost_in_ms);
        let wallpaper_duration = Duration::from_millis(self.config.wallpaper_in_ms);
        let scheduled_wallpaper_start = total.saturating_sub(wallpaper_duration);
        let wallpaper_start = if self.cancelable {
            self.wallpaper_ready_at
                .map_or(scheduled_wallpaper_start, |ready| {
                    ready.max(scheduled_wallpaper_start)
                })
        } else {
            scheduled_wallpaper_start
        };
        // Values on the frame grid, phases on real time: identical values
        // inside one frame make any-rate sampling idempotent for consumers
        // that diff progress, while commits and holds stay punctual.
        let grid = quantize(elapsed);
        debug_assert_eq!(
            self.grid(now)
                .map(|absolute| absolute.saturating_sub(start)),
            Some(grid),
            "the overlay grid and Timeline::grid disagree"
        );
        let frost = graded(
            elapsed,
            grid,
            Duration::ZERO,
            frost_duration,
            self.config.easing,
        );
        let wallpaper = if self.wallpaper_ready {
            graded(
                elapsed,
                grid,
                wallpaper_start,
                wallpaper_duration,
                self.config.easing,
            )
        } else {
            0.0
        };

        let commit_at = wallpaper_start + wallpaper_duration;
        // The hold cap lives here, beside the commit time it is relative to:
        // computing it anywhere else duplicates this arithmetic, and every
        // copy is a chance to disagree with it (issue #56).
        let hold_deadline = (self.cancelable && self.config.wallpaper_hold_max_ms > 0)
            .then(|| commit_at + Duration::from_millis(self.config.wallpaper_hold_max_ms));
        let mut forced_commit = false;
        if self.phase == Phase::Running
            && elapsed >= commit_at
            && (self.wallpaper_ready || !self.cancelable)
        {
            self.phase = Phase::CommitReady;
        } else if self.phase == Phase::Running
            && !self.wallpaper_ready
            && hold_deadline.is_some_and(|deadline| elapsed >= deadline)
        {
            // A wedged asset pipeline must not leave the machine unlocked:
            // lock with whatever the scene has.
            self.phase = Phase::CommitReady;
            forced_commit = !self.hold_expired;
            self.hold_expired = true;
        }
        let should_commit = self.phase == Phase::CommitReady && !self.commit_emitted;
        if should_commit {
            self.commit_emitted = true;
            self.phase = Phase::Committing;
        }
        let animating = self.phase == Phase::Running
            && (elapsed < frost_duration
                || (self.wallpaper_ready && elapsed >= wallpaper_start && elapsed < commit_at));
        let next_frame = if animating {
            Some(Duration::from_millis(FRAME_INTERVAL_MS))
        } else if self.phase == Phase::Running && self.wallpaper_ready && elapsed < wallpaper_start
        {
            Some(wallpaper_start - elapsed)
        } else if self.phase == Phase::Running
            && let Some(deadline) = hold_deadline
            && !self.wallpaper_ready
        {
            // Nothing else arms a wake while the commit is held.
            Some(
                deadline
                    .saturating_sub(elapsed)
                    .max(Duration::from_millis(1)),
            )
        } else if self.phase == Phase::Running && !self.cancelable {
            // A transition commits on schedule whether or not the wallpaper
            // ever arrives: always wake for the commit.
            Some((commit_at.saturating_sub(elapsed)).max(Duration::from_millis(1)))
        } else {
            None
        };
        Sample {
            phase: self.phase,
            frost: if matches!(
                self.phase,
                Phase::CommitReady | Phase::Committing | Phase::Locked
            ) {
                1.0
            } else {
                frost
            },
            wallpaper: if matches!(
                self.phase,
                Phase::CommitReady | Phase::Committing | Phase::Locked
            ) {
                1.0
            } else {
                wallpaper
            },
            should_commit,
            forced_commit,
            next_frame,
        }
    }

    pub fn keyframe_time(&self, keyframe: WarningKeyframe) -> Option<Duration> {
        let start = self.start?;
        match keyframe {
            WarningKeyframe::Painted | WarningKeyframe::FrostStart => Some(start),
            WarningKeyframe::FrostEnd => {
                Some(start + Duration::from_millis(self.config.frost_in_ms))
            }
            WarningKeyframe::WallpaperStart => {
                let scheduled = Duration::from_millis(
                    self.config
                        .duration_ms
                        .saturating_sub(self.config.wallpaper_in_ms),
                );
                let ready = self.wallpaper_ready_at.filter(|_| self.cancelable);
                Some(start + ready.map_or(scheduled, |ready| ready.max(scheduled)))
            }
            WarningKeyframe::WallpaperSolid => {
                Some(start + Duration::from_millis(self.config.duration_ms))
            }
            WarningKeyframe::Locked => self.locked_at,
            WarningKeyframe::None => None,
        }
    }

    /// The one frame grid for this timeline at `now`, in the same absolute
    /// terms the caller passes in. Every ramp — overlay and GUI element —
    /// must derive its value from this, or two ramps land on grids offset
    /// by the timeline's start and the scene changes twice per frame
    /// (issue #53's mechanism, one layer up).
    fn grid(&self, now: Duration) -> Option<Duration> {
        let start = self.start?;
        Some(start + quantize(now.saturating_sub(start)))
    }

    pub fn element_samples(&self, now: Duration) -> Vec<ElementSample> {
        // Before the timeline starts there is no grid; nothing animates.
        let grid = self.grid(now).unwrap_or(now);
        DEFAULT_SELECTORS
            .iter()
            .map(|selector| {
                let animation = self
                    .config
                    .gui
                    .element
                    .iter()
                    .rev()
                    .find(|element| element.selector == *selector);
                let (start, offset_ms, duration_ms, kind) = animation.map_or(
                    (
                        self.config.gui.start,
                        self.config.gui.offset_ms,
                        self.config.gui.duration_ms,
                        self.config.gui.kind,
                    ),
                    |element| {
                        (
                            element.start,
                            element.offset_ms,
                            element.duration_ms,
                            element.kind,
                        )
                    },
                );
                // A start that can *never* resolve is not an animation that
                // has yet to begin -- it is an element with no start at all.
                // Pinning it at 0.0 left it invisible for the life of the
                // lock and kept `next_gui_wake` returning a frame period
                // for ever, which is a 30 Hz wake in a settled locked
                // session (#65). A keyframe that merely has not resolved
                // *yet* (`Locked` before the lock lands) still holds at 0.0.
                let unreachable_start = start == WarningKeyframe::None;
                let progress = self.keyframe_time(start).map_or(
                    if unreachable_start { 1.0 } else { 0.0 },
                    |keyframe| {
                        let start = keyframe + Duration::from_millis(offset_ms);
                        if now < start {
                            0.0
                        } else if kind == WarningAnimation::None || duration_ms == 0 {
                            1.0
                        } else {
                            // Same single grid: per-selector `offset_ms` would
                            // otherwise give each element its own phase and
                            // multiply the per-frame scene updates.
                            graded(
                                now,
                                grid,
                                start,
                                Duration::from_millis(duration_ms),
                                self.config.easing,
                            )
                        }
                    },
                );
                ElementSample {
                    selector: (*selector).into(),
                    progress,
                    kind,
                }
            })
            .collect()
    }

    pub fn gui_complete(&self, now: Duration) -> bool {
        self.phase == Phase::Locked
            && self
                .element_samples(now)
                .iter()
                .all(|element| element.progress >= 1.0 || element.kind == WarningAnimation::None)
    }

    pub fn next_gui_wake(&self, now: Duration) -> Option<Duration> {
        self.element_samples(now)
            .into_iter()
            .filter_map(|element| {
                if self.phase == Phase::Locked
                    && element.progress < 1.0
                    && element.kind != WarningAnimation::None
                {
                    Some(Duration::from_millis(FRAME_INTERVAL_MS))
                } else {
                    None
                }
            })
            .min()
    }

    fn cancel(&mut self) {
        if self.cancelable && matches!(self.phase, Phase::Mapped | Phase::Running) {
            self.phase = Phase::Cancelled;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RevealSample {
    pub frost: f32,
    pub wallpaper: f32,
    pub done: bool,
    pub next_frame: Option<Duration>,
}

/// Post-unlock fade: the lock wallpaper's opacity fades out, uncovering the
/// desktop. Deliberately NO frost - blur is a pre-lock warning signal only,
/// so unlocking reveals a sharp desktop (the reveal overlay carries no blur
/// region and `frost` stays 0, so no tint and no compositor blur). The
/// `(frost, wallpaper)` contract is shared with the warning's [`Sample`];
/// here `frost` is pinned 0 and `wallpaper` is the fading opacity.
pub struct Reveal {
    fade: Duration,
    easing: WarningEasing,
    start: Option<Duration>,
}

impl Reveal {
    pub fn new(fade_ms: u64, easing: WarningEasing) -> Self {
        Self {
            fade: Duration::from_millis(fade_ms),
            easing,
            start: None,
        }
    }

    /// Idempotent: the first call fixes the origin.
    pub fn start(&mut self, now: Duration) {
        if self.start.is_none() {
            self.start = Some(now);
        }
    }

    pub fn started(&self) -> bool {
        self.start.is_some()
    }

    pub fn sample(&self, now: Duration) -> RevealSample {
        let Some(start) = self.start else {
            // Not started: the lock wallpaper still fully covers, but frost
            // is 0 - the reveal never blurs.
            return RevealSample {
                frost: 0.0,
                wallpaper: 1.0,
                done: false,
                next_frame: None,
            };
        };
        let elapsed = now.saturating_sub(start);
        let grid = quantize(elapsed);
        // The lock wallpaper fades out uniformly; the sharp desktop appears
        // through it. frost stays 0 - no tint, no blur on unlock.
        let wallpaper = 1.0 - graded(elapsed, grid, Duration::ZERO, self.fade, self.easing);
        let done = elapsed >= self.fade;
        RevealSample {
            frost: 0.0,
            wallpaper,
            done,
            next_frame: (!done).then(|| Duration::from_millis(FRAME_INTERVAL_MS)),
        }
    }
}

fn ratio(elapsed: Duration, duration: Duration) -> f32 {
    if duration.is_zero() {
        1.0
    } else {
        (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
    }
}

fn ease(value: f32, easing: WarningEasing) -> f32 {
    match easing {
        WarningEasing::Linear => value,
        WarningEasing::EaseOut => 1.0 - (1.0 - value).powi(3),
        WarningEasing::EaseInOut => {
            if value < 0.5 {
                4.0 * value.powi(3)
            } else {
                1.0 - (-2.0 * value + 2.0).powi(3) / 2.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LockWarning {
        LockWarning {
            duration_ms: 10_000,
            frost_in_ms: 1_500,
            wallpaper_in_ms: 1_500,
            ..LockWarning::default()
        }
    }

    #[test]
    fn timeline_reaches_commit_with_opaque_wallpaper() {
        let mut timeline = Timeline::new(config());
        timeline.start(Duration::ZERO);
        let sample = timeline.sample(Duration::from_secs(10));
        assert!(sample.should_commit);
        assert_eq!(sample.wallpaper, 1.0);
        assert_eq!(sample.frost, 1.0);
        assert!(!timeline.sample(Duration::from_secs(11)).should_commit);
    }

    #[test]
    fn input_cancels_before_but_not_after_commit() {
        let key = InputEvent::Key {
            keysym: 1,
            utf8: None,
            pressed: true,
        };
        let mut before = Timeline::new(config());
        before.start(Duration::ZERO);
        before.input(&key);
        assert_eq!(
            before.sample(Duration::from_secs(1)).phase,
            Phase::Cancelled
        );
        let mut after = Timeline::new(config());
        after.start(Duration::ZERO);
        after.request_commit();
        after.input(&key);
        assert!(after.sample(Duration::ZERO).should_commit);
    }

    #[test]
    fn accumulated_motion_and_hotplug_cancel() {
        let mut timeline = Timeline::new(config());
        timeline.start(Duration::ZERO);
        timeline.input(&InputEvent::PointerMotion { dx: 4.0, dy: 0.0 });
        assert_eq!(timeline.sample(Duration::ZERO).phase, Phase::Running);
        timeline.input(&InputEvent::PointerMotion { dx: 4.0, dy: 0.0 });
        assert_eq!(timeline.sample(Duration::ZERO).phase, Phase::Cancelled);
        let mut hotplug = Timeline::new(config());
        hotplug.start(Duration::ZERO);
        hotplug.hotplug();
        assert_eq!(hotplug.sample(Duration::ZERO).phase, Phase::Cancelled);
    }

    #[test]
    fn a_start_keyframe_that_never_resolves_settles_instead_of_waking_for_ever() {
        // `start = none` names no keyframe, so the animation can never
        // begin. Pinning it at progress 0.0 left the element invisible for
        // the whole lock AND kept next_gui_wake returning a frame period --
        // a permanent 30 Hz wake in a settled locked session (#65). This is
        // reachable from config: WarningElement::default() pairs
        // `start: None` with `kind: None`, so a theme that sets `kind` and
        // omits `start` lands here.
        let mut cfg = config();
        cfg.gui.element = vec![vigil_config::WarningElement {
            selector: "power".into(),
            start: WarningKeyframe::None,
            offset_ms: 0,
            duration_ms: 300,
            kind: WarningAnimation::Fade,
        }];
        let mut timeline = Timeline::new(cfg);
        timeline.start(Duration::ZERO);
        timeline.locked(Duration::from_secs(1));
        let late = Duration::from_secs(600);
        let power = timeline
            .element_samples(late)
            .into_iter()
            .find(|e| e.selector.as_str() == "power")
            .expect("power element");
        assert!(
            power.progress >= 1.0,
            "an animation that can never start must render settled, not invisible"
        );
        assert!(timeline.gui_complete(late));
        assert_eq!(timeline.next_gui_wake(late), None);
    }

    #[test]
    fn keyframes_are_deterministic() {
        let mut timeline = Timeline::new(config());
        timeline.start(Duration::from_secs(2));
        assert_eq!(
            timeline.keyframe_time(WarningKeyframe::FrostEnd),
            Some(Duration::from_millis(3_500))
        );
        assert_eq!(
            timeline.keyframe_time(WarningKeyframe::WallpaperStart),
            Some(Duration::from_millis(10_500))
        );
        assert_eq!(timeline.keyframe_time(WarningKeyframe::Locked), None);
        timeline.locked(Duration::from_secs(12));
        assert_eq!(
            timeline.keyframe_time(WarningKeyframe::Locked),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn late_wallpaper_holds_commit_and_gets_a_full_fade() {
        let mut timeline = Timeline::new(config());
        timeline.start(Duration::ZERO);
        timeline.set_wallpaper_ready(false, Duration::ZERO);
        let held = timeline.sample(Duration::from_secs(12));
        assert_eq!(held.phase, Phase::Running);
        assert_eq!(held.wallpaper, 0.0);
        assert!(!held.should_commit);

        timeline.set_wallpaper_ready(true, Duration::from_secs(12));
        let start = timeline.sample(Duration::from_secs(12));
        assert_eq!(start.wallpaper, 0.0);
        let middle = timeline.sample(Duration::from_millis(12_750));
        assert!(middle.wallpaper > 0.0 && middle.wallpaper < 1.0);
        let committed = timeline.sample(Duration::from_millis(13_500));
        assert_eq!(committed.wallpaper, 1.0);
        assert!(committed.should_commit);
    }

    #[test]
    fn gui_defaults_wait_for_lock_and_selector_override_can_start_early() {
        let mut config = config();
        config.gui.element.push(vigil_config::WarningElement {
            selector: "clock".into(),
            start: WarningKeyframe::Painted,
            duration_ms: 0,
            kind: WarningAnimation::None,
            ..Default::default()
        });
        let mut timeline = Timeline::new(config);
        timeline.start(Duration::ZERO);
        let before = timeline.element_samples(Duration::from_secs(1));
        assert_eq!(
            before
                .iter()
                .find(|e| e.selector == "clock")
                .unwrap()
                .progress,
            1.0
        );
        assert_eq!(
            before
                .iter()
                .find(|e| e.selector == "password")
                .unwrap()
                .progress,
            0.0
        );

        timeline.request_commit();
        timeline.sample(Duration::from_secs(1));
        timeline.locked(Duration::from_secs(2));
        let middle = timeline.element_samples(Duration::from_millis(2_200));
        let password = middle.iter().find(|e| e.selector == "password").unwrap();
        assert!(password.progress > 0.0 && password.progress < 1.0);
        assert!(timeline.gui_complete(Duration::from_millis(2_400)));
    }

    #[test]
    fn static_frost_has_no_frame_clock() {
        let mut timeline = Timeline::new(config());
        timeline.start(Duration::ZERO);
        let held = timeline.sample(Duration::from_secs(2));
        assert_eq!(held.frost, 1.0);
        assert_eq!(held.wallpaper, 0.0);
        assert_eq!(held.next_frame, Some(Duration::from_millis(6_500)));

        // Holding for an absent wallpaper is static too — but it is NOT
        // wake-less: the hold cap is the only thing that will ever move
        // this warning along, so it arms its own deadline. A `None` here
        // is issue #56, not a quiet idle.
        timeline.set_wallpaper_ready(false, Duration::from_secs(2));
        let held = timeline.sample(Duration::from_secs(2));
        assert_eq!(held.next_frame, Some(Duration::from_millis(13_000)));
        assert!(!held.should_commit);

        // With the cap disabled the old wake-less hold is what the operator
        // explicitly asked for.
        let mut forever = Timeline::new(LockWarning {
            wallpaper_hold_max_ms: 0,
            ..config()
        });
        forever.start(Duration::ZERO);
        forever.set_wallpaper_ready(false, Duration::ZERO);
        assert_eq!(forever.sample(Duration::from_secs(2)).next_frame, None);
    }

    fn transition() -> LockWarning {
        vigil_config::LockTransition::default()
            .as_warning(0.35, vigil_config::WarningGui::default())
    }

    #[test]
    fn transition_ignores_input_and_commits_on_schedule() {
        let key = InputEvent::Key {
            keysym: 1,
            utf8: None,
            pressed: true,
        };
        let mut timeline = Timeline::new_transition(transition());
        assert!(!timeline.cancelable());
        timeline.start(Duration::ZERO);
        timeline.input(&key);
        timeline.input(&InputEvent::PointerMotion { dx: 500.0, dy: 0.0 });
        assert_eq!(
            timeline.sample(Duration::from_millis(100)).phase,
            Phase::Running
        );
        let commit = timeline.sample(Duration::from_millis(400));
        assert!(commit.should_commit);
        assert_eq!(commit.frost, 1.0);
        assert_eq!(commit.wallpaper, 1.0);
        assert!(!timeline.sample(Duration::from_millis(401)).should_commit);
    }

    #[test]
    fn transition_hotplug_commits_instead_of_cancelling() {
        let mut timeline = Timeline::new_transition(transition());
        timeline.start(Duration::ZERO);
        timeline.hotplug();
        let sample = timeline.sample(Duration::from_millis(50));
        assert_eq!(sample.phase, Phase::Committing);
        assert!(sample.should_commit);
    }

    #[test]
    fn transition_does_not_wait_for_wallpaper() {
        let mut timeline = Timeline::new_transition(transition());
        timeline.start(Duration::ZERO);
        timeline.set_wallpaper_ready(false, Duration::ZERO);
        let held = timeline.sample(Duration::from_millis(200));
        assert_eq!(held.phase, Phase::Running);
        assert_eq!(held.wallpaper, 0.0);
        assert_eq!(held.next_frame, Some(Duration::from_millis(200)));
        let commit = timeline.sample(Duration::from_millis(400));
        assert!(commit.should_commit);
        assert_eq!(commit.wallpaper, 1.0);
        assert_eq!(
            timeline.keyframe_time(WarningKeyframe::WallpaperStart),
            Some(Duration::from_millis(150))
        );
    }

    #[test]
    fn transition_keeps_cancelable_warning_semantics() {
        let key = InputEvent::Key {
            keysym: 1,
            utf8: None,
            pressed: true,
        };
        let mut warning = Timeline::new(config());
        assert!(warning.cancelable());
        warning.start(Duration::ZERO);
        warning.input(&key);
        assert_eq!(warning.sample(Duration::ZERO).phase, Phase::Cancelled);
    }

    #[test]
    fn reveal_fades_the_wallpaper_out_with_zero_frost() {
        // Unlock has no blur or tint: frost is 0 at every sample and the
        // lock wallpaper's opacity fades to reveal the sharp desktop.
        let mut reveal = Reveal::new(250, WarningEasing::Linear);
        assert_eq!(
            reveal.sample(Duration::from_secs(5)),
            RevealSample {
                frost: 0.0,
                wallpaper: 1.0,
                done: false,
                next_frame: None
            }
        );
        reveal.start(Duration::from_secs(1));
        reveal.start(Duration::from_secs(9)); // idempotent
        let middle = reveal.sample(Duration::from_millis(1_000 + 4 * FRAME_INTERVAL_MS));
        assert!((middle.wallpaper - (1.0 - 132.0 / 250.0)).abs() < 1e-6);
        assert_eq!(middle.frost, 0.0, "no frost/blur mid-reveal");
        assert_eq!(
            middle.next_frame,
            Some(Duration::from_millis(FRAME_INTERVAL_MS))
        );
        // The whole fade is one phase now: done at the fade duration.
        let end = reveal.sample(Duration::from_millis(1_250));
        assert!(end.done);
        assert_eq!(end.wallpaper, 0.0);
        assert_eq!(end.frost, 0.0);
        assert_eq!(end.next_frame, None);
    }

    #[test]
    fn reveal_never_emits_frost() {
        // The invariant the fix rests on, across the whole fade: not one
        // sample carries frost, so nothing can blur or tint the desktop
        // during unlock.
        let mut reveal = Reveal::new(400, WarningEasing::EaseOut);
        reveal.start(Duration::ZERO);
        for ms in (0..=500).step_by(FRAME_INTERVAL_MS as usize) {
            assert_eq!(
                reveal.sample(Duration::from_millis(ms)).frost,
                0.0,
                "frost at {ms} ms must be 0"
            );
        }
    }

    #[test]
    fn the_hold_cap_is_measured_from_the_scheduled_commit() {
        // The claim under test: the cap starts at commit_at, NOT at t0.
        // Without the boundary assertions below, a deadline measured from
        // t0 (which would lock a 30 s warning 25 s early) still passes.
        let mut timeline = Timeline::new(LockWarning {
            duration_ms: 3_000,
            wallpaper_in_ms: 1_500,
            wallpaper_hold_max_ms: 5_000,
            ..LockWarning::default()
        });
        timeline.start(Duration::ZERO);
        timeline.set_wallpaper_ready(false, Duration::ZERO);
        // commit_at = 3000, so the cap is due at exactly 8000.
        let held = timeline.sample(Duration::from_millis(7_999));
        assert!(!held.should_commit, "fired early");
        assert!(!held.forced_commit);
        assert_eq!(
            held.next_frame,
            Some(Duration::from_millis(1)),
            "the cap must arm its own wake — nothing else does while held"
        );
        let forced = timeline.sample(Duration::from_millis(8_000));
        assert!(forced.should_commit);
        assert!(
            forced.forced_commit,
            "the caller must be able to journal it"
        );
        // Reported once only.
        let mut after = Timeline::new(LockWarning {
            duration_ms: 3_000,
            wallpaper_in_ms: 1_500,
            wallpaper_hold_max_ms: 5_000,
            ..LockWarning::default()
        });
        after.start(Duration::ZERO);
        after.set_wallpaper_ready(false, Duration::ZERO);
        after.sample(Duration::from_millis(8_000));
        assert!(!after.sample(Duration::from_millis(8_100)).forced_commit);
    }

    #[test]
    fn a_fade_longer_than_its_warning_is_not_cut_short() {
        // commit_at is max(duration, wallpaper_in), so a cap derived from
        // duration alone fires during a perfectly healthy run.
        let mut timeline = Timeline::new(LockWarning {
            duration_ms: 2_000,
            wallpaper_in_ms: 10_000,
            wallpaper_hold_max_ms: 5_000,
            ..LockWarning::default()
        });
        timeline.start(Duration::ZERO);
        let healthy = timeline.sample(Duration::from_millis(7_000));
        assert!(!healthy.should_commit, "cut a healthy fade short");
        assert!(!healthy.forced_commit);
        assert!(timeline.sample(Duration::from_millis(10_000)).should_commit);
    }

    #[test]
    fn a_late_wallpaper_still_gets_its_full_fade() {
        // ADR 0004: late assets extend the warning. Once one arrives the
        // cap must stand down rather than truncate the fade.
        let mut timeline = Timeline::new(LockWarning {
            duration_ms: 3_000,
            wallpaper_in_ms: 1_500,
            wallpaper_hold_max_ms: 5_000,
            ..LockWarning::default()
        });
        timeline.start(Duration::ZERO);
        timeline.set_wallpaper_ready(false, Duration::ZERO);
        timeline.set_wallpaper_ready(true, Duration::from_millis(7_500));
        let past_naive_deadline = timeline.sample(Duration::from_millis(8_000));
        assert!(!past_naive_deadline.should_commit);
        assert!(
            !past_naive_deadline.forced_commit,
            "the wallpaper did arrive"
        );
        assert!(timeline.sample(Duration::from_millis(9_000)).should_commit);
    }

    #[test]
    fn a_zero_cap_waits_forever_as_before() {
        let mut timeline = Timeline::new(LockWarning {
            duration_ms: 3_000,
            wallpaper_hold_max_ms: 0,
            ..LockWarning::default()
        });
        timeline.start(Duration::ZERO);
        timeline.set_wallpaper_ready(false, Duration::ZERO);
        let held = timeline.sample(Duration::from_secs(600));
        assert!(!held.should_commit);
        assert_eq!(held.next_frame, None);
    }

    #[test]
    fn the_transition_ignores_the_hold_cap() {
        // A non-cancelable transition never waits on the wallpaper, so the
        // cap must never be what commits it.
        let mut timeline = Timeline::new_transition(LockWarning {
            duration_ms: 400,
            frost_in_ms: 150,
            wallpaper_in_ms: 250,
            wallpaper_hold_max_ms: 5_000,
            ..LockWarning::default()
        });
        timeline.start(Duration::ZERO);
        timeline.set_wallpaper_ready(false, Duration::ZERO);
        let commit = timeline.sample(Duration::from_millis(400));
        assert!(commit.should_commit, "the transition commits on schedule");
        assert!(!commit.forced_commit);
    }

    #[test]
    fn every_ramp_shares_one_grid_even_off_grid_and_with_gui_elements() {
        // The overlay ramps quantize timeline-RELATIVE elapsed; the GUI
        // element ramps quantized ABSOLUTE now. Whenever the timeline's
        // start is not a multiple of the frame period — 32 times out of 33
        // — those are two grids, and the scene changes at two phase offsets
        // per frame. That is the storm issue #53 exists to stop, one layer
        // up, and it survived the fix for the overlay ramps.
        let mut config = LockWarning {
            duration_ms: 2_000,
            frost_in_ms: 1_500,
            wallpaper_in_ms: 1_500,
            ..LockWarning::default()
        };
        // A pre-lock keyframe, so element progress moves while the overlay
        // ramps do. The shipped default starts at Locked, which is why this
        // stayed invisible.
        config.gui.start = WarningKeyframe::FrostStart;
        config.gui.duration_ms = 2_000;
        let start = Duration::from_millis(17); // deliberately off-grid
        let mut timeline = Timeline::new(config);
        timeline.start(start);

        let mut changes = 0;
        let mut last = None;
        for ms in 0..1_000 {
            let now = start + Duration::from_millis(ms);
            let sample = timeline.sample(now);
            let elements: Vec<u32> = timeline
                .element_samples(now)
                .iter()
                .map(|e| e.progress.to_bits())
                .collect();
            let value = (sample.frost.to_bits(), sample.wallpaper.to_bits(), elements);
            if last.as_ref() != Some(&value) {
                changes += 1;
                last = Some(value);
            }
        }
        let frames = 1_000 / FRAME_INTERVAL_MS + 1;
        assert!(
            changes <= frames,
            "{changes} scene changes over 1000 ms, expected at most {frames} —              the element grid is not the timeline's grid"
        );
    }

    #[test]
    fn sampling_is_idempotent_within_a_frame() {
        // Any-rate sampling (event-loop wakes) must not manufacture new
        // values between frames — the contract that fixes issue #53 at the
        // source instead of in every consumer.
        let mut timeline = Timeline::new_transition(transition());
        timeline.start(Duration::ZERO);
        let a = timeline.sample(Duration::from_millis(2 * FRAME_INTERVAL_MS));
        let b = timeline.sample(Duration::from_millis(2 * FRAME_INTERVAL_MS + 10));
        assert_eq!((a.frost, a.wallpaper), (b.frost, b.wallpaper));
        let c = timeline.sample(Duration::from_millis(3 * FRAME_INTERVAL_MS));
        assert_ne!((a.frost, a.wallpaper), (c.frost, c.wallpaper));

        let mut reveal = Reveal::new(250, WarningEasing::Linear);
        reveal.start(Duration::ZERO);
        let a = reveal.sample(Duration::from_millis(2 * FRAME_INTERVAL_MS));
        let b = reveal.sample(Duration::from_millis(2 * FRAME_INTERVAL_MS + 10));
        assert_eq!((a.frost, a.wallpaper), (b.frost, b.wallpaper));

        // Phase timing stays on real time: the transition still commits at
        // its exact deadline even off-grid.
        let mut timeline = Timeline::new_transition(transition());
        timeline.start(Duration::ZERO);
        assert!(timeline.sample(Duration::from_millis(400)).should_commit);
    }

    #[test]
    fn overlapping_ramps_share_one_frame_grid() {
        // frost_in + wallpaper_in > duration makes both ramps animate at
        // once. Each ramp flooring its own relative elapsed would put their
        // value changes at two phase offsets per frame — two scene updates
        // and two commits, the storm issue #53 exists to stop.
        let mut timeline = Timeline::new(LockWarning {
            duration_ms: 2_000,
            frost_in_ms: 1_500,
            wallpaper_in_ms: 1_500,
            ..LockWarning::default()
        });
        timeline.start(Duration::ZERO);
        let mut changes = 0;
        let mut last = None;
        // Walk the overlap window (wallpaper_start = 500) one ms at a time.
        for ms in 500..1_500 {
            let sample = timeline.sample(Duration::from_millis(ms));
            let value = (sample.frost, sample.wallpaper);
            if last != Some(value) {
                changes += 1;
                last = Some(value);
            }
        }
        // 1000 ms at one change per 33 ms frame, inclusive of the first.
        let frames = 1_000 / FRAME_INTERVAL_MS + 1;
        assert!(
            changes <= frames,
            "{changes} value changes over the overlap, expected at most {frames}"
        );
    }

    #[test]
    fn zero_length_reveal_is_done_immediately() {
        let mut reveal = Reveal::new(0, WarningEasing::EaseOut);
        reveal.start(Duration::ZERO);
        let sample = reveal.sample(Duration::ZERO);
        assert!(sample.done);
        assert_eq!(sample.next_frame, None);
    }
}
