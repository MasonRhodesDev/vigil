use std::time::{Duration, Instant};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use vigil_config::Splash as SplashConfig;
use vigil_core::{FrameTarget, scene_to_panel};

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

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

pub struct Splash {
    panel: (u32, u32),
    ground: [u8; 3],
    accent: [u8; 3],
    mark: [u8; 3],
    rest: Vec<u8>,
    glow_rect: Rect,
    glow_alpha: Vec<u8>,
    mark_alpha: Vec<u8>,
}

fn put(buf: &mut [u8], stride: usize, x: usize, y: usize, rgb: [u8; 3]) {
    let o = y * stride + x * 4;
    buf[o] = rgb[2];
    buf[o + 1] = rgb[1];
    buf[o + 2] = rgb[0];
    buf[o + 3] = 0xff;
}

fn mask(path: &std::path::Path, edge: u32) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let mut alpha = if is_svg {
        let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default())
            .map_err(|e| e.to_string())?;
        let size = tree.size();
        let s = edge as f32 / size.width().max(size.height());
        let ox = (edge as f32 - size.width() * s) / 2.0;
        let oy = (edge as f32 - size.height() * s) / 2.0;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(edge, edge).ok_or("empty logo box")?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_row(s, 0.0, 0.0, s, ox, oy),
            &mut pixmap.as_mut(),
        );
        let (pixels, _partial) = pixmap.data().as_chunks::<4>();
        pixels.iter().map(|p| p[3]).collect::<Vec<u8>>()
    } else {
        let img = image::load_from_memory(&data)
            .map_err(|e| e.to_string())?
            .into_rgba8();
        let ratio = (f64::from(edge) / f64::from(img.width().max(img.height()))).min(f64::MAX);
        let (w, h) = (
            ((f64::from(img.width()) * ratio).round() as u32).max(1),
            ((f64::from(img.height()) * ratio).round() as u32).max(1),
        );
        let img = if (w, h) == img.dimensions() {
            img
        } else {
            image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3)
        };
        let mut out = vec![0u8; (edge * edge) as usize];
        let (ox, oy) = ((edge - w.min(edge)) / 2, (edge - h.min(edge)) / 2);
        for (x, y, p) in img.enumerate_pixels() {
            if x < edge && y < edge && ox + x < edge && oy + y < edge {
                out[((oy + y) * edge + ox + x) as usize] = p[3];
            }
        }
        out
    };
    drop_specks(&mut alpha, edge as usize, edge as usize, 0.02);
    Ok(alpha)
}

fn drop_specks(alpha: &mut [u8], w: usize, h: usize, keep: f64) {
    let mut seen = vec![false; w * h];
    let mut shapes: Vec<Vec<usize>> = Vec::new();
    for start in 0..w * h {
        if alpha[start] == 0 || seen[start] {
            continue;
        }
        let mut pts = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(i) = stack.pop() {
            pts.push(i);
            let (x, y) = (i % w, i / w);
            let mut visit = |n: usize| {
                if alpha[n] != 0 && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            };
            if x + 1 < w {
                visit(i + 1);
            }
            if x > 0 {
                visit(i - 1);
            }
            if y + 1 < h {
                visit(i + w);
            }
            if y > 0 {
                visit(i - w);
            }
        }
        shapes.push(pts);
    }
    let biggest = shapes.iter().map(Vec::len).max().unwrap_or(0) as f64;
    for shape in shapes.iter().filter(|s| (s.len() as f64) < keep * biggest) {
        for &i in shape {
            alpha[i] = 0;
        }
    }
}

fn blur(src: &[u8], w: usize, h: usize, sigma: f64) -> Vec<u8> {
    if sigma < 0.3 {
        return src.to_vec();
    }
    let radius = (sigma * 3.0).ceil() as i64;
    let mut k: Vec<f64> = (-radius..=radius)
        .map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp())
        .collect();
    let total: f64 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= total);
    let mut tmp = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (j, kv) in k.iter().enumerate() {
                let sx = (x as i64 + j as i64 - radius).clamp(0, w as i64 - 1) as usize;
                acc += kv * f64::from(src[y * w + sx]);
            }
            tmp[y * w + x] = acc as f32;
        }
    }
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (j, kv) in k.iter().enumerate() {
                let sy = (y as i64 + j as i64 - radius).clamp(0, h as i64 - 1) as usize;
                acc += kv * f64::from(tmp[sy * w + x]);
            }
            out[y * w + x] = acc.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn shrink(src: &[u8], n: usize, f: usize) -> (Vec<u8>, usize) {
    let q = n.div_ceil(f);
    let mut out = vec![0u8; q * q];
    for qy in 0..q {
        for qx in 0..q {
            let (mut sum, mut cnt) = (0u32, 0u32);
            for y in qy * f..((qy + 1) * f).min(n) {
                for x in qx * f..((qx + 1) * f).min(n) {
                    sum += u32::from(src[y * n + x]);
                    cnt += 1;
                }
            }
            out[qy * q + qx] = (sum / cnt.max(1)) as u8;
        }
    }
    (out, q)
}

