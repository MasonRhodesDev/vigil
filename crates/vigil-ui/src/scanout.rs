use vigil_core::{CURSOR, Rect, scene_to_panel};

const BLOCK: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct Layout {
    pub transform: u8,
    pub scene: (u32, u32),
}

impl Layout {
    pub fn to_panel(self, rect: Rect) -> Rect {
        let (sw, sh) = self.scene;
        match self.transform {
            1 => Rect::new(rect.y, sw - rect.right(), rect.h, rect.w),
            2 => Rect::new(sw - rect.right(), sh - rect.bottom(), rect.w, rect.h),
            3 => Rect::new(sh - rect.bottom(), rect.x, rect.h, rect.w),
            _ => rect,
        }
    }
}

fn store<'a>(run: &mut [u8], pixels: impl Iterator<Item = &'a u32>) {
    for (out, pixel) in run.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
        *out = pixel.to_ne_bytes();
    }
}

pub(crate) fn copy(shadow: &[u32], layout: Layout, target: &mut [u8], stride: usize, rect: Rect) {
    let (sw, sh) = (layout.scene.0 as usize, layout.scene.1 as usize);
    let (x, y) = (rect.x as usize, rect.y as usize);
    let (right, bottom) = (rect.right() as usize, rect.bottom() as usize);
    match layout.transform {
        1 | 3 => {
            for block in (x..right).step_by(BLOCK) {
                let end = (block + BLOCK).min(right);
                for py in y..bottom {
                    let (mut source, step) = if layout.transform == 1 {
                        (block * sw + sw - 1 - py, sw as isize)
                    } else {
                        ((sh - 1 - block) * sw + py, -(sw as isize))
                    };
                    let run = &mut target[py * stride + block * 4..py * stride + end * 4];
                    for out in run.as_chunks_mut::<4>().0 {
                        *out = shadow[source].to_ne_bytes();
                        source = source.wrapping_add_signed(step);
                    }
                }
            }
        }
        2 => {
            for py in y..bottom {
                let row = (sh - 1 - py) * sw;
                let run = &mut target[py * stride + x * 4..py * stride + right * 4];
                store(run, shadow[row + sw - right..row + sw - x].iter().rev());
            }
        }
        _ => {
            for py in y..bottom {
                target[py * stride + x * 4..py * stride + right * 4]
                    .copy_from_slice(bytemuck::cast_slice(&shadow[py * sw + x..py * sw + right]));
            }
        }
    }
}

pub(crate) fn cursor_rect(layout: Layout, scale: f32, pointer: (f64, f64)) -> Rect {
    let scale = f64::from(scale.max(1.0));
    Rect::new(
        pointer.0 as u32,
        pointer.1 as u32,
        (CURSOR[0].len() as f64 * scale) as u32,
        (CURSOR.len() as f64 * scale) as u32,
    )
    .clipped(layout.scene.0, layout.scene.1)
}

