//! Damage tracking: which pixels are stale.
//!
//! A desktop that repaints the whole screen because a cursor moved is spending
//! memory bandwidth -- the scarcest thing on a cheap machine with no GPU
//! backend -- on pixels that did not change. A [`Damage`] collects the
//! rectangles that did, and the painter redraws those and nothing else. When it
//! is empty there is nothing to paint at all.

use crate::Rect;

/// Rectangles tracked before falling back to one bounding box.
///
/// Small on purpose. Past a handful of regions, the bookkeeping and the
/// per-region setup cost more than the pixels saved by keeping them apart.
pub const CAPACITY: usize = 8;

/// A set of stale rectangles, in fixed-size storage: no allocation.
///
/// The rectangles never overlap, so painting each one touches every stale
/// pixel exactly once.
#[derive(Clone, Copy, Debug)]
pub struct Damage {
    rects: [Rect; CAPACITY],
    len: usize,
}

impl Damage {
    pub const fn new() -> Self {
        Self {
            rects: [Rect::new(0, 0, 0, 0); CAPACITY],
            len: 0,
        }
    }

    /// Mark `rect` stale.
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }

        // Absorb everything the new rectangle overlaps. The union can reach
        // rectangles the original did not, so start over after each merge.
        let mut merged = rect;
        let mut index = 0;
        while index < self.len {
            if self.rects[index].intersect(&merged).is_some() {
                merged = merged.union(&self.rects[index]);
                self.len -= 1;
                self.rects[index] = self.rects[self.len];
                index = 0;
            } else {
                index += 1;
            }
        }

        // Out of room: one box around everything. Over-painting is always
        // correct; dropping damage never is.
        if self.len == CAPACITY {
            for other in &self.rects[..self.len] {
                merged = merged.union(other);
            }
            self.len = 0;
        }

        self.rects[self.len] = merged;
        self.len += 1;
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.len]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Pixels covered. Exact, because the rectangles are disjoint.
    pub fn area(&self) -> u64 {
        self.rects().iter().map(Rect::area).sum()
    }
}

impl Default for Damage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disjoint(damage: &Damage) -> bool {
        let rects = damage.rects();
        rects
            .iter()
            .enumerate()
            .all(|(i, a)| rects[i + 1..].iter().all(|b| a.intersect(b).is_none()))
    }

    #[test]
    fn separate_rects_stay_separate() {
        let mut damage = Damage::new();
        damage.add(Rect::new(0, 0, 10, 10));
        damage.add(Rect::new(100, 100, 10, 10));
        assert_eq!(damage.rects().len(), 2);
        assert_eq!(damage.area(), 200);
    }

    #[test]
    fn overlapping_rects_merge() {
        let mut damage = Damage::new();
        damage.add(Rect::new(0, 0, 10, 10));
        damage.add(Rect::new(5, 5, 10, 10));
        assert_eq!(damage.rects(), &[Rect::new(0, 0, 15, 15)]);
    }

    #[test]
    fn a_merge_can_cascade() {
        let mut damage = Damage::new();
        damage.add(Rect::new(0, 0, 10, 10));
        damage.add(Rect::new(30, 0, 10, 10));
        // Touches neither on its own terms until it bridges both.
        damage.add(Rect::new(5, 0, 30, 10));
        assert_eq!(damage.rects(), &[Rect::new(0, 0, 40, 10)]);
    }

    #[test]
    fn empty_rects_are_ignored() {
        let mut damage = Damage::new();
        damage.add(Rect::new(5, 5, 0, 10));
        assert!(damage.is_empty());
    }

    #[test]
    fn overflow_collapses_but_loses_nothing() {
        let mut damage = Damage::new();
        for i in 0..(CAPACITY as i32 + 3) {
            damage.add(Rect::new(i * 20, i * 20, 10, 10));
            assert!(disjoint(&damage));
        }
        assert!(damage.rects().len() <= CAPACITY);
        for i in 0..(CAPACITY as i32 + 3) {
            let rect = Rect::new(i * 20, i * 20, 10, 10);
            assert!(damage
                .rects()
                .iter()
                .any(|kept| kept.intersect(&rect) == Some(rect)));
        }
    }
}
