//! Pointer routing over a 2D output layout. Pure and unit-tested; the event
//! loop feeds it deltas and normalized absolute positions and gets back
//! (output index, output-local coords).
//!
//! Outputs may be placed anywhere by a monitor profile, so rects can leave
//! gaps. A pointer must never be stranded in one: [`Row::clamp`] snaps a
//! position outside every rect onto the nearest one.

use vigil_core::OutputId;

#[derive(Debug, Clone, PartialEq)]
struct Span {
    id: OutputId,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Span {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Nearest point inside the rect (its own coordinates if already inside).
    fn nearest(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(self.x, self.x + self.width - 1.0),
            y.clamp(self.y, self.y + self.height - 1.0),
        )
    }

    /// Squared distance from (x, y) to this rect; 0 when inside.
    fn distance2(&self, x: f64, y: f64) -> f64 {
        // Measure to the geometric edge, while `nearest` returns a valid
        // pixel coordinate just inside that edge.
        let nx = x.clamp(self.x, self.x + self.width);
        let ny = y.clamp(self.y, self.y + self.height);
        (x - nx).powi(2) + (y - ny).powi(2)
    }
}

/// The global layout space.
#[derive(Debug, Default)]
pub struct Row {
    spans: Vec<Span>,
}

impl Row {
    /// Rebuild from `(id, x, y, width, height)`. Callers that have no
    /// profile use [`Self::rebuild_scan_order`].
    pub fn rebuild(&mut self, outputs: &[(OutputId, i32, i32, u32, u32)]) {
        self.spans = outputs
            .iter()
            .map(|&(id, x, y, w, h)| Span {
                id,
                x: f64::from(x),
                y: f64::from(y),
                width: f64::from(w),
                height: f64::from(h),
            })
            .collect();
    }

    /// Left-to-right at y=0 in the given order — the layout used when no
    /// profile applies, identical to vigil's behavior before profiles.
    pub fn rebuild_scan_order(&mut self, outputs: &[(OutputId, u32, u32)]) {
        let mut x = 0i32;
        let placed: Vec<_> = outputs
            .iter()
            .map(|&(id, w, h)| {
                let at = (id, x, 0, w, h);
                x += w as i32;
                at
            })
            .collect();
        self.rebuild(&placed);
    }

    /// Bounding box of every output, as `(min_x, min_y, max_x, max_y)`.
    fn bounds(&self) -> (f64, f64, f64, f64) {
        self.spans.iter().fold(
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
            |(x0, y0, x1, y1), s| {
                (
                    x0.min(s.x),
                    y0.min(s.y),
                    x1.max(s.x + s.width),
                    y1.max(s.y + s.height),
                )
            },
        )
    }

    pub fn total_width(&self) -> f64 {
        if self.spans.is_empty() {
            return 0.0;
        }
        let (x0, _, x1, _) = self.bounds();
        x1 - x0
    }

    pub fn max_height(&self) -> f64 {
        if self.spans.is_empty() {
            return 0.0;
        }
        let (_, y0, _, y1) = self.bounds();
        y1 - y0
    }

