use std::time::{Duration, Instant};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use vigil_config::Splash as SplashConfig;
use vigil_core::{FrameTarget, scene_to_panel};

use vigil_scene::{Logo, Plate};

use crate::scene::{Scene, ease};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    GreeterSplash,
    Greeter,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::GreeterSplash => "GREETER-SPLASH",
            Stage::Greeter => "GREETER",
        }
    }
}

pub fn parse_color(text: &str, fallback: [u8; 3]) -> [u8; 3] {
    crate::scene::parse_color(text).unwrap_or(fallback)
}

pub fn resolve_scene(cfg: &SplashConfig) -> Scene {
    let mut scene = Scene::load(&cfg.scene);
    for (over, slot) in [
        (&cfg.ground, &mut scene.bg),
        (&cfg.mark, &mut scene.mark),
        (&cfg.accent, &mut scene.accent),
    ] {
        if let Some(text) = over {
            *slot = parse_color(text, *slot);
        }
    }
    scene
}

pub struct Splash {
    plate: Plate,
    rest: Vec<u8>,
}

fn put(buf: &mut [u8], stride: usize, x: usize, y: usize, rgb: [u8; 3]) {
    let o = y * stride + x * 4;
    buf[o] = rgb[2];
    buf[o + 1] = rgb[1];
    buf[o + 2] = rgb[0];
    buf[o + 3] = 0xff;
}

impl Splash {
    pub fn new(cfg: &SplashConfig, spec: &Scene, scene: (u32, u32), transform: u8) -> Self {
        let plate = cfg
            .logo
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .and_then(|path| {
                Logo::load(path)
                    .and_then(|logo| Plate::new(spec, &logo, scene, transform))
                    .inspect_err(|e| eprintln!("vigil: splash logo {}: {e}", path.display()))
                    .ok()
            })
            .unwrap_or_else(|| Plate::ground_only(spec, scene, transform));
        let rest = plate.frame(0.0);
        Self { plate, rest }
    }

    fn fits(&self, target: &FrameTarget<'_>) -> bool {
        self.plate.fits(
            target.width,
            target.height,
            target.stride,
            target.buffer.len(),
        )
    }

    pub fn draw_full(&self, target: &mut FrameTarget<'_>, level: f64) -> bool {
        if !self.fits(target) {
            return false;
        }
        let row = self.plate.panel().0 as usize * 4;
        for y in 0..self.plate.panel().1 as usize {
            target.buffer[y * target.stride..y * target.stride + row]
                .copy_from_slice(&self.rest[y * row..(y + 1) * row]);
        }
        if level > 0.0 {
            self.plate.draw_rect(target.buffer, target.stride, level);
        }
        true
    }

    pub fn draw_glow(&self, target: &mut FrameTarget<'_>, level: f64) -> bool {
        if !self.fits(target) {
            return false;
        }
        self.plate.draw_rect(target.buffer, target.stride, level);
        true
    }

    pub fn blend_over(&self, target: &mut FrameTarget<'_>, alpha: f32) {
        if !self.fits(target) || alpha <= 0.0 {
            return;
        }
        let a = (alpha.min(1.0) * 256.0) as u32;
        let row = self.plate.panel().0 as usize * 4;
        for y in 0..self.plate.panel().1 as usize {
            let dst = &mut target.buffer[y * target.stride..y * target.stride + row];
            let src = &self.rest[y * row..(y + 1) * row];
            for (d, s) in dst.iter_mut().zip(src) {
                *d = ((u32::from(*s) * a + u32::from(*d) * (256 - a)) >> 8) as u8;
            }
        }
    }
}

pub fn copy_out(src: &[u8], dst: &mut FrameTarget<'_>) {
    let n = (dst.stride * dst.height as usize).min(src.len());
    dst.buffer[..n].copy_from_slice(&src[..n]);
}

pub struct Tag {
    panel: (u32, u32),
    border: usize,
    splash_rgb: [u8; 3],
    greeter_rgb: [u8; 3],
    splash_label: Vec<(u32, u32)>,
    greeter_label: Vec<(u32, u32)>,
}

fn glyph(c: char) -> [u8; 8] {
    if c == '\u{b7}' {
        return [0, 0, 0, 0x18, 0x18, 0, 0, 0];
    }
    BASIC_FONTS.get(c).unwrap_or([0; 8])
}

