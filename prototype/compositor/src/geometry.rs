//! Integer geometry in logical coordinates.

/// A point in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A size in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    pub const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

/// A rectangle in logical coordinates. Contains its top and left edges, not
/// its bottom and right edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn from_parts(origin: Point, size: Size) -> Self {
        Self::new(origin.x, origin.y, size.width, size.height)
    }

    pub const fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub const fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && (point.x as i64) < self.x as i64 + self.width as i64
            && (point.y as i64) < self.y as i64 + self.height as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains_top_left_but_not_bottom_right() {
        let r = Rect::new(10, 20, 30, 40);
        assert!(r.contains(Point::new(10, 20)));
        assert!(r.contains(Point::new(39, 59)));
        assert!(!r.contains(Point::new(40, 59)));
        assert!(!r.contains(Point::new(39, 60)));
        assert!(!r.contains(Point::new(9, 20)));
    }

    #[test]
    fn empty_rect_contains_nothing() {
        let r = Rect::new(0, 0, 0, 10);
        assert!(!r.contains(Point::new(0, 0)));
        assert!(r.size().is_empty());
    }
}
