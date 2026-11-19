use crate::glow::glow_mask;
use crate::spec::{Geometry, Scene, rnd};
use crate::{Error, Logo, MAX_EDGE, Mask};

pub const MAX_SIDE: u32 = 16384;
pub const MAX_PIXELS: u64 = 8192 * 8192;
pub const MAX_GLOW: u32 = 4096;

pub fn check_output(spec: &Scene, panel: (u32, u32), transform: u8) -> Result<(), Error> {
    let (w, h) = panel;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(Error::TooLarge(format!(
            "an output of {w}x{h} is outside 1..={MAX_SIDE} on a side"
        )));
    }
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(Error::TooLarge(format!(
            "an output of {w}x{h} is over {MAX_PIXELS} pixels"
        )));
    }
    let scene = if transform % 2 == 1 { (h, w) } else { (w, h) };
    let g = spec.geometry(scene.0, scene.1);
    if g.logo > MAX_EDGE || g.glow > MAX_GLOW || g.bar_w > MAX_SIDE || g.bar_h > MAX_SIDE {
        return Err(Error::TooLarge(format!(
            "the scene asks for a {} px logo, a {} px glow and a {}x{} bar",
            g.logo, g.glow, g.bar_w, g.bar_h
        )));
    }
    Ok(())
}

pub fn to_panel(transform: u8, scene: (u32, u32), x: i64, y: i64) -> (i64, i64) {
    let (sw, sh) = (i64::from(scene.0), i64::from(scene.1));
    match transform % 4 {
        1 => (y, sw - 1 - x),
        2 => (sw - 1 - x, sh - 1 - y),
        3 => (sh - 1 - y, x),
        _ => (x, y),
    }
}

pub fn mix(from: u8, to: u8, a: u8) -> u8 {
    let (from, to, a) = (u32::from(from), u32::from(to), u32::from(a));
    ((from * (255 - a) + to * a + 127) / 255) as u8
}

