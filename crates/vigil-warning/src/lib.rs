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
}

impl Timeline {
    pub fn new(config: LockWarning) -> Self {
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
        }
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

    pub fn hotplug(&mut self) {
        self.cancel();
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
        if !matches!(self.phase, Phase::Mapped | Phase::Running) {
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
        let wallpaper_start = self
            .wallpaper_ready_at
            .map_or(scheduled_wallpaper_start, |ready| {
                ready.max(scheduled_wallpaper_start)
            });
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
        if self.phase == Phase::Running && elapsed >= commit_at && self.wallpaper_ready {
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
            WarningKeyframe::WallpaperStart => Some(
                start
                    + self.wallpaper_ready_at.map_or_else(
                        || {
                            Duration::from_millis(
                                self.config
                                    .duration_ms
                                    .saturating_sub(self.config.wallpaper_in_ms),
                            )
                        },
                        |ready| {
                            ready.max(Duration::from_millis(
                                self.config
                                    .duration_ms
                                    .saturating_sub(self.config.wallpaper_in_ms),
                            ))
                        },
                    ),
            ),
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
        if matches!(self.phase, Phase::Mapped | Phase::Running) {
            self.phase = Phase::Cancelled;
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
}
