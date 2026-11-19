use crate::glow::glow_mask;
use crate::spec::{Geometry, Scene, rnd};
use crate::{Error, Logo, MAX_EDGE, Mask};

pub const MAX_SIDE: u32 = 16384;
pub const MAX_PIXELS: u64 = 8192 * 8192;
pub const MAX_GLOW: u32 = 4096;

const MAX_REACH: i64 = 4 * MAX_SIDE as i64;
const MAX_SIGMA: u32 = MAX_EDGE / 6;

pub fn to_panel(transform: u8, scene: (u32, u32), x: i64, y: i64) -> Option<(i64, i64)> {
    let (sw, sh) = (i64::from(scene.0), i64::from(scene.1));
    Some(match transform % 4 {
        1 => (y, (sw - 1).checked_sub(x)?),
        2 => ((sw - 1).checked_sub(x)?, (sh - 1).checked_sub(y)?),
        3 => ((sh - 1).checked_sub(y)?, x),
        _ => (x, y),
    })
}

// Pixels inside a sprite are mapped by their offset in its box, which cannot
// overflow the way sums of scene coordinates can.
fn local(transform: u8, size: (usize, usize), x: usize, y: usize) -> (usize, usize) {
    let (w, h) = size;
    match transform % 4 {
        1 => (y, w - 1 - x),
        2 => (w - 1 - x, h - 1 - y),
        3 => (h - 1 - y, x),
        _ => (x, y),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    panel: (u32, u32),
    scene: (u32, u32),
    transform: u8,
    geometry: Geometry,
}

impl Layout {
    fn of(spec: &Scene, scene: (u32, u32), transform: u8) -> Layout {
        let transform = transform % 4;
        let panel = if transform % 2 == 1 {
            (scene.1, scene.0)
        } else {
            scene
        };
        Layout {
            panel,
            scene,
            transform,
            geometry: spec.geometry(scene.0, scene.1),
        }
    }

    pub fn new(spec: &Scene, panel: (u32, u32), transform: u8) -> Result<Layout, Error> {
        spec.check()?;
        let (w, h) = panel;
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err(Error::TooLarge(format!(
                "an output of {w}x{h} is outside 1..={MAX_SIDE} on a side"
            )));
        }
        let area = u64::from(w) * u64::from(h);
        if area > MAX_PIXELS {
            return Err(Error::TooLarge(format!(
                "an output of {w}x{h} is over {MAX_PIXELS} pixels"
            )));
        }
        let scene = if transform % 2 == 1 { (h, w) } else { (w, h) };
        let layout = Layout::of(spec, scene, transform);
        let g = layout.geometry;
        if g.logo > MAX_EDGE || g.glow > MAX_GLOW || u64::from(g.bar_w) * u64::from(g.bar_h) > area
        {
            return Err(Error::TooLarge(format!(
                "the scene asks for a {} px logo, a {} px glow and a {}x{} bar on a {w}x{h} output",
                g.logo, g.glow, g.bar_w, g.bar_h
            )));
        }
        for at in [layout.logo(), layout.glow(), layout.bar()] {
            let reach = at.and_then(|p| {
                let far = p.x.checked_add(i64::from(p.width))?;
                let low = p.y.checked_add(i64::from(p.height))?;
                Some(p.x.abs().max(p.y.abs()).max(far.abs()).max(low.abs()))
            });
            if reach.is_none_or(|r| r > MAX_REACH) {
                return Err(Error::Refused(format!(
                    "the scene places a sprite at {at:?}, beyond {MAX_REACH} pixels"
                )));
            }
        }
        Ok(layout)
    }

    pub fn panel(&self) -> (u32, u32) {
        self.panel
    }

    pub fn scene(&self) -> (u32, u32) {
        self.scene
    }

    pub fn transform(&self) -> u8 {
        self.transform
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn place(&self, x: i64, y: i64, w: u32, h: u32) -> Option<Placement> {
        let a = to_panel(self.transform, self.scene, x, y)?;
        let b = to_panel(
            self.transform,
            self.scene,
            x.checked_add(i64::from(w))?.checked_sub(1)?,
            y.checked_add(i64::from(h))?.checked_sub(1)?,
        )?;
        let (width, height) = if self.transform % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        };
        Some(Placement {
            x: a.0.min(b.0),
            y: a.1.min(b.1),
            width,
            height,
        })
    }

    pub fn logo(&self) -> Option<Placement> {
        let g = self.geometry;
        self.place(g.logo_x, g.logo_y, g.logo, g.logo)
    }

    pub fn glow(&self) -> Option<Placement> {
        let g = self.geometry;
        self.place(g.glow_x, g.glow_y, g.glow, g.glow)
    }

    pub fn bar(&self) -> Option<Placement> {
        let g = self.geometry;
        self.place(g.bar_x, g.bar_y, g.bar_w, g.bar_h)
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
    layout: Layout,
    ground: [u8; 3],
    accent: [u8; 3],
    mark: [u8; 3],
    track: [u8; 3],
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
        let panel = if transform % 2 == 1 {
            (scene.1, scene.0)
        } else {
            scene
        };
        let layout = Layout::new(spec, panel, transform)?;
        let mask = logo.mask(layout.geometry.logo)?;
        Ok(Plate::build(spec, Some(mask), layout))
    }

    pub fn ground_only(spec: &Scene, scene: (u32, u32), transform: u8) -> Plate {
        Plate::build(spec, None, Layout::of(spec, scene, transform))
    }

    fn build(spec: &Scene, logo: Option<Mask>, layout: Layout) -> Plate {
        let Layout {
            scene, transform, ..
        } = layout;
        let (sw, sh) = (scene.0 as usize, scene.1 as usize);
        let g = layout.geometry;
        let (edge, ge) = (g.logo as usize, g.glow as usize);
        let sigma = f64::from((g.logo / spec.glow_blur_div).min(MAX_SIGMA));
        let glow = logo
            .as_ref()
            .map(|l| glow_mask(l.alpha(), edge, ge, sigma))
            .unwrap_or_default();

        let span = |from: i64, side: usize| {
            let end = from.saturating_add(ge as i64);
            let side = side as i64;
            (from.clamp(0, side) as usize, end.clamp(0, side) as usize)
        };
        let ((x0, x1), (y0, y1)) = (span(g.glow_x, sw), span(g.glow_y, sh));
        let size = (x1.saturating_sub(x0), y1.saturating_sub(y0));
        let rect = logo
            .as_ref()
            .filter(|_| size.0 > 0 && size.1 > 0)
            .and_then(|_| layout.place(x0 as i64, y0 as i64, size.0 as u32, size.1 as u32))
            .map_or(Rect::default(), |p| Rect {
                x: p.x as usize,
                y: p.y as usize,
                w: p.width as usize,
                h: p.height as usize,
            });
        let mut glow_alpha = vec![0u8; rect.w * rect.h];
        let mut mark_alpha = vec![0u8; rect.w * rect.h];
        if let Some(logo) = logo.as_ref().filter(|_| rect.w > 0) {
            let logo = logo.alpha();
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let (bx, by) = (sx as i64 - g.glow_x, sy as i64 - g.glow_y);
                    let (lx, ly) = local(transform, size, sx - x0, sy - y0);
                    let i = ly * rect.w + lx;
                    glow_alpha[i] = glow[by as usize * ge + bx as usize];
                    let (lx, ly) = (sx as i64 - g.logo_x, sy as i64 - g.logo_y);
                    if (0..edge as i64).contains(&lx) && (0..edge as i64).contains(&ly) {
                        mark_alpha[i] = logo[ly as usize * edge + lx as usize];
                    }
                }
            }
        }
        Plate {
            layout,
            ground: spec.bg,
            accent: spec.accent,
            mark: spec.mark,
            track: spec.track,
            rect,
            glow_alpha,
            mark_alpha,
            logo,
            glow,
        }
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn panel(&self) -> (u32, u32) {
        self.layout.panel
    }

    pub fn ground(&self) -> [u8; 3] {
        self.ground
    }

    pub fn geometry(&self) -> Geometry {
        self.layout.geometry
    }

    pub fn fits(&self, width: u32, height: u32, stride: usize, len: usize) -> bool {
        (width, height) == self.panel()
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
        let (w, h) = self.panel();
        let row = w as usize * 4;
        for y in 0..h as usize {
            let (pixels, _partial) = xrgb[y * stride..y * stride + row].as_chunks_mut::<4>();
            pixels.fill(px);
        }
        self.draw_rect(xrgb, stride, level);
    }

    pub fn frame(&self, level: f64) -> Vec<u8> {
        let (w, h) = self.panel();
        let stride = w as usize * 4;
        let mut xrgb = vec![0u8; stride * h as usize];
        self.draw(&mut xrgb, stride, level);
        xrgb
    }

    fn sprite(
        &self,
        at: Placement,
        size: (u32, u32),
        pixel: impl Fn(u32, u32) -> [u8; 4],
    ) -> Sprite {
        let (w, h) = (size.0 as usize, size.1 as usize);
        let mut rgba = vec![0u8; w * h * 4];
        for sy in 0..h {
            for sx in 0..w {
                let (lx, ly) = local(self.layout.transform, (w, h), sx, sy);
                let o = (ly * at.width as usize + lx) * 4;
                rgba[o..o + 4].copy_from_slice(&pixel(sx as u32, sy as u32));
            }
        }
        Sprite {
            x: at.x,
            y: at.y,
            width: at.width,
            height: at.height,
            rgba,
        }
    }

    pub fn logo_sprite(&self) -> Option<Sprite> {
        let logo = self.logo.as_ref()?;
        let (edge, alpha, m) = (logo.edge(), logo.alpha(), self.mark);
        Some(self.sprite(self.layout.logo()?, (edge, edge), |x, y| {
            [m[0], m[1], m[2], alpha[(y * edge + x) as usize]]
        }))
    }

    pub fn glow_sprite(&self, level: f64) -> Option<Sprite> {
        self.logo.as_ref()?;
        let (lvl, a) = (level_256(level), self.accent);
        let ge = self.layout.geometry.glow;
        Some(self.sprite(self.layout.glow()?, (ge, ge), |x, y| {
            let alpha = (u32::from(self.glow[(y * ge + x) as usize]) * lvl) >> 8;
            [a[0], a[1], a[2], alpha as u8]
        }))
    }

    pub fn bar_sprite(&self, progress: f64) -> Option<Sprite> {
        let g = self.layout.geometry;
        let fill = rnd(progress.clamp(0.0, 1.0) * f64::from(g.bar_w)).max(0) as u32;
        let (a, t) = (self.accent, self.track);
        Some(self.sprite(self.layout.bar()?, (g.bar_w, g.bar_h), |x, _| {
            let c = if x < fill { a } else { t };
            [c[0], c[1], c[2], 0xff]
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ground_only_plate_survives_a_scene_built_by_hand() {
        let spec = Scene {
            centre_frac: 1e300,
            logo_frac: 1e300,
            glow_mult: u32::MAX,
            ..Scene::default()
        };
        let plate = Plate::ground_only(&spec, (64, 48), 1);
        assert_eq!(plate.frame(0.5).len(), 64 * 48 * 4);
        let _ = plate.bar_sprite(0.5);
    }
}
