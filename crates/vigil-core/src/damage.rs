use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> u32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn contains(&self, other: &Rect) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && self.right() >= other.right()
            && self.bottom() >= other.bottom()
    }

    pub fn clipped(&self, width: u32, height: u32) -> Rect {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Rect {
            x,
            y,
            w: self.w.min(width - x),
            h: self.h.min(height - y),
        }
    }

    fn bounding(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect {
            x,
            y,
            w: self.right().max(other.right()) - x,
            h: self.bottom().max(other.bottom()) - y,
        }
    }
}

const MAX_RECTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    Full,
    Rects(Vec<Rect>),
}

impl Default for Damage {
    fn default() -> Self {
        Self::none()
    }
}

impl Damage {
    pub fn none() -> Self {
        Self::Rects(Vec::new())
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::Rects(rects) if rects.is_empty())
    }

    pub fn add(&mut self, rect: Rect) {
        let Self::Rects(rects) = self else {
            return;
        };
        if rect.is_empty() || rects.iter().any(|held| held.contains(&rect)) {
            return;
        }
        rects.retain(|held| !rect.contains(held));
        rects.push(rect);
        if rects.len() > MAX_RECTS {
            let bounds = rects
                .iter()
                .skip(1)
                .fold(rects[0], |bounds, rect| bounds.bounding(rect));
            rects.clear();
            rects.push(bounds);
        }
    }

    pub fn merge(&mut self, other: &Damage) {
        match other {
            Self::Full => *self = Self::Full,
            Self::Rects(rects) => rects.iter().for_each(|rect| self.add(*rect)),
        }
    }

    pub fn map(&self, f: impl Fn(Rect) -> Rect) -> Damage {
        match self {
            Self::Full => Self::Full,
            Self::Rects(rects) => {
                let mut mapped = Self::none();
                rects.iter().for_each(|rect| mapped.add(f(*rect)));
                mapped
            }
        }
    }

    pub fn rects(&self, width: u32, height: u32) -> Vec<Rect> {
        match self {
            Self::Full => vec![Rect::new(0, 0, width, height)],
            Self::Rects(rects) => rects
                .iter()
                .map(|rect| rect.clipped(width, height))
                .filter(|rect| !rect.is_empty())
                .collect(),
        }
    }
}

pub fn next_frame_serial() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct FrameDamage {
    stale: Damage,
    last: Option<u64>,
    report: Option<(u64, Damage)>,
}

impl FrameDamage {
    pub fn repaint(&mut self, continues: Option<u64>, frame: u64, damage: Damage) -> Damage {
        let damage = if self.last.is_some() && self.last == continues {
            damage
        } else {
            Damage::Full
        };
        let mut repaint = std::mem::take(&mut self.stale);
        repaint.merge(&damage);
        self.report = Some((frame, damage));
        repaint
    }
}

pub struct BufferDamage {
    slots: Vec<Damage>,
    last: Option<u64>,
}

impl BufferDamage {
    pub fn new(slots: usize) -> Self {
        Self {
            slots: vec![Damage::Full; slots],
            last: None,
        }
    }

    pub fn invalidate(&mut self) {
        self.slots.fill(Damage::Full);
        self.last = None;
    }

    pub fn begin(&self, slot: usize) -> FrameDamage {
        FrameDamage {
            stale: self.slots[slot].clone(),
            last: self.last,
            report: None,
        }
    }

    pub fn finish(&mut self, slot: usize, frame: FrameDamage, drew: bool) {
        match frame.report {
            Some((serial, damage)) => {
                for (index, stale) in self.slots.iter_mut().enumerate() {
                    if index == slot {
                        *stale = Damage::none();
                    } else {
                        stale.merge(&damage);
                    }
                }
                self.last = Some(serial);
            }
            None if drew => self.invalidate(),
            None => {}
        }
    }

