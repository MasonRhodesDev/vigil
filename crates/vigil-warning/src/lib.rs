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

/// Floor `elapsed` to the frame grid (value computation only).
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
            wallpaper_ready: true,
            wallpaper_ready_at: None,
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
                next_frame: None,
            };
        }
        let Some(start) = self.start else {
            return Sample {
                phase: self.phase,
                frost: 0.0,
                wallpaper: 0.0,
                should_commit: false,
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
        if self.phase == Phase::Running
            && elapsed >= commit_at
            && (self.wallpaper_ready || !self.cancelable)
        {
            self.phase = Phase::CommitReady;
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

    pub fn element_samples(&self, now: Duration) -> Vec<ElementSample> {
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
                let progress = self.keyframe_time(start).map_or(0.0, |keyframe| {
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
                            quantize(now),
                            start,
                            Duration::from_millis(duration_ms),
                            self.config.easing,
                        )
                    }
                });
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

/// Post-unlock fade (issue #52): the wallpaper dissolves into the frosted
/// desktop, then the frost clears. Same `(frost, wallpaper)` contract as the
/// warning's [`Sample`], consumed by the same overlay compositing.
pub struct Reveal {
    wallpaper_out: Duration,
    frost_out: Duration,
    easing: WarningEasing,
    start: Option<Duration>,
}

impl Reveal {
    pub fn new(wallpaper_out_ms: u64, frost_out_ms: u64, easing: WarningEasing) -> Self {
        Self {
            wallpaper_out: Duration::from_millis(wallpaper_out_ms),
            frost_out: Duration::from_millis(frost_out_ms),
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
            return RevealSample {
                frost: 1.0,
                wallpaper: 1.0,
                done: false,
                next_frame: None,
            };
        };
        let elapsed = now.saturating_sub(start);
        let grid = quantize(elapsed);
        let wallpaper = 1.0
            - graded(
                elapsed,
                grid,
                Duration::ZERO,
                self.wallpaper_out,
                self.easing,
            );
        let frost = 1.0
            - graded(
                elapsed,
                grid,
                self.wallpaper_out,
                self.frost_out,
                self.easing,
            );
        let done = elapsed >= self.wallpaper_out + self.frost_out;
        RevealSample {
            frost,
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

        timeline.set_wallpaper_ready(false, Duration::from_secs(2));
        assert_eq!(timeline.sample(Duration::from_secs(2)).next_frame, None);
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
    fn reveal_fades_wallpaper_then_frost_and_goes_quiet() {
        let mut reveal = Reveal::new(250, 150, WarningEasing::Linear);
        assert_eq!(
            reveal.sample(Duration::from_secs(5)),
            RevealSample {
                frost: 1.0,
                wallpaper: 1.0,
                done: false,
                next_frame: None
            }
        );
        reveal.start(Duration::from_secs(1));
        reveal.start(Duration::from_secs(9)); // idempotent
        // Grid-aligned offsets (multiples of the 33 ms frame period).
        let middle = reveal.sample(Duration::from_millis(1_000 + 4 * FRAME_INTERVAL_MS));
        assert!((middle.wallpaper - (1.0 - 132.0 / 250.0)).abs() < 1e-6);
        assert_eq!(middle.frost, 1.0);
        assert_eq!(
            middle.next_frame,
            Some(Duration::from_millis(FRAME_INTERVAL_MS))
        );
        let frost = reveal.sample(Duration::from_millis(1_000 + 10 * FRAME_INTERVAL_MS));
        assert_eq!(frost.wallpaper, 0.0);
        // One grid for the whole fade: at the on-grid sample 330 ms the
        // frost half has run 330-250 = 80 ms exactly.
        assert!((frost.frost - (1.0 - 80.0 / 150.0)).abs() < 1e-6);
        assert!(!frost.done);
        let end = reveal.sample(Duration::from_millis(1_400));
        assert!(end.done);
        assert_eq!(end.frost, 0.0);
        assert_eq!(end.next_frame, None);
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

        let mut reveal = Reveal::new(250, 150, WarningEasing::Linear);
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
        let mut reveal = Reveal::new(0, 0, WarningEasing::EaseOut);
        reveal.start(Duration::ZERO);
        let sample = reveal.sample(Duration::ZERO);
        assert!(sample.done);
        assert_eq!(sample.next_frame, None);
    }
}
