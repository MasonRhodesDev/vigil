use vigil_core::OutputId;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub id: OutputId,
    pub position: Option<(i32, i32)>,
    pub scene: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: OutputId,
    pub x: f64,
    pub y: f64,
}

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

    fn nearest(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(self.x, self.x + self.width - 1.0),
            y.clamp(self.y, self.y + self.height - 1.0),
        )
    }

    fn distance2(&self, x: f64, y: f64) -> f64 {
        let nx = x.clamp(self.x, self.x + self.width);
        let ny = y.clamp(self.y, self.y + self.height);
        (x - nx).powi(2) + (y - ny).powi(2)
    }
}

#[derive(Debug, Default)]
pub struct Row {
    spans: Vec<Span>,
}

impl Row {
    pub fn place(outputs: &[Geometry]) -> Self {
        let profiled = outputs.iter().any(|output| output.position.is_some());
        let mut next = 0;
        let spans = outputs
            .iter()
            .map(|output| {
                let (x, y) = if profiled {
                    output.position.unwrap_or((0, 0))
                } else {
                    (next, 0)
                };
                next += output.scene.0 as i32;
                Span {
                    id: output.id,
                    x: f64::from(x),
                    y: f64::from(y),
                    width: f64::from(output.scene.0),
                    height: f64::from(output.scene.1),
                }
            })
            .collect();
        Self { spans }
    }

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

    pub fn locate(&self, x: f64, y: f64) -> Option<Hit> {
        let (cx, cy) = self.clamp(x, y);
        let span = self
            .spans
            .iter()
            .find(|s| s.contains(cx, cy))
            .or(self.spans.first())?;
        Some(Hit {
            id: span.id,
            x: cx - span.x,
            y: cy - span.y,
        })
    }

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

    fn unlisted(id: u32, scene: (u32, u32)) -> Geometry {
        Geometry {
            id: OutputId(id),
            position: None,
            scene,
        }
    }

    fn listed(id: u32, position: (i32, i32), scene: (u32, u32)) -> Geometry {
        Geometry {
            id: OutputId(id),
            position: Some(position),
            scene,
        }
    }

    fn hit(id: u32, x: f64, y: f64) -> Option<Hit> {
        Some(Hit {
            id: OutputId(id),
            x,
            y,
        })
    }

    fn row() -> Row {
        Row::place(&[
            unlisted(1, (1920, 1080)),
            unlisted(2, (2560, 1440)),
            unlisted(3, (1280, 800)),
        ])
    }

    fn two(first: (i32, i32), second: (i32, i32)) -> Row {
        Row::place(&[
            listed(1, first, (1920, 1080)),
            listed(2, second, (1920, 1080)),
        ])
    }

    #[test]
    fn scan_order_matches_pre_profile_behavior() {
        let row = row();
        assert_eq!(row.total_width(), 5760.0);
        assert_eq!(row.locate(100.0, 50.0), hit(1, 100.0, 50.0));
        assert_eq!(row.locate(1920.0, 0.0), hit(2, 0.0, 0.0));
        assert_eq!(row.locate(5000.0, 0.0), hit(3, 5000.0 - 4480.0, 0.0));
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
        let row = two((0, 0), (3000, 0));
        assert_eq!(row.clamp(2000.0, 500.0), (1919.0, 500.0));
        assert_eq!(row.clamp(2900.0, 500.0), (3000.0, 500.0));
    }

    #[test]
    fn clamp_gap_tie_goes_to_lowest_index() {
        let row = two((0, 0), (3000, 0));
        assert_eq!(row.clamp(2460.0, 500.0), (1919.0, 500.0));
    }

    #[test]
    fn stacked_rows_locate_by_y() {
        let row = two((0, 0), (0, 1080));
        assert_eq!(row.locate(10.0, 1090.0), hit(2, 10.0, 10.0));
    }

    #[test]
    fn negative_origin_layout() {
        let row = two((-1920, 0), (0, 0));
        assert_eq!(row.total_width(), 3840.0);
        assert_eq!(row.locate(-100.0, 10.0), hit(1, 1820.0, 10.0));
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
