//! Backend-independent rectangle geometry.

/// An axis-aligned rectangle in logical (scene) pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// An empty rectangle at the origin.
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0, 0.0);

    /// Creates a rectangle from its top-left corner and size.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// The x coordinate just past the right edge.
    pub fn right(self) -> f32 {
        self.x + self.w
    }

    /// The y coordinate just past the bottom edge.
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    /// Area in square logical pixels. Empty rectangles report `0.0`.
    pub fn area(self) -> f32 {
        if self.is_empty() {
            0.0
        } else {
            self.w * self.h
        }
    }

    /// Whether the rectangle has no drawable area.
    pub fn is_empty(self) -> bool {
        !(self.w > 0.0 && self.h > 0.0)
    }

    /// Whether all coordinates are finite. Non-finite geometry is never drawable.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.w.is_finite() && self.h.is_finite()
    }

    /// The smallest rectangle covering both inputs.
    ///
    /// Empty rectangles act as the identity: they never grow the other rectangle.
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

    /// The overlapping part of two rectangles, or an empty rectangle when they do not overlap.
    pub fn intersection(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Self::new(x, y, (right - x).max(0.0), (bottom - y).max(0.0))
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
    pub fn contains_rect(self, other: Self) -> bool {
        !other.is_empty()
            && other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// Whether a point lies inside the rectangle. Edges are treated as half-open.
    pub fn contains_point(self, x: f32, y: f32) -> bool {
        !self.is_empty() && x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Moves the rectangle without changing its size.
    pub fn translated(self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

#[cfg(test)]
mod tests {
    use super::Rect;

    #[test]
    fn intersection_and_union_follow_empty_identity() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, -5.0, 10.0, 10.0);

        assert_eq!(a.intersection(b), Rect::new(5.0, 0.0, 5.0, 5.0));
        assert_eq!(a.union(b), Rect::new(0.0, -5.0, 15.0, 15.0));
        assert_eq!(a.union(Rect::ZERO), a);
        assert_eq!(a.intersection(Rect::ZERO), Rect::ZERO);
        assert!(a.contains_rect(Rect::new(1.0, 1.0, 2.0, 2.0)));
        assert!(!a.contains_point(10.0, 5.0));
    }
}