    pub fn behind(&self, slot: usize) -> &Damage {
        &self.slots[slot]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn tracked(
        buffers: &mut BufferDamage,
        slot: usize,
        prev: Option<u64>,
        damage: Damage,
    ) -> Damage {
        let mut frame = buffers.begin(slot);
        let repaint = frame.repaint(prev, prev.map_or(1, |p| p + 1), damage);
        buffers.finish(slot, frame, true);
        repaint
    }

    #[test]
    fn a_contained_rect_adds_nothing_and_a_containing_rect_replaces() {
        let mut damage = Damage::none();
        damage.add(rect(10, 10, 20, 20));
        damage.add(rect(12, 12, 4, 4));
        assert_eq!(damage, Damage::Rects(vec![rect(10, 10, 20, 20)]));
        damage.add(rect(0, 0, 40, 40));
        assert_eq!(damage, Damage::Rects(vec![rect(0, 0, 40, 40)]));
        damage.add(rect(5, 5, 0, 9));
        assert_eq!(damage, Damage::Rects(vec![rect(0, 0, 40, 40)]));
    }

    #[test]
    fn too_many_rects_collapse_to_their_bounds() {
        let mut damage = Damage::none();
        for i in 0..=MAX_RECTS as u32 {
            damage.add(rect(i * 10, i * 3, 2, 2));
        }
        let last = MAX_RECTS as u32;
        assert_eq!(
            damage,
            Damage::Rects(vec![rect(0, 0, last * 10 + 2, last * 3 + 2)])
        );
    }

    #[test]
    fn rects_are_clipped_to_the_buffer() {
        let mut damage = Damage::none();
        damage.add(rect(90, 90, 20, 20));
        damage.add(rect(200, 0, 5, 5));
        assert_eq!(damage.rects(100, 100), vec![rect(90, 90, 10, 10)]);
        assert_eq!(Damage::Full.rects(100, 50), vec![rect(0, 0, 100, 50)]);
    }

    #[test]
    fn each_slot_repaints_what_changed_since_it_was_last_written() {
        let mut buffers = BufferDamage::new(2);
        let a = rect(0, 0, 4, 4);
        let b = rect(8, 8, 4, 4);
        let c = rect(16, 16, 4, 4);
        assert_eq!(tracked(&mut buffers, 0, None, Damage::none()), Damage::Full);
        assert_eq!(
            tracked(&mut buffers, 1, Some(1), Damage::Rects(vec![a])),
            Damage::Full
        );
        assert_eq!(
            tracked(&mut buffers, 0, Some(2), Damage::Rects(vec![b])),
            Damage::Rects(vec![a, b])
        );
        assert_eq!(
            tracked(&mut buffers, 1, Some(3), Damage::Rects(vec![c])),
            Damage::Rects(vec![b, c])
        );
        assert_eq!(buffers.behind(0), &Damage::Rects(vec![c]));
        assert_eq!(buffers.behind(1), &Damage::none());
    }

    #[test]
    fn a_frame_that_does_not_follow_the_last_tracked_one_repaints_everything() {
        let mut buffers = BufferDamage::new(2);
        tracked(&mut buffers, 0, None, Damage::none());
        tracked(&mut buffers, 1, Some(1), Damage::none());
        tracked(&mut buffers, 0, Some(2), Damage::none());
        let mut frame = buffers.begin(1);
        assert_eq!(
            frame.repaint(Some(7), 8, Damage::Rects(vec![rect(0, 0, 1, 1)])),
            Damage::Full
        );
        buffers.finish(1, frame, true);
        assert_eq!(buffers.behind(0), &Damage::Full);
    }

    #[test]
    fn a_writer_that_reports_nothing_leaves_every_slot_unknown() {
        let mut buffers = BufferDamage::new(2);
        tracked(&mut buffers, 0, None, Damage::none());
        tracked(&mut buffers, 1, Some(1), Damage::none());
        let untouched = buffers.begin(0);
        buffers.finish(0, untouched, false);
        assert_eq!(buffers.behind(0), &Damage::none());
        let untracked = buffers.begin(0);
        buffers.finish(0, untracked, true);
        assert_eq!(buffers.behind(0), &Damage::Full);
        assert_eq!(buffers.behind(1), &Damage::Full);
        assert_eq!(
            tracked(&mut buffers, 1, Some(2), Damage::none()),
            Damage::Full
        );
    }
}
