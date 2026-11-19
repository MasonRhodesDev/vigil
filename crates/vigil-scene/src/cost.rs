use resvg::usvg::{self, Transform};

use crate::Error;

// Work is counted in nanoseconds of single-core tiny-skia time. Each rate was
// fitted to release timings of the inputs in tests/budget.rs at the largest
// edge, and errs high. Per pixel, an opaque colour measured 0.36 ns, a
// translucent one 4.3 to 7.3, and every gradient kind and spread 9 to 18
// with three stops and up to 22 with fifty.
const WORK_BUDGET: f64 = 1e9;
const MEMORY_BUDGET: f64 = (256 << 20) as f64;
const EDGE_PAIR_NS: f64 = 0.7;
const EDGE_ROW_NS: f64 = 180.0;
const STROKE_EDGES: f64 = 3.0;
const SOLID_NS: f64 = 0.5;
const ALPHA_NS: f64 = 8.0;
const GRADIENT_NS: f64 = 24.0;
const STOP_NS: f64 = 0.2;
const LAYER_NS: f64 = 20.0;
const CLIP_NS: f64 = 20.0;
const MASK_NS: f64 = 30.0;
const REACH: f64 = 16.0;

#[derive(Clone, Copy)]
struct Area {
    l: f64,
    t: f64,
    r: f64,
    b: f64,
}

impl Area {
    fn of(rect: usvg::Rect, ts: Transform) -> Area {
        let corners = [
            (rect.left(), rect.top()),
            (rect.right(), rect.top()),
            (rect.left(), rect.bottom()),
            (rect.right(), rect.bottom()),
        ]
        .map(|(x, y)| map(ts, x, y));
        let xs = corners.map(|p| p.0);
        let ys = corners.map(|p| p.1);
        let lo = |v: [f64; 4]| v.into_iter().fold(f64::INFINITY, f64::min);
        let hi = |v: [f64; 4]| v.into_iter().fold(f64::NEG_INFINITY, f64::max);
        Area {
            l: lo(xs),
            t: lo(ys),
            r: hi(xs),
            b: hi(ys),
        }
    }

    fn meet(self, o: Area) -> Area {
        Area {
            l: self.l.max(o.l),
            t: self.t.max(o.t),
            r: self.r.min(o.r),
            b: self.b.min(o.b),
        }
    }

    fn grow(self, by: f64) -> Area {
        Area {
            l: (self.l - by).floor(),
            t: (self.t - by).floor(),
            r: (self.r + by).ceil(),
            b: (self.b + by).ceil(),
        }
    }

    fn height(self) -> f64 {
        (self.b - self.t).max(0.0)
    }

    fn size(self) -> f64 {
        (self.r - self.l).max(0.0) * self.height()
    }

    fn within(self, o: Area) -> bool {
        self.l >= o.l && self.t >= o.t && self.r <= o.r && self.b <= o.b
    }
}

fn map(ts: Transform, x: f32, y: f32) -> (f64, f64) {
    let (x, y) = (f64::from(x), f64::from(y));
    (
        f64::from(ts.sx) * x + f64::from(ts.kx) * y + f64::from(ts.tx),
        f64::from(ts.ky) * x + f64::from(ts.sy) * y + f64::from(ts.ty),
    )
}

struct Edges {
    count: f64,
    rows: f64,
    subpaths: f64,
}

// Rows are what the scanline rasteriser steps through per edge. A curve is
// counted as its monotonic pieces, each spanning the curve's whole height.
fn edges(path: &usvg::tiny_skia_path::Path, ts: Transform, height: f64, pad: f64) -> Edges {
    let mut e = Edges {
        count: 0.0,
        rows: 0.0,
        subpaths: 0.0,
    };
    let y = |p: usvg::tiny_skia_path::Point| map(ts, p.x, p.y).1;
    let mut add = |weight: f64, ys: &[f64]| {
        let lo = ys.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        e.count += weight;
        e.rows += weight * (hi - lo + 1.0 + pad).min(height);
    };
    let (mut start, mut last) = (0.0, 0.0);
    let mut subpaths = 0.0;
    for seg in path.segments() {
        use usvg::tiny_skia_path::PathSegment as S;
        match seg {
            S::MoveTo(p) => {
                (start, last) = (y(p), y(p));
                subpaths += 1.0;
            }
            S::LineTo(p) => {
                add(1.0, &[last, y(p)]);
                last = y(p);
            }
            S::QuadTo(a, p) => {
                add(2.0, &[last, y(a), y(p)]);
                last = y(p);
            }
            S::CubicTo(a, b, p) => {
                add(3.0, &[last, y(a), y(b), y(p)]);
                last = y(p);
            }
            S::Close => {
                add(1.0, &[last, start]);
                last = start;
            }
        }
    }
    e.subpaths = subpaths;
    e
}

