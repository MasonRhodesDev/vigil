//! The shared scene spec: one description of geometry, palette and motion
//! for every stage that draws between power-on and the desktop (the boot
//! splash package owns it and writes it as JSON). vigil follows it for its
//! two stages, `greeter-splash` and `greeter`.
//!
//! Every value has a built-in default equal to the spec's, so a machine
//! without the boot-splash package still gets a coherent splash, and a file
//! that is missing, unreadable or of another version is a log line, never a
//! failure.

use std::path::Path;

use serde_json::Value;

/// Round half up, the rule the other drawers use.
fn rnd(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// Pixel geometry of the mark on one screen (origin top-left, upright).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub logo: u32,
    pub glow: u32,
    pub logo_x: i64,
    pub logo_y: i64,
    pub glow_x: i64,
    pub glow_y: i64,
}

/// A debug tag's sizes, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagGeometry {
    pub border: u32,
    pub margin: u32,
    pub pt: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// When set, every screen sizes the scene from this fixed short side in
    /// physical pixels, so the mark is one size on every head.
    pub ref_short_side: Option<u32>,
    pub bg: [u8; 3],
    pub accent: [u8; 3],
    pub mark: [u8; 3],
    pub logo_frac: f64,
    pub glow_mult: u32,
    pub glow_blur_div: u32,
    pub centre_frac: f64,
    pub in_s: f64,
    pub out_s: f64,
    pub breath_period_s: f64,
    pub glow_min: f64,
    pub glow_max: f64,
    pub stage_fade_s: f64,
    pub debug_env: String,
    pub debug_border_frac: f64,
    pub debug_border_min: u32,
    pub debug_margin_frac: f64,
    pub debug_pt_frac: f64,
    pub splash_debug: [u8; 3],
    pub greeter_debug: [u8; 3],
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            ref_short_side: None,
            bg: [0x0b, 0x0d, 0x10],
            accent: [0x17, 0x93, 0xd1],
            mark: [0xc3, 0xc9, 0xd0],
            logo_frac: 0.16,
            glow_mult: 2,
            glow_blur_div: 6,
            centre_frac: 0.47,
            in_s: 0.4,
            out_s: 0.3,
            breath_period_s: 3.2,
            glow_min: 0.12,
            glow_max: 0.42,
            stage_fade_s: 0.4,
            debug_env: "DEBUG_STAGE_TAGS".into(),
            debug_border_frac: 0.004,
            debug_border_min: 4,
            debug_margin_frac: 0.012,
            debug_pt_frac: 0.018,
            splash_debug: [0x3c, 0xb4, 0x4b],
            greeter_debug: [0x42, 0xd4, 0xf4],
        }
    }
}

/// `#rrggbb` to bytes.
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(k))
}