fn level_256(level: f64) -> u32 {
    (level.clamp(0.0, 1.0) * 256.0).round() as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sprite {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Sprite {
    pub fn draw_over(&self, xrgb: &mut [u8], stride: usize, panel: (u32, u32)) {
        for row in 0..self.height as i64 {
            let py = self.y + row;
            if !(0..i64::from(panel.1)).contains(&py) {
                continue;
            }
            for col in 0..self.width as i64 {
                let px = self.x + col;
                if !(0..i64::from(panel.0)).contains(&px) {
                    continue;
                }
                let s = (row as usize * self.width as usize + col as usize) * 4;
                let src = &self.rgba[s..s + 4];
                let o = py as usize * stride + px as usize * 4;
                for (c, off) in [(0usize, 2usize), (1, 1), (2, 0)] {
                    xrgb[o + off] = mix(xrgb[o + off], src[c], src[3]);
                }
                xrgb[o + 3] = 0xff;
            }
        }
    }
}

pub struct Plate {
    panel: (u32, u32),
    scene: (u32, u32),
    transform: u8,
    ground: [u8; 3],
    accent: [u8; 3],
    mark: [u8; 3],
    track: [u8; 3],
    geometry: Geometry,
    rect: Rect,
    glow_alpha: Vec<u8>,
    mark_alpha: Vec<u8>,
    logo: Option<Mask>,
    glow: Vec<u8>,
}

impl Plate {
    pub fn new(
        spec: &Scene,
        logo: &Logo,
        scene: (u32, u32),
        transform: u8,
    ) -> Result<Plate, Error> {
        let g = spec.geometry(scene.0, scene.1);
        if g.glow > MAX_GLOW {
            return Err(Error::TooLarge(format!(
                "a glow {} px wide is over {MAX_GLOW}",
                g.glow
            )));
        }
        let mask = logo.mask(g.logo)?;
        Ok(Plate::build(spec, Some(mask), scene, transform))
    }

    pub fn ground_only(spec: &Scene, scene: (u32, u32), transform: u8) -> Plate {
        Plate::build(spec, None, scene, transform)
    }

    fn build(spec: &Scene, logo: Option<Mask>, scene: (u32, u32), transform: u8) -> Plate {
        let transform = transform % 4;
        let (sw, sh) = (scene.0 as usize, scene.1 as usize);
        let panel = if transform % 2 == 1 {
            (scene.1, scene.0)
        } else {
            scene
        };
        let g = spec.geometry(scene.0, scene.1);
        let (edge, ge) = (g.logo as usize, g.glow as usize);
        let sigma = f64::from(g.logo / spec.glow_blur_div);
        let glow = logo
            .as_ref()
            .map(|l| glow_mask(l.alpha(), edge, ge, sigma))
            .unwrap_or_default();

        let (x0, y0) = (g.glow_x.max(0) as usize, g.glow_y.max(0) as usize);
        let x1 = ((g.glow_x + ge as i64).max(0) as usize).min(sw);
        let y1 = ((g.glow_y + ge as i64).max(0) as usize).min(sh);
        let rect = if logo.is_some() && x1 > x0 && y1 > y0 {
            let a = to_panel(transform, scene, x0 as i64, y0 as i64);
            let b = to_panel(transform, scene, x1 as i64 - 1, y1 as i64 - 1);
            Rect {
                x: a.0.min(b.0) as usize,
                y: a.1.min(b.1) as usize,
                w: (a.0.max(b.0) - a.0.min(b.0) + 1) as usize,
                h: (a.1.max(b.1) - a.1.min(b.1) + 1) as usize,
            }
        } else {
            Rect::default()
        };
        let mut glow_alpha = vec![0u8; rect.w * rect.h];
        let mut mark_alpha = vec![0u8; rect.w * rect.h];
        if let Some(logo) = &logo {
            let logo = logo.alpha();
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let (bx, by) = (sx as i64 - g.glow_x, sy as i64 - g.glow_y);
                    let (px, py) = to_panel(transform, scene, sx as i64, sy as i64);
                    let i = (py as usize - rect.y) * rect.w + (px as usize - rect.x);
                    glow_alpha[i] = glow[by as usize * ge + bx as usize];
                    let (lx, ly) = (sx as i64 - g.logo_x, sy as i64 - g.logo_y);
                    if (0..edge as i64).contains(&lx) && (0..edge as i64).contains(&ly) {
                        mark_alpha[i] = logo[ly as usize * edge + lx as usize];
                    }
                }
            }
        }
        Plate {
            panel,
            scene,
            transform,
            ground: spec.bg,
            accent: spec.accent,
            mark: spec.mark,
            track: spec.track,
            geometry: g,
            rect,
            glow_alpha,
            mark_alpha,
            logo,
            glow,
        }
    }

    pub fn panel(&self) -> (u32, u32) {
        self.panel
    }

    pub fn ground(&self) -> [u8; 3] {
        self.ground
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn fits(&self, width: u32, height: u32, stride: usize, len: usize) -> bool {
        (width, height) == self.panel
            && stride >= width as usize * 4
            && stride
                .checked_mul(height as usize)
                .is_some_and(|need| len >= need)
    }

    pub fn draw_rect(&self, xrgb: &mut [u8], stride: usize, level: f64) {
        let r = self.rect;
        let lvl = level_256(level);
        for ry in 0..r.h {
            let row = (r.y + ry) * stride + r.x * 4;
            let alpha = ry * r.w;
            for rx in 0..r.w {
                let ga = ((u32::from(self.glow_alpha[alpha + rx]) * lvl) >> 8) as u8;
                let la = self.mark_alpha[alpha + rx];
                let o = row + rx * 4;
                for (c, off) in [(2usize, 0usize), (1, 1), (0, 2)] {
                    let under = mix(self.ground[c], self.accent[c], ga);
                    xrgb[o + off] = mix(under, self.mark[c], la);
                }
                xrgb[o + 3] = 0xff;
            }
        }
    }

    pub fn draw(&self, xrgb: &mut [u8], stride: usize, level: f64) {
        let px = [self.ground[2], self.ground[1], self.ground[0], 0xff];
        let row = self.panel.0 as usize * 4;
        for y in 0..self.panel.1 as usize {
            let (pixels, _partial) = xrgb[y * stride..y * stride + row].as_chunks_mut::<4>();
            pixels.fill(px);
        }
        self.draw_rect(xrgb, stride, level);
    }

    pub fn frame(&self, level: f64) -> Vec<u8> {
        let stride = self.panel.0 as usize * 4;
        let mut xrgb = vec![0u8; stride * self.panel.1 as usize];
        self.draw(&mut xrgb, stride, level);
        xrgb
    }

    fn sprite(
        &self,
        x: i64,
        y: i64,
        w: u32,
        h: u32,
        pixel: impl Fn(u32, u32) -> [u8; 4],
    ) -> Sprite {
        let a = to_panel(self.transform, self.scene, x, y);
        let b = to_panel(
            self.transform,
            self.scene,
            x + i64::from(w) - 1,
            y + i64::from(h) - 1,
        );
        let (px0, py0) = (a.0.min(b.0), a.1.min(b.1));
        let (width, height) = if self.transform % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        };
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for sy in 0..h {
            for sx in 0..w {
                let (px, py) = to_panel(
                    self.transform,
                    self.scene,
                    x + i64::from(sx),
                    y + i64::from(sy),
                );
                let o = ((py - py0) as usize * width as usize + (px - px0) as usize) * 4;
                rgba[o..o + 4].copy_from_slice(&pixel(sx, sy));
            }
        }
        Sprite {
            x: px0,
            y: py0,
            width,
            height,
            rgba,
        }
    }

    pub fn logo_sprite(&self) -> Option<Sprite> {
        let logo = self.logo.as_ref()?;
        let (edge, alpha, m) = (logo.edge(), logo.alpha(), self.mark);
        let g = self.geometry;
        Some(self.sprite(g.logo_x, g.logo_y, edge, edge, |x, y| {
            [m[0], m[1], m[2], alpha[(y * edge + x) as usize]]
        }))
    }

    pub fn glow_sprite(&self, level: f64) -> Option<Sprite> {
        self.logo.as_ref()?;
        let (lvl, a, g) = (level_256(level), self.accent, self.geometry);
        let ge = g.glow;
        Some(self.sprite(g.glow_x, g.glow_y, ge, ge, |x, y| {
            let alpha = (u32::from(self.glow[(y * ge + x) as usize]) * lvl) >> 8;
            [a[0], a[1], a[2], alpha as u8]
        }))
    }

    pub fn bar_sprite(&self, progress: f64) -> Sprite {
        let g = self.geometry;
        let fill = rnd(progress.clamp(0.0, 1.0) * f64::from(g.bar_w)).max(0) as u32;
        let (a, t) = (self.accent, self.track);
        self.sprite(g.bar_x, g.bar_y, g.bar_w, g.bar_h, |x, _| {
            let c = if x < fill { a } else { t };
            [c[0], c[1], c[2], 0xff]
        })
    }
}