    /// Snap a position into the layout: unchanged when it lands on an
    /// output, else moved onto the nearest one by squared distance (ties to
    /// the lowest index, so the choice is deterministic).
    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        if self.spans.is_empty() {
            return (0.0, 0.0);
        }
        if self.spans.iter().any(|s| s.contains(x, y)) {
            return (x, y);
        }
        let best = self
            .spans
            .iter()
            .enumerate()
            .min_by(|(ai, a), (bi, b)| {
                a.distance2(x, y)
                    .total_cmp(&b.distance2(x, y))
                    .then(ai.cmp(bi))
            })
            .map(|(_, s)| s)
            .expect("non-empty");
        best.nearest(x, y)
    }

    /// Output index and output-local coordinates for a position.
    pub fn locate(&self, x: f64, y: f64) -> Option<(usize, f64, f64)> {
        if self.spans.is_empty() {
            return None;
        }
        let (cx, cy) = self.clamp(x, y);
        let idx = self
            .spans
            .iter()
            .position(|s| s.contains(cx, cy))
            .unwrap_or(0);
        let span = &self.spans[idx];
        Some((idx, cx - span.x, cy - span.y))
    }

    /// Map normalized (0..1) coordinates over the bounding box to a global
    /// position.
    pub fn denormalize(&self, nx: f64, ny: f64) -> (f64, f64) {
        if self.spans.is_empty() {
            return (0.0, 0.0);
        }
        let (x0, y0, _, _) = self.bounds();
        (
            x0 + nx.clamp(0.0, 1.0) * self.total_width(),
            y0 + ny.clamp(0.0, 1.0) * self.max_height(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        let mut row = Row::default();
        row.rebuild_scan_order(&[
            (OutputId(1), 1920, 1080),
            (OutputId(2), 2560, 1440),
            (OutputId(3), 1280, 800),
        ]);
        row
    }

    #[test]
    fn scan_order_matches_pre_profile_behavior() {
        let row = row();
        assert_eq!(row.total_width(), 5760.0);
        assert_eq!(row.locate(100.0, 50.0), Some((0, 100.0, 50.0)));
        assert_eq!(row.locate(1920.0, 0.0), Some((1, 0.0, 0.0)));
        assert_eq!(row.locate(5000.0, 0.0), Some((2, 5000.0 - 4480.0, 0.0)));
    }

    #[test]
    fn clamp_inside_is_identity() {
        assert_eq!(row().clamp(100.0, 50.0), (100.0, 50.0));
    }

    #[test]
    fn clamp_outside_snaps_to_nearest_edge() {
        let row = row();
        assert_eq!(row.clamp(-50.0, -10.0), (0.0, 0.0));
        assert_eq!(row.clamp(99999.0, 99999.0), (5759.0, 799.0));
    }

    #[test]
    fn clamp_in_a_gap_picks_nearest_rect() {
        let mut row = Row::default();
        row.rebuild(&[
            (OutputId(1), 0, 0, 1920, 1080),
            (OutputId(2), 3000, 0, 1920, 1080),
        ]);
        assert_eq!(row.clamp(2000.0, 500.0), (1919.0, 500.0));
        assert_eq!(row.clamp(2900.0, 500.0), (3000.0, 500.0));
    }

    #[test]
    fn clamp_gap_tie_goes_to_lowest_index() {
        let mut row = Row::default();
        row.rebuild(&[
            (OutputId(1), 0, 0, 1920, 1080),
            (OutputId(2), 3000, 0, 1920, 1080),
        ]);
        assert_eq!(row.clamp(2460.0, 500.0), (1919.0, 500.0));
    }

    #[test]
    fn stacked_rows_locate_by_y() {
        let mut row = Row::default();
        row.rebuild(&[
            (OutputId(1), 0, 0, 1920, 1080),
            (OutputId(2), 0, 1080, 1920, 1080),
        ]);
        assert_eq!(row.locate(10.0, 1090.0), Some((1, 10.0, 10.0)));
    }

    #[test]
    fn negative_origin_layout() {
        let mut row = Row::default();
        row.rebuild(&[
            (OutputId(1), -1920, 0, 1920, 1080),
            (OutputId(2), 0, 0, 1920, 1080),
        ]);
        assert_eq!(row.total_width(), 3840.0);
        assert_eq!(row.locate(-100.0, 10.0), Some((0, 1820.0, 10.0)));
        assert_eq!(row.denormalize(0.0, 0.0), (-1920.0, 0.0));
    }

    #[test]
    fn denormalize_spans_bounding_box() {
        let row = row();
        assert_eq!(row.denormalize(0.0, 0.0), (0.0, 0.0));
        assert_eq!(row.denormalize(1.0, 1.0), (5760.0, 1440.0));
        assert_eq!(row.denormalize(0.5, 0.0).0, 2880.0);
    }

    #[test]
    fn empty_row_is_inert() {
        let row = Row::default();
        assert_eq!(row.total_width(), 0.0);
        assert_eq!(row.clamp(10.0, 10.0), (0.0, 0.0));
        assert!(row.locate(10.0, 10.0).is_none());
    }
}
