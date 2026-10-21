//! The splash: the frame vigil draws the instant it owns a screen, and again
//! the instant a login succeeds, so the greeter reads as one piece with
//! whatever drew the boot before it and whatever draws the desktop after.
//!
//! Every dimension comes from [`vigil_config::Splash`]; nothing here knows a
//! color, size or logo. Drawing is plain CPU writes into the presenter's
//! XRGB8888 target, in the scene's upright orientation mapped to the panel.

use std::time::{Duration, Instant};

use vigil_config::Splash as SplashConfig;
use vigil_core::{FrameTarget, scene_to_panel};

/// Parse `#rrggbb`; anything else keeps `fallback`.
pub fn parse_color(text: &str, fallback: [u8; 3]) -> [u8; 3] {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return fallback;
    }
    let Ok(v) = u32::from_str_radix(hex, 16) else {
        return fallback;
    };
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// Geometry of the loading line in scene pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Track {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// Everything needed to draw one output's splash.
pub struct Splash {
    scene: (u32, u32),
    panel: (u32, u32),
    transform: u8,
    /// Ground, mark and idle line, already in panel orientation, tightly
    /// packed XRGB8888 (little endian: B, G, R, X).
    base: Vec<u8>,
    track: Track,
    line: [u8; 3],
    sweep: [u8; 3],
    sweep_w: f64,
    sweep_ms: u64,
}

/// Pixel geometry of the scene on a `scene` screen, from the fractions in
/// the config. Rounds half up, like the boot splash's own script.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub logo: u32,
    pub glow: u32,
    pub cx: f64,
    pub cy: f64,
    pub track: Track,
    pub shine_w: f64,
}

fn put(buf: &mut [u8], stride: usize, x: usize, y: usize, rgb: [u8; 3]) {
    let o = y * stride + x * 4;
    buf[o] = rgb[2];
    buf[o + 1] = rgb[1];
    buf[o + 2] = rgb[0];
    buf[o + 3] = 0xff;
}

fn rnd(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

pub fn layout(cfg: &SplashConfig, scene: (u32, u32)) -> Geometry {
    let h = f64::from(scene.1);
    let logo = rnd(h * cfg.logo_frac).max(1);
    let glow = ((logo as f64) * cfg.glow_mult).round().max(1.0) as u32;
    let cx = f64::from(scene.0) / 2.0;
    let cy = h * cfg.center_frac;
    let track_w = rnd(h * cfg.track_w_frac).max(1);
    let track_h = rnd(h * cfg.track_h_frac)
        .max(i64::from(cfg.track_h_min))
        .max(1);
    let gap = rnd(h * cfg.gap_frac);
    Geometry {
        logo: logo as u32,
        glow,
        cx,
        cy,
        track: Track {
            x: rnd(cx - track_w as f64 / 2.0),
            y: rnd(cy + logo as f64 / 2.0 + gap as f64),
            w: track_w,
            h: track_h,
        },
        shine_w: (track_w as f64 * cfg.shine_w_frac).round().max(1.0),
    }
}

/// Alpha mask of the mark fitted into an `edge` x `edge` box, centred.
/// SVG is rendered with resvg; a raster uses its own alpha. Shapes smaller
/// than 2% of the largest (a trademark sign) are dropped.
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
        // `as_chunks` over a const size (clippy prefers it to `chunks_exact`);
        // RGBA pixels, so there is never a partial chunk.
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

/// Gaussian blur of a single-channel `w` x `h` image, separable, edge-clamped.
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

/// Composite a colour through an alpha mask (scaled by `opacity`) onto an
/// XRGB scene buffer with its top-left at (`ox`, `oy`).
#[allow(clippy::too_many_arguments)]
fn composite(
    buf: &mut [u8],
    sw: usize,
    sh: usize,
    mask: &[u8],
    mw: usize,
    ox: i64,
    oy: i64,
    rgb: [u8; 3],
    opacity: f64,
) {
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        let (x, y) = (ox + (i % mw) as i64, oy + (i / mw) as i64);
        if x < 0 || y < 0 || x >= sw as i64 || y >= sh as i64 {
            continue;
        }
        let a = (f64::from(m) * opacity.clamp(0.0, 1.0)).round() as u32;
        let o = y as usize * sw * 4 + x as usize * 4;
        for (c, dst) in [(rgb[2], o), (rgb[1], o + 1), (rgb[0], o + 2)] {
            let old = u32::from(buf[dst]);
            buf[dst] = ((u32::from(c) * a + old * (255 - a) + 127) / 255) as u8;
        }
    }
}