impl Tag {
    pub fn new(spec: &Scene, connector: &str, scene: (u32, u32), transform: u8) -> Self {
        let transform = transform % 4;
        let (sw, sh) = (scene.0 as usize, scene.1 as usize);
        let panel = if transform % 2 == 1 {
            (scene.1, scene.0)
        } else {
            scene
        };
        let t = spec.tag_geometry(scene.0, scene.1);
        let scale = (t.pt as usize / 8).max(1);
        let label = |stage: Stage| -> Vec<(u32, u32)> {
            let text = format!(
                "{} \u{b7} {connector} \u{b7} {}x{}",
                stage.name(),
                scene.0,
                scene.1
            );
            let origin = (t.border + t.margin) as usize;
            let mut px = Vec::new();
            for (n, c) in text.chars().enumerate() {
                for (gy, bits) in glyph(c).into_iter().enumerate() {
                    for gx in 0..8usize {
                        if bits >> gx & 1 == 0 {
                            continue;
                        }
                        for dy in 0..scale {
                            for dx in 0..scale {
                                let x = origin + (n * 8 + gx) * scale + dx;
                                let y = origin + gy * scale + dy;
                                if x < sw && y < sh {
                                    let (px_, py_) = scene_to_panel(transform, sw, sh, x, y);
                                    px.push((px_ as u32, py_ as u32));
                                }
                            }
                        }
                    }
                }
            }
            px
        };
        Self {
            panel,
            border: t.border as usize,
            splash_rgb: spec.splash_debug,
            greeter_rgb: spec.greeter_debug,
            splash_label: label(Stage::GreeterSplash),
            greeter_label: label(Stage::Greeter),
        }
    }

