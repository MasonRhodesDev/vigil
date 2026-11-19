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