impl Splash {
    /// Build the static layer for a scene of `scene` pixels on a panel with
    /// the given quarter-turn `transform`.
    pub fn new(cfg: &SplashConfig, scene: (u32, u32), transform: u8) -> Self {
        let transform = transform % 4;
        let ground = parse_color(&cfg.ground, [0x0b, 0x0d, 0x10]);
        let mark = parse_color(&cfg.mark, [0xc3, 0xc9, 0xd0]);
        let accent = parse_color(&cfg.accent, [0x17, 0x93, 0xd1]);
        let line = parse_color(&cfg.track, [0x1f, 0x28, 0x32]);
        let sweep = parse_color(&cfg.shine, [0xbe, 0xe4, 0xf7]);
        let (sw, sh) = (scene.0 as usize, scene.1 as usize);
        let g = layout(cfg, scene);
        let track = g.track;

        // Scene-space base, upright.
        let mut scene_buf = vec![0u8; sw * sh * 4];
        for y in 0..sh {
            for x in 0..sw {
                put(&mut scene_buf, sw * 4, x, y, ground);
            }
        }
        if let Some(path) = cfg.logo.as_ref().filter(|p| !p.as_os_str().is_empty()) {
            match mask(path, g.logo) {
                Ok(logo) => {
                    let edge = g.logo as usize;
                    let ge = g.glow as usize;
                    // Glow: the silhouette centred in a larger box, blurred.
                    let mut big = vec![0u8; ge * ge];
                    let off = (ge.saturating_sub(edge)) / 2;
                    for y in 0..edge.min(ge) {
                        for x in 0..edge.min(ge) {
                            big[(y + off) * ge + x + off] = logo[y * edge + x];
                        }
                    }
                    let glow = blur(&big, ge, ge, f64::from(g.logo / 6));
                    let (gx, gy) = (rnd(g.cx - ge as f64 / 2.0), rnd(g.cy - ge as f64 / 2.0));
                    composite(
                        &mut scene_buf,
                        sw,
                        sh,
                        &glow,
                        ge,
                        gx,
                        gy,
                        accent,
                        cfg.glow_opacity,
                    );
                    let (lx, ly) = (rnd(g.cx - edge as f64 / 2.0), rnd(g.cy - edge as f64 / 2.0));
                    composite(&mut scene_buf, sw, sh, &logo, edge, lx, ly, mark, 1.0);
                }
                Err(e) => eprintln!("vigil: splash logo {}: {e}", path.display()),
            }
        }
        for ty in 0..track.h {
            for tx in 0..track.w {
                let (x, y) = (track.x + tx, track.y + ty);
                if x >= 0 && y >= 0 && (x as usize) < sw && (y as usize) < sh {
                    put(&mut scene_buf, sw * 4, x as usize, y as usize, line);
                }
            }
        }

        let (pw, ph) = if transform % 2 == 1 {
            (scene.1, scene.0)
        } else {
            scene
        };
        let base = if transform == 0 {
            scene_buf
        } else {
            let mut out = vec![0u8; pw as usize * ph as usize * 4];
            for y in 0..sh {
                for x in 0..sw {
                    let (px, py) = scene_to_panel(transform, sw, sh, x, y);
                    let (s, d) = (y * sw * 4 + x * 4, py * pw as usize * 4 + px * 4);
                    out[d..d + 4].copy_from_slice(&scene_buf[s..s + 4]);
                }
            }
            out
        };
        Self {
            scene,
            panel: (pw, ph),
            transform,
            base,
            track,
            line,
            sweep,
            sweep_w: g.shine_w,
            sweep_ms: cfg.sweep_ms.max(1),
        }
    }