    pub fn draw(&self, target: &mut FrameTarget<'_>, stage: Stage) {
        if target.width != self.panel.0 || target.height != self.panel.1 {
            return;
        }
        let (rgb, label) = match stage {
            Stage::GreeterSplash => (self.splash_rgb, &self.splash_label),
            Stage::Greeter => (self.greeter_rgb, &self.greeter_label),
        };
        let (w, h, b) = (target.width as usize, target.height as usize, self.border);
        let stride = target.stride;
        for y in 0..h {
            let full = y < b || y + b >= h;
            let span = if full { 0..w } else { 0..b.min(w) };
            for x in span {
                put(target.buffer, stride, x, y, rgb);
            }
            if !full {
                for x in w.saturating_sub(b)..w {
                    put(target.buffer, stride, x, y, rgb);
                }
            }
        }
        for &(x, y) in label {
            put(target.buffer, stride, x as usize, y as usize, rgb);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Glow(f64),
    Fade(f32),
    Done,
}

pub struct Timeline {
    started: Instant,
    anim_start: Option<Instant>,
    ready_at: Option<Instant>,
    max_wait: Duration,
}

impl Timeline {
    pub fn new(started: Instant, cfg: &SplashConfig) -> Self {
        Self {
            started,
            anim_start: None,
            ready_at: None,
            max_wait: Duration::from_millis(cfg.max_wait_ms),
        }
    }

    pub fn mark_ready(&mut self, now: Instant) {
        self.ready_at.get_or_insert(now);
    }

    pub fn is_ready(&self) -> bool {
        self.ready_at.is_some()
    }

    pub fn phase(&mut self, now: Instant, clock: f64, spec: &Scene) -> Phase {
        if self.ready_at.is_none() && now.duration_since(self.started) >= self.max_wait {
            self.ready_at = Some(now);
        }
        let start = *self.anim_start.get_or_insert(now);
        let t = now.saturating_duration_since(start).as_secs_f64();
        let Some(ready) = self.ready_at else {
            return Phase::Glow(spec.glow_level(t, None, clock));
        };
        let t_out = ready.saturating_duration_since(start).as_secs_f64();
        let t_fade = t_out + spec.out_s;
        if t < t_fade {
            return Phase::Glow(spec.glow_level(t, Some(t_out), clock));
        }
        let fade = spec.stage_fade_s;
        if fade <= 0.0 || t >= t_fade + fade {
            Phase::Done
        } else {
            Phase::Fade(1.0 - ease((t - t_fade) / fade) as f32)
        }
    }

    pub fn easing_out(&self, now: Instant, spec: &Scene) -> bool {
        match (self.anim_start, self.ready_at) {
            (Some(start), Some(ready)) => {
                let t = now.saturating_duration_since(start).as_secs_f64();
                let t_out = ready.saturating_duration_since(start).as_secs_f64();
                t < t_out + spec.out_s
            }
            _ => false,
        }
    }
}

pub struct Cover {
    slots_filled: u8,
    last_draw: Option<Instant>,
    ready: bool,
    last_level: Option<f64>,
    final_shown: bool,
}

pub const FRAME_EVERY: Duration = Duration::from_millis(16);

impl Cover {
    pub fn new() -> Self {
        Self {
            slots_filled: 0,
            last_draw: None,
            ready: false,
            last_level: None,
            final_shown: false,
        }
    }

    pub fn mark_ready(&mut self) {
        self.ready = true;
    }

    pub fn final_shown(&self) -> bool {
        self.final_shown
    }

    pub fn set_final_shown(&mut self) {
        self.final_shown = true;
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn shown(&self) -> bool {
        self.last_draw.is_some()
    }

    pub fn due(&self, now: Instant) -> bool {
        self.last_draw
            .is_none_or(|t| now.duration_since(t) >= FRAME_EVERY)
    }

    pub fn needs_full(&self) -> bool {
        self.slots_filled < 2
    }

    pub fn presented(&mut self, now: Instant, level: f64) {
        self.slots_filled = (self.slots_filled + 1).min(2);
        self.last_draw = Some(now);
        self.last_level = Some(level);
    }

    pub fn reset_slots(&mut self) {
        self.slots_filled = 0;
        self.last_level = None;
    }

    pub fn changes(&self, level: f64) -> bool {
        self.needs_full() || self.last_level != Some(level)
    }

    pub fn next_in(&self, now: Instant) -> Duration {
        self.last_draw.map_or(Duration::ZERO, |t| {
            FRAME_EVERY.saturating_sub(now.duration_since(t))
        })
    }
}

impl Default for Cover {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(buf: &mut Vec<u8>, w: u32, h: u32) -> FrameTarget<'_> {
        buf.resize(w as usize * h as usize * 4, 0);
        FrameTarget {
            buffer: buf.as_mut_slice(),
            width: w,
            height: h,
            stride: w as usize * 4,
            damage: None,
        }
    }

    fn px(t: &FrameTarget<'_>, x: usize, y: usize) -> [u8; 3] {
        let o = y * t.stride + x * 4;
        [t.buffer[o + 2], t.buffer[o + 1], t.buffer[o]]
    }

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
        <rect x="10" y="10" width="80" height="80" fill="#1793d1"/>
        <rect x="0" y="0" width="2" height="2" fill="#1793d1"/></svg>"##;

    const BOX_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
        <rect x="-10" y="-10" width="120" height="120"/></svg>"##;

    const PLACED: [(u32, u32, u32, usize, usize, usize); 10] = [
        (1440, 1920, 1080, 230, 845, 382),
        (1440, 2560, 1440, 230, 1165, 562),
        (1440, 3440, 1440, 230, 1605, 562),
        (1440, 3840, 2160, 230, 1805, 922),
        (1440, 2160, 3840, 230, 965, 1762),
        (0, 1920, 1080, 173, 874, 421),
        (0, 2560, 1440, 230, 1165, 562),
        (0, 3440, 1440, 230, 1605, 562),
        (0, 3840, 2160, 346, 1747, 842),
        (0, 2160, 3840, 346, 907, 1632),
    ];

    fn cfg_with_svg(name: &str, svg: &str) -> SplashConfig {
        let dir = std::env::temp_dir().join(format!("vigil-splash-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logo.svg");
        std::fs::write(&path, svg).unwrap();
        SplashConfig {
            logo: Some(path),
            ..SplashConfig::default()
        }
    }

    fn cfg_with_logo(name: &str) -> SplashConfig {
        cfg_with_svg(name, SVG)
    }

    #[test]
    fn mark_fills_the_rect_the_boot_scene_places_it_at() {
        let cfg = cfg_with_svg("placed", BOX_SVG);
        for (reference, w, h, logo, x, y) in PLACED {
            let spec = Scene {
                ref_short_side: (reference > 0).then_some(reference),
                ..Scene::default()
            };
            let transform = u8::from(h > w);
            let (sw, sh) = (w as usize, h as usize);
            let a = scene_to_panel(transform, sw, sh, x, y);
            let b = scene_to_panel(transform, sw, sh, x + logo - 1, y + logo - 1);
            let want = (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));

            let (pw, ph) = if transform == 1 { (h, w) } else { (w, h) };
            let s = Splash::new(&cfg, &spec, (w, h), transform);
            let mut buf = Vec::new();
            let mut t = target(&mut buf, pw, ph);
            assert!(s.draw_full(&mut t, 0.0));
            let mark = [spec.mark[2], spec.mark[1], spec.mark[0], 0xff];
            let mut got = (usize::MAX, usize::MAX, 0, 0);
            let (pixels, _partial) = t.buffer.as_chunks::<4>();
            for (i, _) in pixels.iter().enumerate().filter(|(_, p)| **p == mark) {
                let (px, py) = (i % pw as usize, i / pw as usize);
                got = (got.0.min(px), got.1.min(py), got.2.max(px), got.3.max(py));
            }
            assert_eq!(got, want, "{w}x{h} reference {reference}");
        }
    }

    #[test]
    fn rest_frame_is_ground_and_mark_only() {
        let spec = Scene::default();
        let cfg = cfg_with_logo("rest");
        let s = Splash::new(&cfg, &spec, (800, 500), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 800, 500);
        assert!(s.draw_full(&mut t, 0.0));
        assert_eq!(px(&t, 0, 0), spec.bg);
        let g = spec.geometry(800, 500);
        let (cx, cy) = (400, (g.logo_y + i64::from(g.logo) / 2) as usize);
        assert_eq!(px(&t, cx, cy), spec.mark);
        assert_eq!(px(&t, cx, g.logo_y as usize - 3), spec.bg);
        assert_ne!(px(&t, g.logo_x as usize, g.logo_y as usize), spec.mark);
    }

    #[test]
    fn glow_tints_the_ground_under_the_mark_and_scales_with_level() {
        let spec = Scene::default();
        let cfg = cfg_with_logo("glow");
        let s = Splash::new(&cfg, &spec, (800, 500), 0);
        let g = spec.geometry(800, 500);
        let at = |level: f64| {
            let mut buf = Vec::new();
            let mut t = target(&mut buf, 800, 500);
            s.draw_full(&mut t, level);
            (
                px(&t, 400, g.logo_y as usize - 3),
                px(&t, 400, g.logo_y as usize + 40),
            )
        };
        let (lo, mark_lo) = at(0.1);
        let (hi, mark_hi) = at(0.4);
        assert!(lo[2] > spec.bg[2] && hi[2] > lo[2], "{lo:?} {hi:?}");
        assert_eq!(mark_lo, spec.mark, "the mark stays over the glow");
        assert_eq!(mark_hi, spec.mark);
        let mut a = Vec::new();
        let mut b = Vec::new();
        let mut ta = target(&mut a, 800, 500);
        let mut tb = target(&mut b, 800, 500);
        s.draw_full(&mut ta, 0.0);
        s.draw_glow(&mut ta, 0.3);
        s.draw_full(&mut tb, 0.3);
        assert_eq!(ta.buffer, tb.buffer);
    }

    #[test]
    fn rotated_panel_gets_the_same_picture_turned() {
        let spec = Scene::default();
        let cfg = cfg_with_logo("rot");
        let s = Splash::new(&cfg, &spec, (400, 800), 3);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 800, 400);
        assert!(s.draw_full(&mut t, 0.2));
        let g = spec.geometry(400, 800);
        let (sx, sy) = (200usize, (g.logo_y + i64::from(g.logo) / 2) as usize);
        let (px_, py_) = scene_to_panel(3, 400, 800, sx, sy);
        assert_eq!(px(&t, px_, py_), spec.mark);
    }

    #[test]
    fn no_logo_is_just_the_ground() {
        let spec = Scene::default();
        let s = Splash::new(&SplashConfig::default(), &spec, (200, 100), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 200, 100);
        assert!(s.draw_full(&mut t, 0.4));
        assert_eq!(px(&t, 100, 47), spec.bg);
        let mut small = Vec::new();
        assert!(!s.draw_full(&mut target(&mut small, 10, 10), 0.0));
    }

    #[test]
    fn blend_moves_from_target_to_rest() {
        let spec = Scene::default();
        let s = Splash::new(&SplashConfig::default(), &spec, (4, 4), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 4, 4);
        t.buffer.fill(200);
        s.blend_over(&mut t, 0.0);
        assert_eq!(px(&t, 0, 0), [200, 200, 200]);
        s.blend_over(&mut t, 1.0);
        let p = px(&t, 0, 0);
        assert!(p[0] <= 0x0c && p[2] <= 0x11, "{p:?}");
    }

    #[test]
    fn tag_draws_border_and_label_in_the_stage_colour() {
        let spec = Scene::default();
        let tag = Tag::new(&spec, "Virtual-1", (640, 360), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 640, 360);
        tag.draw(&mut t, Stage::GreeterSplash);
        assert_eq!(px(&t, 0, 180), spec.splash_debug);
        assert_eq!(px(&t, 639, 180), spec.splash_debug);
        assert_eq!(px(&t, 320, 0), spec.splash_debug);
        let b = spec.tag_geometry(640, 360);
        let lit = (0..120)
            .flat_map(|y| (0..320).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                x > b.border as usize && y > b.border as usize && px(&t, x, y) == spec.splash_debug
            })
            .count();
        assert!(lit > 50, "label pixels: {lit}");
        tag.draw(&mut t, Stage::Greeter);
        assert_eq!(px(&t, 0, 180), spec.greeter_debug);
    }