fn paint_ns(paint: &usvg::Paint, opacity: usvg::Opacity) -> Result<f64, Error> {
    match paint {
        usvg::Paint::Color(_) if opacity.get() >= 1.0 => Ok(SOLID_NS),
        usvg::Paint::Color(_) => Ok(ALPHA_NS),
        usvg::Paint::LinearGradient(g) => Ok(GRADIENT_NS + STOP_NS * g.stops().len() as f64),
        usvg::Paint::RadialGradient(g) => Ok(GRADIENT_NS + STOP_NS * g.stops().len() as f64),
        usvg::Paint::Pattern(_) => Err(Error::Refused("a pattern paint".into())),
    }
}

// The walk follows resvg's own recursion through layers, clip paths and
// masks, so every pixmap it would allocate is counted while it is alive.
struct Walk {
    work: f64,
    live: f64,
    reach: Area,
    edge: f64,
}

impl Walk {
    fn add(&mut self, ns: f64) -> Result<(), Error> {
        self.work += ns;
        if self.work.is_nan() || self.work > WORK_BUDGET {
            return Err(Error::Refused(format!(
                "drawing this SVG would take over {} ms of work",
                WORK_BUDGET / 1e6
            )));
        }
        Ok(())
    }

    fn alloc(&mut self, bytes: f64) -> Result<(), Error> {
        self.live += bytes;
        if self.live.is_nan() || self.live > MEMORY_BUDGET {
            return Err(Error::Refused(format!(
                "drawing this SVG would hold over {} MiB of layers in memory",
                MEMORY_BUDGET / f64::from(1 << 20)
            )));
        }
        Ok(())
    }

    fn free(&mut self, bytes: f64) {
        self.live -= bytes;
    }

    fn path(
        &mut self,
        p: &usvg::Path,
        ts: Transform,
        target: Area,
        stroked: bool,
    ) -> Result<(), Error> {
        if !p.is_visible() {
            return Ok(());
        }
        let stroke = p.stroke().filter(|_| stroked);
        if stroke.is_some_and(|s| s.dasharray().is_some()) {
            return Err(Error::Refused("a dashed stroke".into()));
        }
        let reach = Area::of(p.stroke_bounding_box(), ts);
        if !reach.within(self.reach) {
            return Err(Error::Refused(
                "a shape reaches far outside the logo's box".into(),
            ));
        }
        let height = target.height();
        if let Some(fill) = p.fill() {
            let e = edges(p.data(), ts, height, 0.0);
            let area = Area::of(p.bounding_box(), ts).meet(target).size();
            self.add(
                EDGE_PAIR_NS * e.count * e.count
                    + EDGE_ROW_NS * e.rows
                    + area * paint_ns(fill.paint(), fill.opacity())?,
            )?;
        }
        if let Some(stroke) = stroke {
            let rows_per_unit = f64::from(ts.ky).hypot(f64::from(ts.sy));
            let width = f64::from(stroke.width().get()) * rows_per_unit;
            let e = edges(p.data(), ts, height, width);
            let count = STROKE_EDGES * (e.count + e.subpaths);
            let rows = STROKE_EDGES * (e.rows + e.subpaths * width.min(height));
            let area = reach.meet(target).size();
            self.add(
                EDGE_PAIR_NS * count * count
                    + EDGE_ROW_NS * rows
                    + area * paint_ns(stroke.paint(), stroke.opacity())?,
            )?;
        }
        Ok(())
    }