impl Scene {
    /// Read the spec at `path`. Anything wrong keeps the defaults.
    pub fn load(path: &Path) -> Scene {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "vigil: scene spec {}: {e}; using the built-in defaults",
                    path.display()
                );
                return Scene::default();
            }
        };
        match Scene::parse(&text) {
            Ok(scene) => scene,
            Err(e) => {
                eprintln!(
                    "vigil: scene spec {}: {e}; using the built-in defaults",
                    path.display()
                );
                Scene::default()
            }
        }
    }

    pub fn parse(text: &str) -> Result<Scene, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        match at(&v, &["version"]).and_then(Value::as_u64) {
            Some(2) => {}
            other => return Err(format!("unsupported version {other:?}")),
        }
        let d = Scene::default();
        let num = |path: &[&str], default: f64| {
            at(&v, path)
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite())
                .unwrap_or(default)
        };
        let int = |path: &[&str], default: u32| {
            at(&v, path)
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .unwrap_or(default)
        };
        let color = |path: &[&str], default: [u8; 3]| {
            at(&v, path)
                .and_then(Value::as_str)
                .and_then(parse_color)
                .unwrap_or(default)
        };
        let scene = Scene {
            ref_short_side: at(&v, &["ref_short_side"])
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .filter(|x| *x > 0),
            bg: color(&["palette", "bg"], d.bg),
            accent: color(&["palette", "accent"], d.accent),
            mark: color(&["palette", "mark"], d.mark),
            logo_frac: num(&["geometry", "logo_frac"], d.logo_frac),
            glow_mult: int(&["geometry", "glow_mult"], d.glow_mult).max(1),
            glow_blur_div: int(&["geometry", "glow_blur_div"], d.glow_blur_div).max(1),
            centre_frac: num(&["geometry", "centre_frac_of_h"], d.centre_frac),
            in_s: num(&["motion", "in_s"], d.in_s),
            out_s: num(&["motion", "out_s"], d.out_s),
            breath_period_s: num(&["motion", "breath_period_s"], d.breath_period_s).max(0.001),
            glow_min: num(&["motion", "glow_min"], d.glow_min),
            glow_max: num(&["motion", "glow_max"], d.glow_max),
            stage_fade_s: num(&["motion", "stage_fade_s"], d.stage_fade_s),
            debug_env: at(&v, &["debug", "env"])
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map_or(d.debug_env, str::to_owned),
            debug_border_frac: num(&["debug", "border_frac"], d.debug_border_frac),
            debug_border_min: int(&["debug", "border_min"], d.debug_border_min),
            debug_margin_frac: num(&["debug", "margin_frac"], d.debug_margin_frac),
            debug_pt_frac: num(&["debug", "pt_frac"], d.debug_pt_frac),
            splash_debug: color(&["stages", "greeter-splash", "debug"], d.splash_debug),
            greeter_debug: color(&["stages", "greeter", "debug"], d.greeter_debug),
        };
        Ok(scene)
    }

    /// The short side sizes are fractions of: the fixed reference when the
    /// spec has one, else this screen's own.
    fn short_side(&self, w: u32, h: u32) -> f64 {
        f64::from(self.ref_short_side.unwrap_or_else(|| w.min(h)))
    }

    /// Geometry of the mark on a `w` x `h` screen (physical pixels, upright).
    /// Sizes are fractions of the short side so a portrait screen gets the
    /// same apparent size as a landscape one. With a fixed reference the
    /// centre sits at H/2 + (centre_frac - 0.5) * ref, so the mark is the
    /// same size and the same distance from the middle on every head.
    pub fn geometry(&self, w: u32, h: u32) -> Geometry {
        let s = self.short_side(w, h);
        let logo = rnd(s * self.logo_frac).max(1) as u32;
        let glow = logo * self.glow_mult;
        let cx = f64::from(w) / 2.0;
        let cy = match self.ref_short_side {
            Some(_) => f64::from(h) / 2.0 + (self.centre_frac - 0.5) * s,
            None => f64::from(h) * self.centre_frac,
        };
        Geometry {
            logo,
            glow,
            logo_x: rnd(cx - f64::from(logo) / 2.0),
            logo_y: rnd(cy - f64::from(logo) / 2.0),
            glow_x: rnd(cx - f64::from(glow) / 2.0),
            glow_y: rnd(cy - f64::from(glow) / 2.0),
        }
    }

    /// Sizes of a debug tag on a `w` x `h` screen.
    pub fn tag_geometry(&self, w: u32, h: u32) -> TagGeometry {
        let s = self.short_side(w, h);
        TagGeometry {
            border: (rnd(s * self.debug_border_frac).max(0) as u32).max(self.debug_border_min),
            margin: rnd(s * self.debug_margin_frac).max(0) as u32,
            pt: (rnd(s * self.debug_pt_frac).max(0) as u32).max(10),
        }
    }

    /// Whether the scene's debug environment variable asks for stage tags.
    pub fn debug_from_env(&self) -> bool {
        std::env::var(&self.debug_env)
            .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "yes" | "true" | "on"))
    }
}

