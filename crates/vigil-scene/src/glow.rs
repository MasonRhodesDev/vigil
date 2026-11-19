// Each output sums its taps in kernel order, exactly as a per-pixel loop
// would, so the result is bit for bit the same; walking the taps outermost
// lets the inner loop run across a row.
fn blur(src: &[u8], w: usize, h: usize, sigma: f64) -> Vec<u8> {
    if sigma < 0.3 {
        return src.to_vec();
    }
    let radius = (sigma * 3.0).ceil() as usize;
    let mut k: Vec<f64> = (-(radius as i64)..=radius as i64)
        .map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp())
        .collect();
    let total: f64 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= total);
    let mut tmp = vec![0f32; w * h];
    let mut padded = vec![0f64; w + 2 * radius];
    let mut acc = vec![0f64; w];
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        for (i, p) in padded.iter_mut().enumerate() {
            *p = f64::from(row[i.saturating_sub(radius).min(w - 1)]);
        }
        acc.fill(0.0);
        for (j, kv) in k.iter().enumerate() {
            for (a, p) in acc.iter_mut().zip(&padded[j..j + w]) {
                *a += kv * p;
            }
        }
        for (t, a) in tmp[y * w..(y + 1) * w].iter_mut().zip(&acc) {
            *t = *a as f32;
        }
    }
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        acc.fill(0.0);
        for (j, kv) in k.iter().enumerate() {
            let sy = (y + j).saturating_sub(radius).min(h - 1);
            for (a, t) in acc.iter_mut().zip(&tmp[sy * w..(sy + 1) * w]) {
                *a += kv * f64::from(*t);
            }
        }
        for (o, a) in out[y * w..(y + 1) * w].iter_mut().zip(&acc) {
            *o = a.round().clamp(0.0, 255.0) as u8;
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

pub(crate) fn glow_mask(logo: &[u8], edge: usize, ge: usize, sigma: f64) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn per_pixel_blur(src: &[u8], w: usize, h: usize, sigma: f64) -> Vec<u8> {
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

    #[test]
    fn the_row_blur_matches_the_per_pixel_blur_bit_for_bit() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for (w, h) in [(1, 1), (3, 7), (64, 64), (173, 91), (257, 300)] {
            let src: Vec<u8> = (0..w * h)
                .map(|i| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    if (i / 7) % 3 == 0 { 0 } else { state as u8 }
                })
                .collect();
            for sigma in [0.2, 0.3, 1.0, 2.5, 7.2, 10.75, 21.0, 43.0, 85.0] {
                assert!(
                    blur(&src, w, h, sigma) == per_pixel_blur(&src, w, h, sigma),
                    "{w}x{h} sigma {sigma}"
                );
            }
        }
    }
}