    #[test]
    fn timeline_glows_then_eases_out_then_fades_once() {
        let spec = Scene::default();
        let cfg = SplashConfig::default();
        let t0 = Instant::now();
        let mut tl = Timeline::new(t0, &cfg);
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(tl.phase(at(0), 0.0, &spec), Phase::Glow(0.0));
        match tl.phase(at(2000), 0.0, &spec) {
            Phase::Glow(l) => assert!((l - spec.glow_min).abs() < 1e-9),
            p => panic!("{p:?}"),
        }
        tl.mark_ready(at(2000));
        match tl.phase(at(2000 + 150), 0.0, &spec) {
            Phase::Glow(l) => assert!(l < spec.glow_min),
            p => panic!("{p:?}"),
        }
        assert!(tl.easing_out(at(2100), &spec));
        match tl.phase(at(2000 + 300 + 200), 0.0, &spec) {
            Phase::Fade(a) => assert!((a - 0.5).abs() < 0.01, "{a}"),
            p => panic!("{p:?}"),
        }
        assert_eq!(tl.phase(at(2000 + 300 + 400), 0.0, &spec), Phase::Done);
    }

    #[test]
    fn timeline_gives_up_waiting_after_max_wait() {
        let spec = Scene::default();
        let cfg = SplashConfig::default();
        let t0 = Instant::now();
        let mut tl = Timeline::new(t0, &cfg);
        let late = t0 + Duration::from_millis(cfg.max_wait_ms + 1);
        tl.phase(late, 0.0, &spec);
        assert!(tl.is_ready());
    }

