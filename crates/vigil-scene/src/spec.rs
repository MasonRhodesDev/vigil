use std::path::Path;

use serde_json::Value;

use crate::Error;

pub const DEFAULT_SPEC: &str = include_str!("../../../tests/fixtures/scene.json");

pub(crate) fn rnd(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

fn size(x: i64) -> u32 {
    x.clamp(0, i64::from(u32::MAX)) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub logo: u32,
    pub glow: u32,
    pub logo_x: i64,
    pub logo_y: i64,
    pub glow_x: i64,
    pub glow_y: i64,
    pub bar_w: u32,
    pub bar_h: u32,
    pub bar_x: i64,
    pub bar_y: i64,
    pub gap: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagGeometry {
    pub border: u32,
    pub margin: u32,
    pub pt: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub ref_short_side: Option<u32>,
    pub bg: [u8; 3],
    pub accent: [u8; 3],
    pub mark: [u8; 3],
    pub track: [u8; 3],
    pub logo_frac: f64,
    pub glow_mult: u32,
    pub glow_blur_div: u32,
    pub centre_frac: f64,
    pub gap_frac: f64,
    pub bar_w_frac: f64,
    pub bar_h_frac: f64,
    pub bar_h_min: u32,
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
            track: [0x1f, 0x28, 0x32],
            logo_frac: 0.16,
            glow_mult: 2,
            glow_blur_div: 6,
            centre_frac: 0.47,
            gap_frac: 0.05,
            bar_w_frac: 0.26,
            bar_h_frac: 0.0028,
            bar_h_min: 2,
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
        Scene::from_value(&v)
    }

    fn from_value(v: &Value) -> Result<Scene, String> {
        match at(v, &["version"]).and_then(Value::as_u64) {
            Some(2) => {}
            other => return Err(format!("unsupported version {other:?}")),
        }
        let d = Scene::default();
        let num = |path: &[&str], default: f64| {
            at(v, path)
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite())
                .unwrap_or(default)
        };
        let int = |path: &[&str], default: u32| {
            at(v, path)
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .unwrap_or(default)
        };
        let color = |path: &[&str], default: [u8; 3]| {
            at(v, path)
                .and_then(Value::as_str)
                .and_then(parse_color)
                .unwrap_or(default)
        };
        Ok(Scene {
            ref_short_side: at(v, &["ref_short_side"])
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok())
                .filter(|x| *x > 0),
            bg: color(&["palette", "bg"], d.bg),
            accent: color(&["palette", "accent"], d.accent),
            mark: color(&["palette", "mark"], d.mark),
            track: color(&["palette", "track"], d.track),
            logo_frac: num(&["geometry", "logo_frac"], d.logo_frac),
            glow_mult: int(&["geometry", "glow_mult"], d.glow_mult).max(1),
            glow_blur_div: int(&["geometry", "glow_blur_div"], d.glow_blur_div).max(1),
            centre_frac: num(&["geometry", "centre_frac_of_h"], d.centre_frac),
            gap_frac: num(&["geometry", "gap_frac"], d.gap_frac),
            bar_w_frac: num(&["geometry", "bar_w_frac"], d.bar_w_frac),
            bar_h_frac: num(&["geometry", "bar_h_frac"], d.bar_h_frac),
            bar_h_min: int(&["geometry", "bar_h_min"], d.bar_h_min),
            in_s: num(&["motion", "in_s"], d.in_s),
            out_s: num(&["motion", "out_s"], d.out_s),
            breath_period_s: num(&["motion", "breath_period_s"], d.breath_period_s).max(0.001),
            glow_min: num(&["motion", "glow_min"], d.glow_min),
            glow_max: num(&["motion", "glow_max"], d.glow_max),
            stage_fade_s: num(&["motion", "stage_fade_s"], d.stage_fade_s),
            debug_env: at(v, &["debug", "env"])
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map_or(d.debug_env, str::to_owned),
            debug_border_frac: num(&["debug", "border_frac"], d.debug_border_frac),
            debug_border_min: int(&["debug", "border_min"], d.debug_border_min),
            debug_margin_frac: num(&["debug", "margin_frac"], d.debug_margin_frac),
            debug_pt_frac: num(&["debug", "pt_frac"], d.debug_pt_frac),
            splash_debug: color(&["stages", "greeter-splash", "debug"], d.splash_debug),
            greeter_debug: color(&["stages", "greeter", "debug"], d.greeter_debug),
        })
    }

    fn short_side(&self, w: u32, h: u32) -> f64 {
        f64::from(self.ref_short_side.unwrap_or_else(|| w.min(h)))
    }

    pub fn geometry(&self, w: u32, h: u32) -> Geometry {
        let s = self.short_side(w, h);
        let logo = size(rnd(s * self.logo_frac)).max(1);
        let glow = logo.saturating_mul(self.glow_mult);
        let cx = f64::from(w) / 2.0;
        let basis = match self.ref_short_side {
            Some(_) => s,
            None => f64::from(h),
        };
        let cy = f64::from(h) / 2.0 + (self.centre_frac - 0.5) * basis;
        let bar_w = size(rnd(s * self.bar_w_frac));
        let gap = size(rnd(s * self.gap_frac));
        Geometry {
            logo,
            glow,
            logo_x: rnd(cx - f64::from(logo) / 2.0),
            logo_y: rnd(cy - f64::from(logo) / 2.0),
            glow_x: rnd(cx - f64::from(glow) / 2.0),
            glow_y: rnd(cy - f64::from(glow) / 2.0),
            bar_w,
            bar_h: size(rnd(s * self.bar_h_frac)).max(self.bar_h_min),
            bar_x: rnd(cx - f64::from(bar_w) / 2.0),
            bar_y: rnd(cy + f64::from(logo) / 2.0 + f64::from(gap)),
            gap,
        }
    }

    pub fn tag_geometry(&self, w: u32, h: u32) -> TagGeometry {
        let s = self.short_side(w, h);
        TagGeometry {
            border: size(rnd(s * self.debug_border_frac)).max(self.debug_border_min),
            margin: size(rnd(s * self.debug_margin_frac)),
            pt: size(rnd(s * self.debug_pt_frac)).max(10),
        }
    }

    pub fn debug_from_env(&self) -> bool {
        std::env::var(&self.debug_env)
            .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "yes" | "true" | "on"))
    }

    pub fn motion_level(&self, t: f64, t_out: Option<f64>) -> f64 {
        let mut level = ease(t / self.in_s.max(f64::EPSILON));
        if let Some(t_out) = t_out {
            level = level.min(1.0 - ease((t - t_out) / self.out_s.max(f64::EPSILON)));
        }
        level
    }

    pub fn breath(&self, phase_s: f64) -> f64 {
        (1.0 - (std::f64::consts::TAU * phase_s / self.breath_period_s).cos()) / 2.0
    }

    pub fn glow_level(&self, t: f64, t_out: Option<f64>, clock: f64) -> f64 {
        self.motion_level(t, t_out)
            * (self.glow_min + (self.glow_max - self.glow_min) * self.breath(clock))
    }
}