pub(crate) fn blit_cursor(layout: Layout, scale: f32, at: Rect, target: &mut [u8], stride: usize) {
    let scale = f64::from(scale.max(1.0));
    let (sw, sh) = (layout.scene.0 as usize, layout.scene.1 as usize);
    for oy in 0..at.h as usize {
        let row = CURSOR[((oy as f64 / scale) as usize).min(CURSOR.len() - 1)];
        for ox in 0..at.w as usize {
            let pixel: u32 = match row[((ox as f64 / scale) as usize).min(row.len() - 1)] {
                b'X' => 0,
                b'#' => 0x00ff_ffff,
                _ => continue,
            };
            let (tx, ty) = scene_to_panel(
                layout.transform,
                sw,
                sh,
                at.x as usize + ox,
                at.y as usize + oy,
            );
            target[ty * stride + tx * 4..ty * stride + tx * 4 + 4]
                .copy_from_slice(&pixel.to_ne_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: u8 = 0xa5;

    fn panel(layout: Layout) -> (u32, u32) {
        if layout.transform % 2 == 1 {
            (layout.scene.1, layout.scene.0)
        } else {
            layout.scene
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, bound: u32) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) % u64::from(bound)) as u32
        }

        fn rect(&mut self, width: u32, height: u32) -> Rect {
            let x = self.below(width);
            let y = self.below(height);
            Rect::new(x, y, 1 + self.below(width - x), 1 + self.below(height - y))
        }
    }

    fn reference(shadow: &[u32], layout: Layout, stride: usize) -> Vec<u8> {
        let (sw, sh) = (layout.scene.0 as usize, layout.scene.1 as usize);
        let (_, ph) = panel(layout);
        let mut out = vec![SENTINEL; stride * ph as usize];
        for sy in 0..sh {
            for sx in 0..sw {
                let (px, py) = scene_to_panel(layout.transform, sw, sh, sx, sy);
                out[py * stride + px * 4..py * stride + px * 4 + 4]
                    .copy_from_slice(&shadow[sy * sw + sx].to_ne_bytes());
            }
        }
        out
    }

    #[test]
    fn copying_scene_rects_matches_the_per_pixel_mapping_and_stays_inside_them() {
        let scene = (37u32, 23u32);
        let shadow: Vec<u32> = (0..scene.0 * scene.1)
            .map(|i| i.wrapping_mul(2_654_435_761) & 0x00ff_ffff)
            .collect();
        let mut rng = Lcg(7);
        for transform in 0..4u8 {
            let layout = Layout { transform, scene };
            let (pw, ph) = panel(layout);
            let stride = pw as usize * 4 + 12;
            let expected = reference(&shadow, layout, stride);
            for _ in 0..200 {
                let scene_rect = rng.rect(scene.0, scene.1);
                let rect = layout.to_panel(scene_rect);
                let mut target = vec![SENTINEL; stride * ph as usize];
                copy(&shadow, layout, &mut target, stride, rect);
                for py in 0..ph {
                    for px in 0..pw {
                        let at = py as usize * stride + px as usize * 4;
                        let inside = rect.contains(&Rect::new(px, py, 1, 1));
                        let want = if inside {
                            &expected[at..at + 4]
                        } else {
                            &[SENTINEL; 4][..]
                        };
                        assert_eq!(
                            &target[at..at + 4],
                            want,
                            "transform {transform} scene rect {scene_rect:?} panel ({px}, {py})"
                        );
                    }
                }
                for py in 0..ph as usize {
                    let pad = &target[py * stride + pw as usize * 4..(py + 1) * stride];
                    assert!(pad.iter().all(|&b| b == SENTINEL), "row padding written");
                }
            }
        }
    }

    #[test]
    fn a_scene_rect_maps_to_the_panel_rect_its_pixels_land_in() {
        let scene = (9u32, 5u32);
        let mut rng = Lcg(11);
        for transform in 0..4u8 {
            let layout = Layout { transform, scene };
            for _ in 0..100 {
                let rect = rng.rect(scene.0, scene.1);
                let mapped = layout.to_panel(rect);
                assert_eq!(mapped.w * mapped.h, rect.w * rect.h);
                for sy in rect.y..rect.bottom() {
                    for sx in rect.x..rect.right() {
                        let (px, py) = scene_to_panel(
                            transform,
                            scene.0 as usize,
                            scene.1 as usize,
                            sx as usize,
                            sy as usize,
                        );
                        assert!(
                            mapped.contains(&Rect::new(px as u32, py as u32, 1, 1)),
                            "transform {transform} rect {rect:?} pixel ({sx}, {sy})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_cursor_is_drawn_inside_its_rect_and_clipped_at_the_scene_edge() {
        let scene = (40u32, 30u32);
        for transform in 0..4u8 {
            let layout = Layout { transform, scene };
            let (pw, ph) = panel(layout);
            let stride = pw as usize * 4;
            for pointer in [(3.0, 4.0), (35.0, 20.0), (39.0, 29.0)] {
                let at = cursor_rect(layout, 1.5, pointer);
                let panel_rect = layout.to_panel(at);
                let mut target = vec![SENTINEL; stride * ph as usize];
                blit_cursor(layout, 1.5, at, &mut target, stride);
                let mut drawn = 0;
                for py in 0..ph {
                    for px in 0..pw {
                        let i = py as usize * stride + px as usize * 4;
                        if target[i..i + 4] != [SENTINEL; 4] {
                            drawn += 1;
                            assert!(
                                panel_rect.contains(&Rect::new(px, py, 1, 1)),
                                "transform {transform} pointer {pointer:?} drew outside its rect"
                            );
                        }
                    }
                }
                assert!(
                    drawn > 0,
                    "transform {transform} pointer {pointer:?} drew nothing"
                );
            }
        }
    }
}