    #[cfg(test)]
    pub fn track(&self) -> Track {
        self.track
    }

    /// Highlight strength 0..=0.8 at track column `x` after `t_ms`: a
    /// gradient band that enters from the left edge, crosses, and repeats.
    pub fn sweep_alpha(&self, x: f64, t_ms: u64) -> f64 {
        let phase = (t_ms % self.sweep_ms) as f64 / self.sweep_ms as f64;
        let start = -self.sweep_w + (self.track.w as f64 + self.sweep_w) * phase;
        let u = (x + 0.5 - start) / self.sweep_w;
        if !(0.0..=1.0).contains(&u) {
            return 0.0;
        }
        0.8 * (1.0 - (2.0 * u - 1.0).abs())
    }

    fn fits(&self, target: &FrameTarget<'_>) -> bool {
        target.width == self.panel.0
            && target.height == self.panel.1
            && target.stride >= target.width as usize * 4
            && target.buffer.len() >= target.stride * target.height as usize
    }

    /// The whole splash, with the sweep at `t_ms`. False when the target is
    /// not this panel's size (nothing is drawn).
    pub fn draw_full(&self, target: &mut FrameTarget<'_>, t_ms: u64) -> bool {
        if !self.fits(target) {
            return false;
        }
        let row = self.panel.0 as usize * 4;
        for y in 0..self.panel.1 as usize {
            target.buffer[y * target.stride..y * target.stride + row]
                .copy_from_slice(&self.base[y * row..(y + 1) * row]);
        }
        self.draw_track(target, t_ms);
        true
    }

    /// Only the loading line. Valid on a buffer that already holds the
    /// splash's static layer.
    pub fn draw_track(&self, target: &mut FrameTarget<'_>, t_ms: u64) {
        let (sw, sh) = (self.scene.0 as usize, self.scene.1 as usize);
        for ty in 0..self.track.h {
            for tx in 0..self.track.w {
                let (x, y) = (self.track.x + tx, self.track.y + ty);
                if x < 0 || y < 0 || x as usize >= sw || y as usize >= sh {
                    continue;
                }
                let a = self.sweep_alpha(tx as f64, t_ms);
                let mix =
                    |l: u8, s: u8| (f64::from(l) + (f64::from(s) - f64::from(l)) * a).round() as u8;
                let rgb = [
                    mix(self.line[0], self.sweep[0]),
                    mix(self.line[1], self.sweep[1]),
                    mix(self.line[2], self.sweep[2]),
                ];
                let (px, py) = scene_to_panel(self.transform, sw, sh, x as usize, y as usize);
                put(target.buffer, target.stride, px, py, rgb);
            }
        }
    }

    /// Mix the splash's static layer over what the target already holds:
    /// `alpha` 1 is all splash, 0 leaves the target untouched.
    pub fn blend_over(&self, target: &mut FrameTarget<'_>, alpha: f32) {
        if !self.fits(target) || alpha <= 0.0 {
            return;
        }
        let a = (alpha.min(1.0) * 256.0) as u32;
        let row = self.panel.0 as usize * 4;
        for y in 0..self.panel.1 as usize {
            let dst = &mut target.buffer[y * target.stride..y * target.stride + row];
            let src = &self.base[y * row..(y + 1) * row];
            for (d, s) in dst.iter_mut().zip(src) {
                *d = ((u32::from(*s) * a + u32::from(*d) * (256 - a)) >> 8) as u8;
            }
        }
    }
}

/// Where one output is in the splash-to-greeter hand-off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// The splash owns the screen; the greeter is not ready.
    Hold,
    /// Cross-fading; the value is the splash's remaining opacity.
    Fade(f32),
    /// The greeter owns the screen.
    Done,
}

