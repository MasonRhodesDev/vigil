use std::fmt;

use vigil_core::OutputId;

const OVERLAP_SLACK: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry<'a> {
    pub id: OutputId,
    pub connector: &'a str,
    pub position: Option<(i32, i32)>,
    pub scene: (u32, u32),
    pub scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placed {
    Profile,
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
struct Span {
    id: OutputId,
    connector: String,
    placed: Placed,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
}

impl Span {
    fn sized(output: &Geometry<'_>, placed: Placed, x: f64, y: f64) -> Self {
        let scale = if output.scale.is_finite() && output.scale > 0.0 {
            output.scale
        } else {
            1.0
        };
        Self {
            id: output.id,
            connector: output.connector.to_owned(),
            placed,
            x,
            y,
            width: f64::from(output.scene.0) / scale,
            height: f64::from(output.scene.1) / scale,
            scale,
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn centre(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn nearest(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(self.x, self.x + (self.width - 1.0).max(0.0)),
            y.clamp(self.y, self.y + (self.height - 1.0).max(0.0)),
        )
    }

    fn distance2(&self, x: f64, y: f64) -> f64 {
        let nx = x.clamp(self.x, self.x + self.width);
        let ny = y.clamp(self.y, self.y + self.height);
        (x - nx).powi(2) + (y - ny).powi(2)
    }

    fn overlaps(&self, other: &Span) -> bool {
        let wide = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let tall = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        wide > OVERLAP_SLACK && tall > OVERLAP_SLACK
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub connector: String,
    pub under: String,
    pub moved: bool,
}

impl fmt::Display for Overlap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} overlaps {}", self.connector, self.under)?;
        if self.moved {
            write!(f, " (unreachable there, placed auto)")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: OutputId,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Row {
    spans: Vec<Span>,
    overlaps: Vec<Overlap>,
}

impl Row {
    pub fn place(outputs: &[Geometry<'_>]) -> Self {
        let mut row = Row::default();
        let mut auto: Vec<&Geometry<'_>> = Vec::new();
        for output in outputs {
            let Some((x, y)) = output.position else {
                auto.push(output);
                continue;
            };
            let span = Span::sized(output, Placed::Profile, f64::from(x), f64::from(y));
            let (cx, cy) = span.centre();
            if let Some(cover) = row.spans.iter().find(|placed| placed.contains(cx, cy)) {
                row.overlaps.push(Overlap {
                    connector: span.connector.clone(),
                    under: cover.connector.clone(),
                    moved: true,
                });
                auto.push(output);
                continue;
            }
            for placed in row.spans.iter().filter(|placed| placed.overlaps(&span)) {
                row.overlaps.push(Overlap {
                    connector: span.connector.clone(),
                    under: placed.connector.clone(),
                    moved: false,
                });
            }
            row.spans.push(span);
        }
        auto.sort_by(|a, b| a.connector.cmp(b.connector).then(a.id.cmp(&b.id)));
        for output in auto {
            let right = row
                .spans
                .iter()
                .map(|span| span.x + span.width)
                .fold(0.0, f64::max);
            row.spans
                .push(Span::sized(output, Placed::Auto, right.ceil(), 0.0));
        }
        row
    }

    pub fn overlaps(&self) -> &[Overlap] {
        &self.overlaps
    }

    pub fn contains(&self, id: OutputId) -> bool {
        self.span(id).is_some()
    }

    pub fn connector(&self, id: OutputId) -> Option<&str> {
        self.span(id).map(|span| span.connector.as_str())
    }

    pub fn centre(&self, id: OutputId) -> Option<(f64, f64)> {
        self.span(id).map(Span::centre)
    }

    pub fn point(&self, id: OutputId, x: f64, y: f64) -> Option<(f64, f64)> {
        self.span(id)
            .map(|span| span.nearest(span.x + x / span.scale, span.y + y / span.scale))
    }

    pub fn home(&self) -> Option<OutputId> {
        let listed = self.spans.iter().any(|span| span.placed == Placed::Profile);
        self.spans
            .iter()
            .filter(|span| !listed || span.placed == Placed::Profile)
            .min_by(|a, b| a.distance2(0.0, 0.0).total_cmp(&b.distance2(0.0, 0.0)))
            .map(|span| span.id)
    }

    fn span(&self, id: OutputId) -> Option<&Span> {
        self.spans.iter().find(|span| span.id == id)
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

    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        if self.spans.iter().any(|s| s.contains(x, y)) {
            return (x, y);
        }
        self.spans
            .iter()
            .min_by(|a, b| a.distance2(x, y).total_cmp(&b.distance2(x, y)))
            .map_or((0.0, 0.0), |span| span.nearest(x, y))
    }

    pub fn locate(&self, x: f64, y: f64) -> Option<Hit> {
        let (cx, cy) = self.clamp(x, y);
        let span = self.spans.iter().find(|s| s.contains(cx, cy))?;
        Some(Hit {
            id: span.id,
            x: (cx - span.x) * span.scale,
            y: (cy - span.y) * span.scale,
        })
    }

    pub fn denormalize(&self, nx: f64, ny: f64) -> (f64, f64) {
        if self.spans.is_empty() {
            return (0.0, 0.0);
        }
        let (x0, y0, x1, y1) = self.bounds();
        (
            x0 + nx.clamp(0.0, 1.0) * (x1 - x0),
            y0 + ny.clamp(0.0, 1.0) * (y1 - y0),
        )
    }
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.spans.is_empty() {
            return write!(f, "no outputs");
        }
        for (i, span) in self.spans.iter().enumerate() {
            if i > 0 {
                write!(f, " | ")?;
            }
            write!(
                f,
                "{} {},{} {}x{} {}",
                span.connector,
                units(span.x),
                units(span.y),
                units(span.width),
                units(span.height),
                match span.placed {
                    Placed::Profile => "profile",
                    Placed::Auto => "auto",
                }
            )?;
        }
        let (x0, y0, x1, y1) = self.bounds();
        write!(
            f,
            "; pointer box {},{} {}x{}",
            units(x0),
            units(y0),
            units(x1 - x0),
            units(y1 - y0)
        )
    }
}

fn units(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelReason {
    Origin,
    Removed,
    Pointer,
}

impl fmt::Display for PanelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PanelReason::Origin => "origin",
            PanelReason::Removed => "removed",
            PanelReason::Pointer => "pointer",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settled {
    pub panel: OutputId,
    pub cursor: (f64, f64),
    pub moved: Option<PanelReason>,
}

pub fn settle(
    old: &Row,
    new: &Row,
    panel: Option<OutputId>,
    cursor: (f64, f64),
) -> Option<Settled> {
    if let Some(panel) = panel.filter(|&id| new.contains(id)) {
        let kept = old
            .locate(cursor.0, cursor.1)
            .filter(|hit| hit.id == panel)
            .and_then(|hit| new.point(panel, hit.x, hit.y))
            .filter(|&(x, y)| new.locate(x, y).is_some_and(|hit| hit.id == panel));
        return Some(Settled {
            panel,
            cursor: kept.or_else(|| new.centre(panel))?,
            moved: None,
        });
    }
    let home = new.home()?;
    Some(Settled {
        panel: home,
        cursor: new.centre(home)?,
        moved: Some(if panel.is_some() {
            PanelReason::Removed
        } else {
            PanelReason::Origin
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(
        id: u32,
        connector: &'static str,
        position: (i32, i32),
        scene: (u32, u32),
    ) -> Geometry<'static> {
        Geometry {
            id: OutputId(id),
            connector,
            position: Some(position),
            scene,
            scale: 1.0,
        }
    }

    fn unlisted(id: u32, connector: &'static str, scene: (u32, u32)) -> Geometry<'static> {
        Geometry {
            id: OutputId(id),
            connector,
            position: None,
            scene,
            scale: 1.0,
        }
    }

    fn hit(id: u32, x: f64, y: f64) -> Option<Hit> {
        Some(Hit {
            id: OutputId(id),
            x,
            y,
        })
    }

    fn at(row: &Row, x: f64, y: f64) -> OutputId {
        row.locate(x, y).expect("an output").id
    }

    #[test]
    fn home_is_the_output_at_the_origin_not_a_neighbour_touching_it() {
        let left = Row::place(&[
            listed(1, "OUT-L", (-1920, 0), (1920, 1080)),
            listed(2, "OUT-O", (0, 0), (1920, 1080)),
        ]);
        assert_eq!(left.home(), Some(OutputId(2)));
        let above = Row::place(&[
            listed(1, "OUT-U", (0, -1080), (1920, 1080)),
            listed(2, "OUT-O", (0, 0), (1920, 1080)),
        ]);
        assert_eq!(above.home(), Some(OutputId(2)));
    }

    #[test]
    fn relative_motion_reaches_an_unlisted_output_beside_a_stacked_layout() {
        let row = Row::place(&[
            listed(1, "OUT-A", (0, 0), (1920, 1080)),
            listed(2, "OUT-B", (0, 1080), (2560, 1440)),
            unlisted(3, "OUT-Z", (1920, 1080)),
        ]);
        let (mut x, y) = (1280.0, 1800.0);
        let mut reached = false;
        for _ in 0..400 {
            let (cx, cy) = row.clamp(x + 50.0, y);
            x = cx;
            if at(&row, cx, cy) == OutputId(3) {
                reached = true;
                break;
            }
        }
        assert!(reached, "{row}");
    }

    fn row() -> Row {
        Row::place(&[
            unlisted(1, "DP-1", (1920, 1080)),
            unlisted(2, "DP-2", (2560, 1440)),
            unlisted(3, "DP-3", (1280, 800)),
        ])
    }

    fn two(first: (i32, i32), second: (i32, i32)) -> Row {
        Row::place(&[
            listed(1, "DP-1", first, (1920, 1080)),
            listed(2, "DP-2", second, (1920, 1080)),
        ])
    }

    fn profile_with_one_unlisted() -> Row {
        Row::place(&[
            listed(1, "DP-1", (0, 0), (2560, 1440)),
            listed(2, "DP-2", (2560, -360), (1080, 1920)),
            unlisted(3, "HDMI-A-1", (1920, 1080)),
        ])
    }

    #[test]
    fn outputs_without_a_profile_form_a_row() {
        let row = row();
        assert_eq!(row.locate(100.0, 50.0), hit(1, 100.0, 50.0));
        assert_eq!(row.locate(1920.0, 0.0), hit(2, 0.0, 0.0));
        assert_eq!(row.locate(5000.0, 0.0), hit(3, 5000.0 - 4480.0, 0.0));
        assert_eq!(row.denormalize(1.0, 1.0), (5760.0, 1440.0));
    }

    #[test]
    fn the_row_follows_connector_names_not_arrival_order() {
        let row = Row::place(&[
            unlisted(7, "HDMI-A-2", (1280, 800)),
            unlisted(3, "DP-1", (1920, 1080)),
            unlisted(5, "HDMI-A-1", (2560, 1440)),
        ]);
        assert_eq!(row.centre(OutputId(3)), Some((960.0, 540.0)));
        assert_eq!(row.centre(OutputId(5)), Some((1920.0 + 1280.0, 720.0)));
        assert_eq!(row.centre(OutputId(7)), Some((4480.0 + 640.0, 400.0)));
    }

    #[test]
    fn an_unlisted_output_sits_right_of_the_profile_at_y_zero() {
        let row = profile_with_one_unlisted();
        assert_eq!(row.centre(OutputId(3)), Some((3640.0 + 960.0, 540.0)));
        assert_eq!(row.locate(3640.0, 0.0), hit(3, 0.0, 0.0));
        assert_eq!(at(&row, 100.0, 100.0), OutputId(1));
    }

    #[test]
    fn two_unlisted_outputs_sit_side_by_side() {
        let row = Row::place(&[
            unlisted(4, "HDMI-A-2", (1280, 800)),
            listed(1, "DP-1", (0, 0), (2560, 1440)),
            unlisted(3, "HDMI-A-1", (1920, 1080)),
        ]);
        assert_eq!(row.locate(2560.0, 0.0), hit(3, 0.0, 0.0));
        assert_eq!(row.locate(2560.0 + 1920.0, 0.0), hit(4, 0.0, 0.0));
        assert!(row.overlaps().is_empty());
    }

    #[test]
    fn the_absolute_pointer_reaches_an_unlisted_output() {
        let row = profile_with_one_unlisted();
        let (x, y) = row.denormalize(4600.0 / 5560.0, (540.0 + 360.0) / 1920.0);
        assert_eq!(row.locate(x, y), hit(3, 960.0, 540.0));
    }

    #[test]
    fn relative_motion_to_the_right_reaches_an_unlisted_output() {
        let row = profile_with_one_unlisted();
        let mut cursor = row.centre(OutputId(1)).expect("centre");
        let mut visited = vec![at(&row, cursor.0, cursor.1)];
        for _ in 0..80 {
            cursor = row.clamp(cursor.0 + 100.0, cursor.1);
            let now = at(&row, cursor.0, cursor.1);
            if visited.last() != Some(&now) {
                visited.push(now);
            }
        }
        assert_eq!(visited, [OutputId(1), OutputId(2), OutputId(3)]);
    }

    #[test]
    fn a_rotated_scaled_output_spans_its_logical_size() {
        let mut tall = listed(1, "DP-1", (0, 0), (2160, 3840));
        tall.scale = 1.5;
        let row = Row::place(&[tall, listed(2, "DP-2", (1440, 0), (1920, 1080))]);
        assert_eq!(row.locate(1440.0, 0.0), hit(2, 0.0, 0.0));
        assert_eq!(row.locate(1439.0, 2559.0), hit(1, 2158.5, 3838.5));
        assert_eq!(row.locate(720.0, 1280.0), hit(1, 1080.0, 1920.0));
        assert!(row.overlaps().is_empty());
    }

    #[test]
    fn overlapping_profile_positions_are_reported_by_connector() {
        let row = two((0, 0), (960, 0));
        let reported: Vec<String> = row.overlaps().iter().map(ToString::to_string).collect();
        assert_eq!(reported, ["DP-2 overlaps DP-1"]);
    }

    #[test]
    fn an_output_hidden_under_another_moves_where_the_pointer_reaches_it() {
        let row = Row::place(&[
            listed(1, "DP-1", (0, 0), (2560, 1440)),
            listed(2, "DP-2", (0, 0), (1920, 1080)),
        ]);
        let (x, y) = row.centre(OutputId(2)).expect("centre");
        assert_eq!(at(&row, x, y), OutputId(2));
        assert_eq!(at(&row, 960.0, 540.0), OutputId(1));
        let reported: Vec<String> = row.overlaps().iter().map(ToString::to_string).collect();
        assert_eq!(
            reported,
            ["DP-2 overlaps DP-1 (unreachable there, placed auto)"]
        );
    }

    fn random_layouts(mut check: impl FnMut(&[Geometry<'static>], &mut dyn FnMut(u64) -> u64)) {
        const SCENES: [(u32, u32); 5] = [
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
            (1080, 1920),
            (1280, 800),
        ];
        const SCALES: [f64; 4] = [1.0, 1.25, 1.5, 2.0];
        const NAMES: [&str; 6] = ["DP-1", "DP-2", "DP-3", "HDMI-A-1", "HDMI-A-2", "eDP-1"];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for _ in 0..2000 {
            let count = 1 + next(6) as usize;
            let outputs: Vec<Geometry<'static>> = (0..count)
                .map(|i| Geometry {
                    id: OutputId(i as u32 + 1),
                    connector: NAMES[i],
                    position: (next(3) > 0)
                        .then(|| (next(6000) as i32 - 3000, next(3000) as i32 - 1500)),
                    scene: SCENES[next(5) as usize],
                    scale: SCALES[next(4) as usize],
                })
                .collect();
            check(&outputs, &mut next);
        }
    }

    #[test]
    fn every_centre_locates_to_its_own_output() {
        random_layouts(|outputs, _| {
            let row = Row::place(outputs);
            for output in outputs {
                let (x, y) = row.centre(output.id).expect("placed");
                let hit = row.locate(x, y).expect("located");
                assert_eq!(hit.id, output.id, "{row} from {outputs:?}");
                assert!(
                    hit.x < f64::from(output.scene.0) && hit.y < f64::from(output.scene.1),
                    "{hit:?} outside {output:?}"
                );
            }
        });
    }

    fn side_by_side() -> Row {
        two((0, 0), (1920, 0))
    }

    #[test]
    fn the_first_build_puts_the_panel_on_the_origin_output() {
        let row = Row::place(&[
            listed(2, "DP-2", (1920, 0), (1920, 1080)),
            listed(1, "DP-1", (0, 0), (1920, 1080)),
            unlisted(3, "DP-0", (1920, 1080)),
        ]);
        assert_eq!(
            settle(&Row::default(), &row, None, (0.0, 0.0)),
            Some(Settled {
                panel: OutputId(1),
                cursor: (960.0, 540.0),
                moved: Some(PanelReason::Origin),
            })
        );
    }

    #[test]
    fn a_rescan_keeps_the_panel_where_the_pointer_left_it() {
        let row = side_by_side();
        assert_eq!(
            settle(&row, &row, Some(OutputId(2)), (2020.0, 200.0)),
            Some(Settled {
                panel: OutputId(2),
                cursor: (2020.0, 200.0),
                moved: None,
            })
        );
    }

    #[test]
    fn the_pointer_keeps_its_spot_on_the_panel_when_the_layout_shifts() {
        let old = side_by_side();
        let new = Row::place(&[
            listed(1, "DP-1", (0, 0), (2560, 1440)),
            listed(2, "DP-2", (2560, 0), (1920, 1080)),
        ]);
        assert_eq!(
            settle(&old, &new, Some(OutputId(2)), (2020.0, 200.0)),
            Some(Settled {
                panel: OutputId(2),
                cursor: (2660.0, 200.0),
                moved: None,
            })
        );
    }

    #[test]
    fn a_removed_panel_output_falls_back_to_the_origin() {
        let old = side_by_side();
        let new = Row::place(&[listed(1, "DP-1", (0, 0), (1920, 1080))]);
        assert_eq!(
            settle(&old, &new, Some(OutputId(2)), (2020.0, 200.0)),
            Some(Settled {
                panel: OutputId(1),
                cursor: (960.0, 540.0),
                moved: Some(PanelReason::Removed),
            })
        );
    }

    #[test]
    fn without_an_origin_the_panel_starts_on_the_listed_output_nearest_it() {
        let row = Row::place(&[
            unlisted(3, "DP-0", (1920, 1080)),
            listed(2, "DP-2", (500, 500), (1920, 1080)),
            listed(1, "DP-1", (-1920, -100), (1920, 1080)),
        ]);
        let settled = settle(&Row::default(), &row, None, (0.0, 0.0)).expect("settled");
        assert_eq!(settled.panel, OutputId(1));
        assert_eq!(settled.cursor, (-960.0, 440.0));
    }

    #[test]
    fn without_a_profile_the_panel_starts_on_the_leftmost_output() {
        let row = Row::place(&[
            unlisted(2, "HDMI-A-1", (1920, 1080)),
            unlisted(1, "DP-1", (1920, 1080)),
        ]);
        let settled = settle(&Row::default(), &row, None, (0.0, 0.0)).expect("settled");
        assert_eq!(settled.panel, OutputId(1));
    }

    #[test]
    fn no_outputs_means_no_panel() {
        assert_eq!(
            settle(
                &side_by_side(),
                &Row::default(),
                Some(OutputId(1)),
                (5.0, 5.0)
            ),
            None
        );
    }

    #[test]
    fn the_pointer_is_always_on_the_panel_after_a_rebuild() {
        let mut previous: Option<(Row, OutputId)> = None;
        random_layouts(|outputs, next| {
            let new = Row::place(outputs);
            let (old, panel, cursor) = match previous.take() {
                Some((old, panel)) => {
                    let cursor = old.centre(panel).expect("centre");
                    (old, Some(panel), cursor)
                }
                None => (Row::default(), None, (0.0, 0.0)),
            };
            let settled = settle(&old, &new, panel, cursor).expect("settled");
            assert_eq!(
                new.locate(settled.cursor.0, settled.cursor.1)
                    .map(|hit| hit.id),
                Some(settled.panel),
                "{old} then {new}"
            );
            if panel.is_some_and(|id| new.contains(id)) {
                assert_eq!((Some(settled.panel), settled.moved), (panel, None));
            }
            let pick = outputs[next(outputs.len() as u64) as usize].id;
            previous = Some((new, pick));
        });
    }

    #[test]
    fn the_layout_line_names_each_span_and_the_pointer_box() {
        assert_eq!(
            profile_with_one_unlisted().to_string(),
            "DP-1 0,0 2560x1440 profile | DP-2 2560,-360 1080x1920 profile | \
             HDMI-A-1 3640,0 1920x1080 auto; pointer box 0,-360 5560x1920"
        );
        assert_eq!(Row::default().to_string(), "no outputs");
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
        assert_eq!(row.clamp(10.0, 10.0), (0.0, 0.0));
        assert!(row.locate(10.0, 10.0).is_none());
    }
}
