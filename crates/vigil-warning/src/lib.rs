//! Pure, deterministic pre-lock warning timeline.

use std::time::Duration;

use vigil_config::{LockWarning, WarningEasing, WarningKeyframe};
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

pub struct Timeline {
    config: LockWarning,
    phase: Phase,
    start: Option<Duration>,
    locked_at: Option<Duration>,
    commit_emitted: bool,
    pointer: Option<(f64, f64)>,
    motion: f64,
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
        let wallpaper_start = total.saturating_sub(wallpaper_duration);
        let frost = ease(ratio(elapsed, frost_duration), self.config.easing);
        let wallpaper = ease(
            ratio(elapsed.saturating_sub(wallpaper_start), wallpaper_duration),
            self.config.easing,
        );

        if self.phase == Phase::Running && elapsed >= total {
            self.phase = Phase::CommitReady;
        }
        let should_commit = self.phase == Phase::CommitReady && !self.commit_emitted;
        if should_commit {
            self.commit_emitted = true;
            self.phase = Phase::Committing;
        }
        let animating = (frost < 1.0 || wallpaper < 1.0) && self.phase == Phase::Running;
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
            next_frame: animating.then_some(Duration::from_millis(33)),
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
                    + Duration::from_millis(
                        self.config
                            .duration_ms
                            .saturating_sub(self.config.wallpaper_in_ms),
                    ),
            ),
            WarningKeyframe::WallpaperSolid => {
                Some(start + Duration::from_millis(self.config.duration_ms))
            }
            WarningKeyframe::Locked => self.locked_at,
            WarningKeyframe::None => None,
        }
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
}