pub fn ease(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

pub fn emit_spec(source: Option<&str>, ref_short_side: Option<u32>) -> Result<String, Error> {
    let mut v: Value = serde_json::from_str(source.unwrap_or(DEFAULT_SPEC))
        .map_err(|e| Error::Invalid(format!("scene spec: {e}")))?;
    Scene::from_value(&v).map_err(|e| Error::Invalid(format!("scene spec: {e}")))?;
    if let (Some(r), Some(doc)) = (ref_short_side, v.as_object_mut()) {
        doc.insert(
            "ref_short_side".into(),
            if r == 0 { Value::Null } else { r.into() },
        );
    }
    let mut text = serde_json::to_string_pretty(&v).map_err(|e| Error::Invalid(e.to_string()))?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_document_is_the_defaults() {
        assert_eq!(Scene::parse(DEFAULT_SPEC).unwrap(), Scene::default());
    }

    #[test]
    fn emitting_keeps_every_field_and_only_sets_the_reference() {
        let source = r#"{"version":2,"exit":{"max_wait":9},"debug":{"stage_tags":true}}"#;
        let out: Value = serde_json::from_str(&emit_spec(Some(source), None).unwrap()).unwrap();
        assert_eq!(out, serde_json::from_str::<Value>(source).unwrap());
        let fixed: Value =
            serde_json::from_str(&emit_spec(Some(source), Some(1440)).unwrap()).unwrap();
        assert_eq!(fixed["ref_short_side"], 1440);
        assert_eq!(fixed["exit"]["max_wait"], 9);
        let free: Value = serde_json::from_str(&emit_spec(None, Some(0)).unwrap()).unwrap();
        assert_eq!(
            free,
            serde_json::from_str::<Value>(DEFAULT_SPEC).unwrap(),
            "0 means per screen, which the built-in document already says"
        );
        assert!(emit_spec(Some(r#"{"version":3}"#), None).is_err());
        assert!(emit_spec(Some("[1]"), None).is_err());
    }

    #[test]
    fn absurd_fractions_saturate_instead_of_wrapping() {
        let s = Scene {
            logo_frac: 1e300,
            glow_mult: u32::MAX,
            ..Scene::default()
        };
        let g = s.geometry(100, 100);
        assert_eq!((g.logo, g.glow), (u32::MAX, u32::MAX));
        let s = Scene {
            logo_frac: -5.0,
            ..Scene::default()
        };
        assert_eq!(s.geometry(100, 100).logo, 1);
    }
}
