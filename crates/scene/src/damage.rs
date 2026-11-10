//! Multi-rectangle damage tracking for incremental rendering.

/// One damaged pixel rectangle. Coordinates are top-left based, edges are half-open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl DamageRect {
    /// Creates a damage rectangle.
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// The x coordinate just past the right edge, saturated on overflow.
    pub fn right(self) -> u32 {
        self.x.saturating_add(self.w)
    }

    /// The y coordinate just past the bottom edge, saturated on overflow.
    pub fn bottom(self) -> u32 {
        self.y.saturating_add(self.h)
    }

    /// Whether the rectangle has no area.
    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// The smallest rectangle covering both inputs.
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(x, y, right - x, bottom - y)
    }

    /// The overlap with another rectangle, or an empty rectangle when disjoint.
    pub fn intersection(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            Self::new(x, y, 0, 0)
        } else {
            Self::new(x, y, right - x, bottom - y)
        }
    }

    /// Whether both rectangles have area and overlap.
    pub fn intersects(self, other: Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// Whether the non-empty rectangle `other` lies entirely inside `self`.
    pub fn contains(self, other: Self) -> bool {
        !other.is_empty()
            && other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    fn touches_or_overlaps(self, other: Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
}

/// A set of damaged rectangles, or a full-screen invalidation.
///
/// This deliberately is not a single rectangle: disjoint damage (for example text in one pane
/// corner and a cursor in another) is kept as separate rectangles so a renderer can scissor or
/// upload just those regions. Overlapping or touching rectangles are coalesced conservatively.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageRegion {
    /// Coalesced, non-overlapping damage rectangles. Ignored when [`Self::full`] is true.
    pub rects: Vec<DamageRect>,
    /// Invalidate the entire scene. When true, `rects` is empty by construction.
    pub full: bool,
}

impl DamageRegion {
    /// Creates an empty damage region.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates a full-screen invalidation.
    pub fn full() -> Self {
        Self {
            rects: Vec::new(),
            full: true,
        }
    }

    /// Whether nothing is damaged.
    pub fn is_empty(&self) -> bool {
        !self.full && self.rects.is_empty()
    }

    /// Whether this invalidates the entire scene.
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// Adds one damaged rectangle, coalescing it with overlapping or touching regions.
    ///
    /// Empty rectangles and additions to an already-full region are no-ops.
    pub fn push(&mut self, rect: DamageRect) {
        if self.full || rect.is_empty() {
            return;
        }
        if self.rects.iter().any(|existing| existing.contains(rect)) {
            return;
        }

        self.rects.retain(|existing| !rect.contains(*existing));
        let mut merged = rect;
        while let Some(index) = self
            .rects
            .iter()
            .position(|existing| existing.touches_or_overlaps(merged))
        {
            let other = self.rects.remove(index);
            merged = merged.union(other);
        }
        self.rects.push(merged);
    }

    /// Unions another damage region into this one.
    ///
    /// A full region on either side dominates and clears the rectangle list.
    pub fn union(&mut self, other: &Self) {
        if self.full {
            return;
        }
        if other.full {
            self.full = true;
            self.rects.clear();
            return;
        }
        for rect in &other.rects {
            self.push(*rect);
        }
    }

    /// The smallest rectangle covering all finite damage, or `None` for empty/full damage.
    ///
    /// A full invalidation has no meaningful finite bounds; consumers must check
    /// [`Self::is_full`] first.
    pub fn bounds(&self) -> Option<DamageRect> {
        if self.full {
            return None;
        }
        let mut rects = self.rects.iter().copied();
        let first = rects.next()?;
        Some(rects.fold(first, DamageRect::union))
    }

    /// Whether a rectangle is fully covered by this damage region.
    pub fn contains(&self, rect: DamageRect) -> bool {
        self.full || self.rects.iter().any(|damage| damage.contains(rect))
    }

    /// Whether a rectangle overlaps any damage.
    pub fn intersects(&self, rect: DamageRect) -> bool {
        self.full || self.rects.iter().any(|damage| damage.intersects(rect))
    }

    /// Clears all damage.
    pub fn clear(&mut self) {
        self.full = false;
        self.rects.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{DamageRect, DamageRegion};

    fn rect(x: u32, y: u32, w: u32, h: u32) -> DamageRect {
        DamageRect::new(x, y, w, h)
    }

    #[test]
    fn overlapping_damage_coalesces_into_a_union() {
        let mut damage = DamageRegion::empty();
        damage.push(rect(0, 0, 10, 10));
        damage.push(rect(5, 0, 10, 10));

        assert_eq!(damage.rects, vec![rect(0, 0, 15, 10)]);
        assert_eq!(damage.bounds(), Some(rect(0, 0, 15, 10)));
    }

    #[test]
    fn disjoint_damage_stays_multi_rect() {
        let mut damage = DamageRegion::empty();
        damage.push(rect(0, 0, 5, 5));
        damage.push(rect(20, 20, 5, 5));

        assert_eq!(damage.rects.len(), 2);
        assert_eq!(damage.bounds(), Some(rect(0, 0, 25, 25)));
        assert!(damage.contains(rect(1, 1, 2, 2)));
        assert!(!damage.contains(rect(0, 0, 25, 25)));
    }

    #[test]
    fn region_union_merges_rects_and_full_dominates() {
        let mut left = DamageRegion::empty();
        left.push(rect(0, 0, 10, 10));

        let mut right = DamageRegion::empty();
        right.push(rect(5, 0, 10, 10));
        right.push(rect(40, 40, 5, 5));

        left.union(&right);
        assert_eq!(left.rects, vec![rect(0, 0, 15, 10), rect(40, 40, 5, 5)]);

        left.union(&DamageRegion::full());
        assert!(left.is_full());
        assert!(left.rects.is_empty());
        assert!(left.contains(rect(0, 0, 1, 1)));
    }

    #[test]
    fn touching_and_contained_damage_is_not_duplicated() {
        let mut damage = DamageRegion::empty();
        damage.push(rect(0, 0, 10, 10));
        damage.push(rect(10, 0, 10, 10));
        assert_eq!(damage.rects, vec![rect(0, 0, 20, 10)]);

        damage.push(rect(2, 2, 1, 1));
        assert_eq!(damage.rects, vec![rect(0, 0, 20, 10)]);
    }
}