fn enlarge(src: &[u8], q: usize, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n * n];
    let scale = q as f64 / n as f64;
    for y in 0..n {
        let fy = ((y as f64 + 0.5) * scale - 0.5).clamp(0.0, (q - 1) as f64);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(q - 1);
        for x in 0..n {
            let fx = ((x as f64 + 0.5) * scale - 0.5).clamp(0.0, (q - 1) as f64);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(q - 1);
            let at = |xx: usize, yy: usize| f64::from(src[yy * q + xx]);
            let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
            let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
            out[y * n + x] = (top + (bottom - top) * ty).round() as u8;
        }
    }
    out
}

const GLOW_DOWN: usize = 4;

fn glow_mask(logo: &[u8], edge: usize, ge: usize, sigma: f64) -> Vec<u8> {
    let mut big = vec![0u8; ge * ge];
    let off = ge.saturating_sub(edge) / 2;
    for y in 0..edge.min(ge) {
        for x in 0..edge.min(ge) {
            big[(y + off) * ge + x + off] = logo[y * edge + x];
        }
    }
    let (small, q) = shrink(&big, ge, GLOW_DOWN);
    let blurred = blur(&small, q, q, sigma / GLOW_DOWN as f64);
    enlarge(&blurred, q, ge)
}

impl Splash {
    pub fn new(cfg: &SplashConfig, spec: &Scene, scene: (u32, u32), transform: u8) -> Self {
        let transform = transform % 4;
        let (sw, sh) = (scene.0 as usize, scene.1 as usize);
        let (pw, ph) = if transform % 2 == 1 {
            (sh, sw)
        } else {
            (sw, sh)
        };
        let g = spec.geometry(scene.0, scene.1);
        let (edge, ge) = (g.logo as usize, g.glow as usize);

        let logo = match cfg.logo.as_ref().filter(|p| !p.as_os_str().is_empty()) {
            Some(path) => match mask(path, g.logo) {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("vigil: splash logo {}: {e}", path.display());
                    None
                }
            },
            None => None,
        };
        let sigma = f64::from(g.logo / spec.glow_blur_div);
        let glow = logo
            .as_ref()
            .map(|l| glow_mask(l, edge, ge, sigma))
            .unwrap_or_default();