/// Smoothstep on 0..1, clamped: the one easing curve every drawer uses.
pub fn ease(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

impl Scene {
    /// How far the glow is eased in, 0..1. `t` is seconds since the stage
    /// began animating, `t_out` the second it began easing out for a hand-off.
    pub fn motion_level(&self, t: f64, t_out: Option<f64>) -> f64 {
        let mut level = ease(t / self.in_s.max(f64::EPSILON));
        if let Some(t_out) = t_out {
            level = level.min(1.0 - ease((t - t_out) / self.out_s.max(f64::EPSILON)));
        }
        level
    }

    /// Breathing curve, 0..1, starting at its bottom.
    pub fn breath(&self, phase_s: f64) -> f64 {
        (1.0 - (std::f64::consts::TAU * phase_s / self.breath_period_s).cos()) / 2.0
    }

    /// Glow opacity at stage time `t`. `clock` (CLOCK_MONOTONIC seconds)
    /// sets the breathing phase, so two drawers agree on it.
    pub fn glow_level(&self, t: f64, t_out: Option<f64>, clock: f64) -> f64 {
        self.motion_level(t, t_out)
            * (self.glow_min + (self.glow_max - self.glow_min) * self.breath(clock))
    }
}

/// CLOCK_MONOTONIC in seconds, the breathing phase's shared source.
pub fn monotonic_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec we hand it.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    // Values printed by the boot-splash spec's geometry(W, H).
    #[test]
    fn geometry_follows_the_short_side() {
        let s = Scene::default();
        let g = s.geometry(3440, 1440);
        assert_eq!((g.logo, g.glow, g.logo_x, g.logo_y), (230, 460, 1605, 562));
        // A portrait screen gets the same apparent size from its short side.
        let g = s.geometry(2160, 3840);
        assert_eq!((g.logo, g.glow, g.logo_x, g.logo_y), (346, 692, 907, 1632));
        let g = s.geometry(1920, 1080);
        assert_eq!((g.logo, g.glow, g.logo_x, g.logo_y), (173, 346, 874, 421));
    }

    #[test]
    fn fixed_reference_gives_one_size_and_a_fixed_offset() {
        let s = Scene::parse(r#"{"version":2,"ref_short_side":1440}"#).unwrap();
        assert_eq!(s.ref_short_side, Some(1440));
        let wide = s.geometry(3440, 1440);
        let hd = s.geometry(1920, 1080);
        let tall = s.geometry(2160, 3840);
        assert_eq!((wide.logo, hd.logo, tall.logo), (230, 230, 230));
        // Centre = H/2 - 0.03 * 1440 = H/2 - 43.2, then rounded half up.
        assert_eq!(wide.logo_y, 562);
        assert_eq!(hd.logo_y, 540 - 43 - 115);
        assert_eq!(tall.logo_y, 1920 - 43 - 115);
        assert_eq!(s.tag_geometry(1920, 1080), s.tag_geometry(3440, 1440));
        // Null or zero means per-screen sizing, as before.
        for text in [
            r#"{"version":2,"ref_short_side":null}"#,
            r#"{"version":2,"ref_short_side":0}"#,
        ] {
            assert_eq!(Scene::parse(text).unwrap().ref_short_side, None);
        }
    }

    #[test]
    fn json_export_round_trips_to_the_defaults() {
        let text = include_str!("../../../tests/fixtures/scene.json");
        assert_eq!(Scene::parse(text).unwrap(), Scene::default());
    }

    #[test]
    fn other_versions_and_junk_are_refused() {
        assert!(Scene::parse("{\"version\": 3}").is_err());
        assert!(Scene::parse("nope").is_err());
        assert_eq!(
            Scene::load(Path::new("/nonexistent/scene.json")),
            Scene::default()
        );
    }

    #[test]
    fn partial_spec_keeps_the_rest_of_the_defaults() {
        let s =
            Scene::parse(r##"{"version":2,"palette":{"bg":"#102030","accent":"nope"}}"##).unwrap();
        assert_eq!(s.bg, [0x10, 0x20, 0x30]);
        assert_eq!(s.accent, Scene::default().accent);
    }

    #[test]
    fn motion_eases_in_and_out_and_breathes() {
        let s = Scene::default();
        assert_eq!(s.motion_level(0.0, None), 0.0);
        assert_eq!(s.motion_level(s.in_s, None), 1.0);
        assert!((s.motion_level(s.in_s / 2.0, None) - 0.5).abs() < 1e-9);
        // Easing out from fully in lands back on zero after out_s.
        assert_eq!(s.motion_level(5.0 + s.out_s, Some(5.0)), 0.0);
        assert!(s.motion_level(5.0 + s.out_s / 2.0, Some(5.0)) < 1.0);
        assert!((s.breath(0.0)).abs() < 1e-9);
        assert!((s.breath(s.breath_period_s / 2.0) - 1.0).abs() < 1e-9);
        assert!((s.glow_level(1.0, None, s.breath_period_s / 2.0) - s.glow_max).abs() < 1e-9);
        assert!((s.glow_level(1.0, None, 0.0) - s.glow_min).abs() < 1e-9);
    }

    #[test]
    fn tag_sizes_have_floors() {
        let t = Scene::default().tag_geometry(300, 200);
        assert_eq!((t.border, t.pt), (4, 10));
    }
}
