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
        let frost = ease(ratio(elapsed, frost_duration), self.config.easing);
        let wallpaper = if self.wallpaper_ready {
            ease(
                ratio(elapsed.saturating_sub(wallpaper_start), wallpaper_duration),
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
            Some(Duration::from_millis(33))
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
                        ease(
                            ratio(
                                now.saturating_sub(start),
                                Duration::from_millis(duration_ms),
                            ),
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
                    Some(Duration::from_millis(33))
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
        let wallpaper = 1.0 - ease(ratio(elapsed, self.wallpaper_out), self.easing);
        let frost = 1.0
            - ease(
                ratio(elapsed.saturating_sub(self.wallpaper_out), self.frost_out),
                self.easing,
            );
        let done = elapsed >= self.wallpaper_out + self.frost_out;
        RevealSample {
            frost,
            wallpaper,
            done,
            next_frame: (!done).then(|| Duration::from_millis(33)),
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
        let middle = reveal.sample(Duration::from_millis(1_125));
        assert!((middle.wallpaper - 0.5).abs() < 1e-6);
        assert_eq!(middle.frost, 1.0);
        assert_eq!(middle.next_frame, Some(Duration::from_millis(33)));
        let frost = reveal.sample(Duration::from_millis(1_325));
        assert_eq!(frost.wallpaper, 0.0);
        assert!((frost.frost - 0.5).abs() < 1e-6);
        assert!(!frost.done);
        let end = reveal.sample(Duration::from_millis(1_400));
        assert!(end.done);
        assert_eq!(end.frost, 0.0);
        assert_eq!(end.next_frame, None);
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