    fn children(&mut self, g: &usvg::Group, base: Transform, target: Area) -> Result<(), Error> {
        for node in g.children() {
            match node {
                usvg::Node::Group(g) => self.group(g, base, target)?,
                usvg::Node::Path(p) => {
                    self.path(p, base.pre_concat(p.abs_transform()), target, true)?;
                }
                usvg::Node::Image(_) | usvg::Node::Text(_) => {
                    return Err(Error::Refused("an image or text in a logo".into()));
                }
            }
        }
        Ok(())
    }

    fn group(&mut self, g: &usvg::Group, base: Transform, target: Area) -> Result<(), Error> {
        if !g.should_isolate() {
            return self.children(g, base, target);
        }
        if !g.filters().is_empty() {
            return Err(Error::Refused("a filter".into()));
        }
        let ts = base.pre_concat(g.abs_transform());
        // resvg clamps a layer to five canvases around the origin of the
        // pixmap it is drawn into, which for a nested layer is its parent's.
        let e = self.edge;
        let limit = Area {
            l: target.l - 2.0 * e,
            t: target.t - 2.0 * e,
            r: target.l + 3.0 * e,
            b: target.t + 3.0 * e,
        };
        let layer = Area::of(g.layer_bounding_box().to_rect(), ts)
            .grow(2.0)
            .meet(limit);
        let size = layer.size();
        self.alloc(4.0 * size)?;
        self.add(LAYER_NS * size)?;
        self.children(g, base, layer)?;
        if let Some(c) = g.clip_path() {
            self.clip(c, ts, layer)?;
        }
        if let Some(m) = g.mask() {
            self.mask(m, ts, layer)?;
        }
        self.free(4.0 * size);
        Ok(())
    }

    fn clip(&mut self, c: &usvg::ClipPath, ts: Transform, target: Area) -> Result<(), Error> {
        let size = target.size();
        self.alloc(4.0 * size)?;
        self.add(CLIP_NS * size)?;
        self.clip_children(c.root(), ts.pre_concat(c.transform()), target)?;
        if let Some(next) = c.clip_path() {
            self.clip(next, ts, target)?;
        }
        self.alloc(size)?;
        self.free(5.0 * size);
        Ok(())
    }

    fn clip_children(
        &mut self,
        g: &usvg::Group,
        base: Transform,
        target: Area,
    ) -> Result<(), Error> {
        for node in g.children() {
            match node {
                usvg::Node::Path(p) => {
                    self.path(p, base.pre_concat(p.abs_transform()), target, false)?;
                }
                usvg::Node::Group(h) => match h.clip_path() {
                    Some(c) => {
                        let size = target.size();
                        self.alloc(4.0 * size)?;
                        self.add(LAYER_NS * size)?;
                        self.clip_children(h, base, target)?;
                        self.clip(c, base.pre_concat(h.abs_transform()), target)?;
                        self.free(4.0 * size);
                    }
                    None => self.clip_children(h, base, target)?,
                },
                usvg::Node::Image(_) | usvg::Node::Text(_) => {
                    return Err(Error::Refused("an image or text in a logo".into()));
                }
            }
        }
        Ok(())
    }

    fn mask(&mut self, m: &usvg::Mask, ts: Transform, target: Area) -> Result<(), Error> {
        let size = target.size();
        if m.root().children().is_empty() {
            return self.add(SOLID_NS * size);
        }
        self.alloc(5.0 * size)?;
        self.add(MASK_NS * size)?;
        self.children(m.root(), ts, target)?;
        self.free(size);
        if let Some(next) = m.mask() {
            self.mask(next, ts, target)?;
        }
        self.alloc(size)?;
        self.free(5.0 * size);
        Ok(())
    }
}

pub(crate) fn check(tree: &usvg::Tree, fit: Transform, edge: u32) -> Result<(), Error> {
    let e = f64::from(edge);
    let canvas = Area {
        l: 0.0,
        t: 0.0,
        r: e,
        b: e,
    };
    let mut walk = Walk {
        work: 0.0,
        live: 0.0,
        reach: Area {
            l: -REACH * e,
            t: -REACH * e,
            r: REACH * e,
            b: REACH * e,
        },
        edge: e,
    };
    walk.alloc(4.0 * canvas.size())?;
    walk.children(tree.root(), fit, canvas)
}