    #[test]
    fn cover_redraws_only_the_glow_once_both_slots_are_filled() {
        let t0 = Instant::now();
        let mut c = Cover::new();
        assert!(c.needs_full());
        c.presented(t0, 0.0);
        assert!(c.needs_full());
        c.presented(t0, 0.1);
        assert!(!c.needs_full());
        assert!(!c.changes(0.1));
        assert!(c.changes(0.2));
        c.reset_slots();
        assert!(c.needs_full());
    }

    #[test]
    fn parses_colors_and_keeps_fallback() {
        assert_eq!(parse_color("#0b0d10", [1, 2, 3]), [0x0b, 0x0d, 0x10]);
        assert_eq!(parse_color("nope", [1, 2, 3]), [1, 2, 3]);
    }
}

#[cfg(test)]
mod goldens {
    use super::*;

    fn case(name: &str) -> Option<(u32, u32, u8, f64, u32)> {
        let rest = name.strip_prefix("splash-")?.strip_suffix(".png")?;
        let mut it = rest.split('-');
        let (w, h) = it.next()?.split_once('x')?;
        let t = it.next()?.strip_prefix('t')?.parse().ok()?;
        let l: u32 = it.next()?.strip_prefix('l')?.parse().ok()?;
        let r = it.next()?.strip_prefix('r')?.parse().ok()?;
        Some((w.parse().ok()?, h.parse().ok()?, t, f64::from(l) / 100.0, r))
    }

    #[test]
    fn splash_draws_every_scene_golden_exactly() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cfg = SplashConfig {
            logo: Some(root.join("tests/fixtures/mark.svg")),
            ..SplashConfig::default()
        };
        let mut checked = 0;
        for entry in std::fs::read_dir(root.join("tests/golden/scene")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let Some((pw, ph, transform, level, reference)) = case(&name) else {
                continue;
            };
            let spec = Scene {
                ref_short_side: (reference > 0).then_some(reference),
                ..Scene::default()
            };
            let scene = if transform % 2 == 1 {
                (ph, pw)
            } else {
                (pw, ph)
            };
            let s = Splash::new(&cfg, &spec, scene, transform);
            let mut buf = vec![0u8; (pw * ph * 4) as usize];
            let mut t = FrameTarget {
                buffer: &mut buf,
                width: pw,
                height: ph,
                stride: pw as usize * 4,
                damage: None,
            };
            assert!(s.draw_full(&mut t, level));
            let got: Vec<u8> = buf.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let want = image::open(&path).unwrap().into_rgb8().into_raw();
            let off = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            assert_eq!(off, 0, "{name}: {off} channel values differ");
            checked += 1;
        }
        assert!(checked >= 7, "only {checked} goldens found");
    }
}