        let (x0, y0) = (g.glow_x.max(0) as usize, g.glow_y.max(0) as usize);
        let x1 = ((g.glow_x + ge as i64).max(0) as usize).min(sw);
        let y1 = ((g.glow_y + ge as i64).max(0) as usize).min(sh);
        let rect = if x1 > x0 && y1 > y0 {
            let a = scene_to_panel(transform, sw, sh, x0, y0);
            let b = scene_to_panel(transform, sw, sh, x1 - 1, y1 - 1);
            Rect {
                x: a.0.min(b.0),
                y: a.1.min(b.1),
                w: a.0.max(b.0) - a.0.min(b.0) + 1,
                h: a.1.max(b.1) - a.1.min(b.1) + 1,
            }
        } else {
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            }
        };
        let mut glow_alpha = vec![0u8; rect.w * rect.h];
        let mut mark_alpha = vec![0u8; rect.w * rect.h];
        if let Some(logo) = &logo {
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let (bx, by) = (sx as i64 - g.glow_x, sy as i64 - g.glow_y);
                    let (px, py) = scene_to_panel(transform, sw, sh, sx, sy);
                    let i = (py - rect.y) * rect.w + (px - rect.x);
                    glow_alpha[i] = glow[by as usize * ge + bx as usize];
                    let (lx, ly) = (sx as i64 - g.logo_x, sy as i64 - g.logo_y);
                    if (0..edge as i64).contains(&lx) && (0..edge as i64).contains(&ly) {
                        mark_alpha[i] = logo[ly as usize * edge + lx as usize];
                    }
                }
            }
        }

        let mut splash = Self {
            panel: (pw as u32, ph as u32),
            ground: spec.bg,
            accent: spec.accent,
            mark: spec.mark,
            rest: Vec::new(),
            glow_rect: rect,
            glow_alpha,
            mark_alpha,
        };
        let mut rest = vec![0u8; pw * ph * 4];
        let (pixels, _partial) = rest.as_chunks_mut::<4>();
        for px in pixels {
            *px = [spec.bg[2], spec.bg[1], spec.bg[0], 0xff];
        }
        let mut frame = FrameTarget {
            buffer: &mut rest,
            width: pw as u32,
            height: ph as u32,
            stride: pw * 4,
            damage: None,
        };
        splash.draw_glow_rect(&mut frame, 0.0);
        splash.rest = rest;
        splash
    }

    fn fits(&self, target: &FrameTarget<'_>) -> bool {
        target.width == self.panel.0
            && target.height == self.panel.1
            && target.stride >= target.width as usize * 4
            && target.buffer.len() >= target.stride * target.height as usize
    }

    fn draw_glow_rect(&self, target: &mut FrameTarget<'_>, level: f64) {
        let r = self.glow_rect;
        let lvl = (level.clamp(0.0, 1.0) * 256.0).round() as u32;
        let mix =
            |from: u32, to: u32, a: u32| -> u8 { ((from * (255 - a) + to * a + 127) / 255) as u8 };
        for ry in 0..r.h {
            let row = (r.y + ry) * target.stride + r.x * 4;
            let alpha = ry * r.w;
            for rx in 0..r.w {
                let ga = (u32::from(self.glow_alpha[alpha + rx]) * lvl) >> 8;
                let la = u32::from(self.mark_alpha[alpha + rx]);
                let o = row + rx * 4;
                for (c, off) in [(2usize, 0usize), (1, 1), (0, 2)] {
                    let under = mix(u32::from(self.ground[c]), u32::from(self.accent[c]), ga);
                    target.buffer[o + off] = mix(u32::from(under), u32::from(self.mark[c]), la);
                }
                target.buffer[o + 3] = 0xff;
            }
        }
    }

    pub fn draw_full(&self, target: &mut FrameTarget<'_>, level: f64) -> bool {
        if !self.fits(target) {
            return false;
        }
        let row = self.panel.0 as usize * 4;
        for y in 0..self.panel.1 as usize {
            target.buffer[y * target.stride..y * target.stride + row]
                .copy_from_slice(&self.rest[y * row..(y + 1) * row]);
        }
        if level > 0.0 {
            self.draw_glow_rect(target, level);
        }
        true
    }

    pub fn draw_glow(&self, target: &mut FrameTarget<'_>, level: f64) -> bool {
        if !self.fits(target) {
            return false;
        }
        self.draw_glow_rect(target, level);
        true
    }

    pub fn blend_over(&self, target: &mut FrameTarget<'_>, alpha: f32) {
        if !self.fits(target) || alpha <= 0.0 {
            return;
        }
        let a = (alpha.min(1.0) * 256.0) as u32;
        let row = self.panel.0 as usize * 4;
        for y in 0..self.panel.1 as usize {
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
mod bless_pre_extraction {
    use super::*;
    use image::ImageEncoder;

    #[test]
    #[ignore = "writes tests/golden/scene from the splash as it stands"]
    fn bless() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cfg = SplashConfig {
            logo: Some(root.join("tests/fixtures/mark.svg")),
            ..SplashConfig::default()
        };
        let out = root.join("tests/golden/scene");
        std::fs::create_dir_all(&out).unwrap();
        for (pw, ph, transform, level, reference) in [
            (1920u32, 1080u32, 0u8, 0.0f64, 0u32),
            (1920, 1080, 0, 0.42, 0),
            (3840, 2160, 0, 0.42, 0),
            (3840, 2160, 1, 0.42, 0),
            (1366, 768, 0, 0.42, 0),
            (1201, 799, 3, 0.25, 0),
            (1920, 1080, 0, 0.42, 1440),
        ] {
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
            let rgb: Vec<u8> = buf.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let name = format!(
                "splash-{pw}x{ph}-t{transform}-l{:03}-r{reference}.png",
                (level * 100.0).round() as u32
            );
            let file = std::fs::File::create(out.join(name)).unwrap();
            image::codecs::png::PngEncoder::new_with_quality(
                file,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(&rgb, pw, ph, image::ExtendedColorType::Rgb8)
            .unwrap();
        }
    }
}
