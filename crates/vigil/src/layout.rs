//! Pointer routing math over a horizontal left-to-right row of outputs
//! (scan order). Pure and unit-tested; the event loop feeds it deltas and
//! normalized absolute positions and gets back (output index, local coords).

use vigil_core::OutputId;

#[derive(Debug, Clone, PartialEq)]
struct Span {
    id: OutputId,
    x: f64,
    width: f64,
    height: f64,
}

/// The global layout space: outputs side by side at y=0.
#[derive(Debug, Default)]
pub struct Row {
    spans: Vec<Span>,
}

impl Row {
    /// Rebuild from `(id, width, height)` in scan order.
    pub fn rebuild(&mut self, outputs: &[(OutputId, u32, u32)]) {
        self.spans.clear();
        let mut x = 0.0;
        for &(id, w, h) in outputs {
            self.spans.push(Span {
                id,
                x,
                width: w as f64,
                height: h as f64,
            });
            x += w as f64;
        }
    }

    pub fn total_width(&self) -> f64 {
        self.spans.last().map(|s| s.x + s.width).unwrap_or(0.0)
    }

    pub fn max_height(&self) -> f64 {
        self.spans.iter().map(|s| s.height).fold(0.0, f64::max)
    }

    /// Clamp a global cursor position into the layout: x into the row span,
    /// y into the height of the output under x.
    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        if self.spans.is_empty() {
            return (0.0, 0.0);
        }
        let cx = x.clamp(0.0, self.total_width() - 1.0);
        let span = self.span_at(cx);
        (cx, y.clamp(0.0, span.height - 1.0))
    }

    /// Output index and output-local coordinates for a (clamped) position.
    pub fn locate(&self, x: f64, y: f64) -> Option<(usize, f64, f64)> {
        if self.spans.is_empty() {
            return None;
        }
        let (cx, cy) = self.clamp(x, y);
        let idx = self.index_at(cx);
        let span = &self.spans[idx];
        Some((idx, cx - span.x, cy))
    }

    /// Map normalized (0..1) coordinates over the whole span to a global
    /// position.
    pub fn denormalize(&self, nx: f64, ny: f64) -> (f64, f64) {
        (
            nx.clamp(0.0, 1.0) * self.total_width(),
            ny.clamp(0.0, 1.0) * self.max_height(),
        )
    }

    fn index_at(&self, x: f64) -> usize {
        self.spans
            .iter()
            .rposition(|s| x >= s.x)
            .unwrap_or(0)
            .min(self.spans.len() - 1)
    }

    fn span_at(&self, x: f64) -> &Span {
        &self.spans[self.index_at(x)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        let mut r = Row::default();
        r.rebuild(&[
            (OutputId(1), 1920, 1080),
            (OutputId(2), 2560, 1440),
            (OutputId(3), 1280, 800),
        ]);
        r
    }

    #[test]
    fn locates_outputs_and_local_coords() {
        let r = row();
        assert_eq!(r.total_width(), 5760.0);
        let (idx, lx, ly) = r.locate(100.0, 50.0).unwrap();
        assert_eq!((idx, lx, ly), (0, 100.0, 50.0));
        let (idx, lx, _) = r.locate(1920.0, 0.0).unwrap();
        assert_eq!((idx, lx), (1, 0.0));
        let (idx, lx, _) = r.locate(5000.0, 0.0).unwrap();
        assert_eq!((idx, lx), (2, 5000.0 - 4480.0));
    }

    #[test]
    fn clamps_x_to_row_and_y_to_local_output() {
        let r = row();
        assert_eq!(r.clamp(-50.0, -10.0), (0.0, 0.0));
        let (cx, cy) = r.clamp(99999.0, 99999.0);
        assert_eq!(cx, 5759.0);
        assert_eq!(cy, 799.0); // last output is 800 tall
        // y clamps against the output under x, not the tallest output
        let (_, cy) = r.clamp(2000.0, 99999.0);
        assert_eq!(cy, 1439.0);
    }

    #[test]
    fn normalized_maps_over_full_span() {
        let r = row();
        assert_eq!(r.denormalize(0.0, 0.0), (0.0, 0.0));
        assert_eq!(r.denormalize(1.0, 1.0), (5760.0, 1440.0));
        let (x, _) = r.denormalize(0.5, 0.0);
        assert_eq!(x, 2880.0);
    }

    #[test]
    fn empty_row_is_inert() {
        let r = Row::default();
        assert_eq!(r.total_width(), 0.0);
        assert_eq!(r.clamp(10.0, 10.0), (0.0, 0.0));
        assert!(r.locate(10.0, 10.0).is_none());
    }
}