/// Timing for one output's cover. The splash itself lives in the entry
/// because the post-login splash needs it after the cover is gone.
pub struct Cover {
    started: Instant,
    ready_at: Option<Instant>,
    /// Frames fully drawn into distinct swapchain slots (2 means both
    /// buffers hold the static layer, so animation can redraw the line only).
    slots_filled: u8,
    last_draw: Option<Instant>,
}

/// Animation cadence while waiting.
pub const FRAME_EVERY: Duration = Duration::from_millis(33);

impl Cover {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            ready_at: None,
            slots_filled: 0,
            last_draw: None,
        }
    }

    /// The greeter has what it was waiting for (its background).
    pub fn mark_ready(&mut self, now: Instant) {
        self.ready_at.get_or_insert(now);
    }

    pub fn phase(&mut self, now: Instant, cfg: &SplashConfig) -> Phase {
        let max_wait = Duration::from_millis(cfg.max_wait_ms);
        if self.ready_at.is_none() && now.duration_since(self.started) >= max_wait {
            self.ready_at = Some(now);
        }
        let Some(ready) = self.ready_at else {
            return Phase::Hold;
        };
        let fade = Duration::from_millis(cfg.fade_ms);
        let elapsed = now.duration_since(ready);
        if fade.is_zero() || elapsed >= fade {
            Phase::Done
        } else {
            Phase::Fade(1.0 - elapsed.as_secs_f32() / fade.as_secs_f32())
        }
    }

    /// Splash milliseconds for the sweep.
    pub fn t_ms(&self, now: Instant) -> u64 {
        now.duration_since(self.started).as_millis() as u64
    }

    pub fn due(&self, now: Instant) -> bool {
        self.last_draw
            .is_none_or(|t| now.duration_since(t) >= FRAME_EVERY)
    }

    pub fn needs_full(&self) -> bool {
        self.slots_filled < 2
    }

    /// A drawn frame reached the CRTC.
    pub fn presented(&mut self, now: Instant) {
        self.slots_filled = (self.slots_filled + 1).min(2);
        self.last_draw = Some(now);
    }

    /// The swapchain was invalidated; both slots need the static layer again.
    pub fn reset_slots(&mut self) {
        self.slots_filled = 0;
    }

    /// The cover has been released by the greeter and is fading off.
    pub fn released(&self) -> bool {
        self.ready_at.is_some()
    }

    /// A frame has reached the CRTC, and when the last one did.
    pub fn last_presented(&self) -> Option<Instant> {
        self.last_draw
    }

    /// Time until the next animation frame is due.
    pub fn next_in(&self, now: Instant) -> Duration {
        self.last_draw.map_or(Duration::ZERO, |t| {
            FRAME_EVERY.saturating_sub(now.duration_since(t))
        })
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
        }
    }

    fn px(t: &FrameTarget<'_>, x: usize, y: usize) -> [u8; 3] {
        let o = y * t.stride + x * 4;
        [t.buffer[o + 2], t.buffer[o + 1], t.buffer[o]]
    }

    #[test]
    fn parses_colors_and_keeps_fallback() {
        assert_eq!(parse_color("#0b0d10", [1, 2, 3]), [0x0b, 0x0d, 0x10]);
        assert_eq!(parse_color("nope", [1, 2, 3]), [1, 2, 3]);
        assert_eq!(parse_color("#12", [1, 2, 3]), [1, 2, 3]);
    }

    #[test]
    fn geometry_follows_the_shared_scene_spec() {
        // Same numbers desktop-boot lib/scene.py geometry() gives for 3440x1440.
        let g = layout(&SplashConfig::default(), (3440, 1440));
        assert_eq!(g.logo, 230); // round(1440 * 0.16)
        assert_eq!(g.glow, 460);
        assert_eq!(g.cx, 1720.0);
        assert!((g.cy - 676.8).abs() < 1e-9);
        assert_eq!((g.track.w, g.track.h), (374, 4)); // 0.26, 0.0028 -> 4.03
        assert_eq!(g.track.x, 1533);
        assert_eq!(g.track.y, 677 + 115 + 72); // round(cy + 115 + 72)
        assert_eq!(g.shine_w, 150.0);
    }

    #[test]
    fn geometry_matches_scene_py_on_other_screens() {
        // Values printed by desktop-boot lib/scene.py geometry(W, H):
        // (W, H, logo, track_x, track_y, track_w, track_h, shine_w)
        for (w, h, logo, tx, ty, tw, th, shine) in [
            (
                2160u32, 3840u32, 614u32, 581i64, 2304i64, 998i64, 11i64, 399.0,
            ),
            (1920, 1080, 173, 820, 648, 281, 3, 112.0),
        ] {
            let g = layout(&SplashConfig::default(), (w, h));
            assert_eq!(
                (
                    g.logo, g.track.x, g.track.y, g.track.w, g.track.h, g.shine_w
                ),
                (logo, tx, ty, tw, th, shine),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn track_thickness_has_a_floor_and_everything_scales_with_height() {
        let g = layout(&SplashConfig::default(), (600, 400));
        assert_eq!(g.track.h, 2, "0.0028 * 400 = 1.1, floored to 2");
        let tall = layout(&SplashConfig::default(), (2160, 3840));
        assert_eq!(tall.logo, 614);
    }

    #[test]
    fn first_frame_is_ground_with_idle_line() {
        let cfg = SplashConfig::default();
        let s = Splash::new(&cfg, (400, 200), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 400, 200);
        assert!(s.draw_full(&mut t, 0));
        assert_eq!(px(&t, 0, 0), [0x0b, 0x0d, 0x10]);
        let tr = s.track();
        assert_eq!(
            px(&t, (tr.x + 3) as usize, tr.y as usize),
            [0x1f, 0x28, 0x32],
            "sweep has not entered at t=0"
        );
    }

    #[test]
    fn sweep_crosses_the_line_and_repeats() {
        let s = Splash::new(&SplashConfig::default(), (800, 500), 0);
        let mid = s.sweep_alpha(s.track().w as f64 / 2.0, 1100);
        assert!(mid > 0.5, "centre of the sweep is bright, got {mid}");
        assert_eq!(s.sweep_alpha(0.0, 0), 0.0);
        assert_eq!(s.sweep_alpha(50.0, 123), s.sweep_alpha(50.0, 123 + 2200));
    }

    #[test]
    fn rotated_output_puts_the_line_in_panel_space() {
        let s = Splash::new(&SplashConfig::default(), (200, 400), 1);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 400, 200);
        assert!(s.draw_full(&mut t, 0));
        let tr = s.track();
        let (px_, py_) = scene_to_panel(1, 200, 400, (tr.x + 3) as usize, tr.y as usize);
        assert_eq!(px(&t, px_, py_), [0x1f, 0x28, 0x32]);
        assert_eq!(px(&t, 0, 0), [0x0b, 0x0d, 0x10]);
    }

    #[test]
    fn wrong_sized_target_is_refused() {
        let s = Splash::new(&SplashConfig::default(), (400, 200), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 10, 10);
        assert!(!s.draw_full(&mut t, 0));
    }

    fn write_logo(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vigil-splash-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
        <rect x="10" y="10" width="80" height="80" fill="#1793d1"/>
        <rect x="0" y="0" width="2" height="2" fill="#1793d1"/></svg>"##;

    #[test]
    fn svg_mark_is_centred_in_the_mark_colour_and_specks_are_dropped() {
        let cfg = SplashConfig {
            logo: Some(write_logo("logo.svg", SVG)),
            ..SplashConfig::default()
        };
        let s = Splash::new(&cfg, (800, 500), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 800, 500);
        s.draw_full(&mut t, 0);
        let g = layout(&cfg, (800, 500));
        let (cx, cy) = (400usize, g.cy as usize);
        assert_eq!(
            px(&t, cx, cy),
            [0xc3, 0xc9, 0xd0],
            "mark colour, not the SVG's"
        );
        // The 2x2 corner shape (a TM sign) must not survive: the box's
        // top-left pixel stays ground or glow, never mark colour.
        let (bx, by) = (400 - g.logo as usize / 2, cy - g.logo as usize / 2);
        assert_ne!(px(&t, bx, by), [0xc3, 0xc9, 0xd0]);
        // Glow is accent-tinted ground just outside the mark.
        let out = px(&t, 400, cy - g.logo as usize / 2 - 5);
        assert!(
            out[2] > 0x10,
            "accent glow raises blue above ground, got {out:?}"
        );
    }

    #[test]
    fn png_mark_uses_its_alpha() {
        let dir = std::env::temp_dir().join(format!("vigil-splash-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logo.png");
        image::RgbaImage::from_pixel(32, 32, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let cfg = SplashConfig {
            logo: Some(path),
            ..SplashConfig::default()
        };
        let s = Splash::new(&cfg, (800, 500), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 800, 500);
        s.draw_full(&mut t, 0);
        assert_eq!(px(&t, 400, 235), [0xc3, 0xc9, 0xd0]);
    }

    #[test]
    fn missing_logo_leaves_ground_and_line() {
        let cfg = SplashConfig {
            logo: Some("/nonexistent/logo.svg".into()),
            ..SplashConfig::default()
        };
        let s = Splash::new(&cfg, (400, 200), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 400, 200);
        assert!(s.draw_full(&mut t, 0));
        assert_eq!(px(&t, 200, 94), [0x0b, 0x0d, 0x10]);
    }

    #[test]
    fn blend_moves_from_target_to_splash() {
        let s = Splash::new(&SplashConfig::default(), (4, 4), 0);
        let mut buf = Vec::new();
        let mut t = target(&mut buf, 4, 4);
        for b in t.buffer.iter_mut() {
            *b = 200;
        }
        s.blend_over(&mut t, 0.0);
        assert_eq!(px(&t, 0, 0), [200, 200, 200]);
        s.blend_over(&mut t, 1.0);
        let p = px(&t, 0, 0);
        assert!(
            p[0] <= 0x0c && p[2] <= 0x11,
            "all splash at alpha 1, got {p:?}"
        );
    }

    #[test]
    fn cover_holds_then_fades_then_is_done() {
        let cfg = SplashConfig::default();
        let t0 = Instant::now();
        let mut c = Cover::new(t0);
        assert_eq!(c.phase(t0 + Duration::from_millis(500), &cfg), Phase::Hold);
        let ready = t0 + Duration::from_millis(600);
        c.mark_ready(ready);
        match c.phase(ready + Duration::from_millis(125), &cfg) {
            Phase::Fade(a) => assert!((a - 0.5).abs() < 0.01),
            other => panic!("expected fade, got {other:?}"),
        }
        assert_eq!(
            c.phase(ready + Duration::from_millis(300), &cfg),
            Phase::Done
        );
    }

    #[test]
    fn cover_gives_up_waiting_after_max_wait() {
        let cfg = SplashConfig::default();
        let t0 = Instant::now();
        let mut c = Cover::new(t0);
        let late = t0 + Duration::from_millis(cfg.max_wait_ms + 1);
        assert!(matches!(c.phase(late, &cfg), Phase::Fade(_)));
    }

    #[test]
    fn zero_fade_cuts() {
        let cfg = SplashConfig {
            fade_ms: 0,
            ..SplashConfig::default()
        };
        let t0 = Instant::now();
        let mut c = Cover::new(t0);
        c.mark_ready(t0);
        assert_eq!(c.phase(t0, &cfg), Phase::Done);
    }

    #[test]
    fn animation_redraws_only_the_line_once_both_slots_are_filled() {
        let t0 = Instant::now();
        let mut c = Cover::new(t0);
        assert!(c.needs_full());
        c.presented(t0);
        assert!(c.needs_full());
        c.presented(t0);
        assert!(!c.needs_full());
        c.reset_slots();
        assert!(c.needs_full());
    }
}
