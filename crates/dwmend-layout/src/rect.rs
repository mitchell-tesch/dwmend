//! Geometric primitives shared between the layout engine and the platform layer.
//!
//! A `Rect` here is in **virtual screen pixels** — the same coordinate space
//! the Win32 `RECT` reports. Origin is top-left; +Y goes down.

/// Axis-aligned rectangle. `w` and `h` may be negative briefly during arithmetic
/// but a valid layout result always has `w >= 0 && h >= 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    #[inline]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    #[inline]
    pub fn from_ltrb(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            x: left,
            y: top,
            w: right - left,
            h: bottom - top,
        }
    }

    #[inline]
    pub const fn left(&self) -> i32 {
        self.x
    }
    #[inline]
    pub const fn top(&self) -> i32 {
        self.y
    }
    #[inline]
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
    #[inline]
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    #[inline]
    pub const fn center_x(&self) -> i32 {
        self.x + self.w / 2
    }
    #[inline]
    pub const fn center_y(&self) -> i32 {
        self.y + self.h / 2
    }

    #[inline]
    pub const fn area(&self) -> i64 {
        self.w as i64 * self.h as i64
    }

    /// Inset this rect by `n` pixels on all sides.
    #[inline]
    pub fn inset(self, n: i32) -> Self {
        Self {
            x: self.x + n,
            y: self.y + n,
            w: (self.w - 2 * n).max(0),
            h: (self.h - 2 * n).max(0),
        }
    }

    /// Inset by (left, top, right, bottom).
    #[inline]
    pub fn inset_each(self, l: i32, t: i32, r: i32, b: i32) -> Self {
        Self {
            x: self.x + l,
            y: self.y + t,
            w: (self.w - l - r).max(0),
            h: (self.h - t - b).max(0),
        }
    }

    /// Split into (left, right) halves at `ratio` (0..=1) of the width.
    pub fn split_vertical(&self, ratio: f32) -> (Self, Self) {
        let left_w = (self.w as f32 * ratio).round() as i32;
        let left_w = left_w.clamp(0, self.w);
        (
            Self {
                x: self.x,
                y: self.y,
                w: left_w,
                h: self.h,
            },
            Self {
                x: self.x + left_w,
                y: self.y,
                w: self.w - left_w,
                h: self.h,
            },
        )
    }

    /// Split into (top, bottom) halves at `ratio` (0..=1) of the height.
    pub fn split_horizontal(&self, ratio: f32) -> (Self, Self) {
        let top_h = (self.h as f32 * ratio).round() as i32;
        let top_h = top_h.clamp(0, self.h);
        (
            Self {
                x: self.x,
                y: self.y,
                w: self.w,
                h: top_h,
            },
            Self {
                x: self.x,
                y: self.y + top_h,
                w: self.w,
                h: self.h - top_h,
            },
        )
    }

    /// Whether a point lies inside (half-open: right/bottom edges excluded).
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Clamp this rect so it lies entirely inside `container`.
    ///
    /// Used after a monitor topology change to drag floating windows whose
    /// stored absolute coordinates pointed at a now-removed monitor back
    /// into a surviving monitor's work area. Width / height are shrunk if
    /// the rect is larger than the container; the position is moved so the
    /// shrunk rect's right/bottom edges sit inside the container.
    pub fn clamp_inside(self, container: Rect) -> Rect {
        if container.w <= 0 || container.h <= 0 {
            return self;
        }
        // Width / height first — a too-large rect can't be slid into the
        // container without trimming.
        let w = self.w.min(container.w).max(0);
        let h = self.h.min(container.h).max(0);
        // Then clamp the top-left so right/bottom edges stay inside.
        let x = self.x.clamp(container.x, container.x + container.w - w);
        let y = self.y.clamp(container.y, container.y + container.h - h);
        Rect { x, y, w, h }
    }
}

/// Cardinal direction used by focus / move / resize operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub const fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Up => Self::Down,
            Self::Down => Self::Up,
        }
    }

    pub const fn is_horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// Split axis for a BSP node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    /// Horizontal split → children stacked vertically (top / bottom).
    Horizontal,
    /// Vertical split → children side by side (left / right).
    Vertical,
}

impl Axis {
    pub const fn from_direction(d: Direction) -> Self {
        if d.is_horizontal() {
            Self::Vertical
        } else {
            Self::Horizontal
        }
    }

    pub const fn flip(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_vertical_halves_width() {
        let r = Rect::new(0, 0, 1920, 1080);
        let (l, r2) = r.split_vertical(0.5);
        assert_eq!(l, Rect::new(0, 0, 960, 1080));
        assert_eq!(r2, Rect::new(960, 0, 960, 1080));
        assert_eq!(l.w + r2.w, 1920); // no pixel loss
    }

    #[test]
    fn split_horizontal_halves_height() {
        let r = Rect::new(0, 0, 1920, 1080);
        let (t, b) = r.split_horizontal(0.5);
        assert_eq!(t, Rect::new(0, 0, 1920, 540));
        assert_eq!(b, Rect::new(0, 540, 1920, 540));
        assert_eq!(t.h + b.h, 1080);
    }

    #[test]
    fn inset_clamps_to_zero() {
        let r = Rect::new(0, 0, 10, 10).inset(100);
        assert_eq!(r.w, 0);
        assert_eq!(r.h, 0);
    }

    #[test]
    fn from_ltrb_round_trip() {
        let r = Rect::from_ltrb(10, 20, 110, 220);
        assert_eq!(r, Rect::new(10, 20, 100, 200));
        assert_eq!(r.right(), 110);
        assert_eq!(r.bottom(), 220);
    }

    #[test]
    fn direction_opposites() {
        assert_eq!(Direction::Left.opposite(), Direction::Right);
        assert_eq!(Direction::Up.opposite(), Direction::Down);
    }

    #[test]
    fn clamp_inside_already_inside_is_no_op() {
        let inner = Rect::new(100, 100, 400, 300);
        let outer = Rect::new(0, 0, 1920, 1080);
        assert_eq!(inner.clamp_inside(outer), inner);
    }

    #[test]
    fn clamp_inside_off_right_edge_slides_back() {
        // 800x600 floating window remembered from a 2nd monitor at x=3440
        // dragged back to fit a single 1920-wide surviving monitor.
        let stale = Rect::new(3500, 200, 800, 600);
        let surviving = Rect::new(0, 0, 1920, 1080);
        let clamped = stale.clamp_inside(surviving);
        assert_eq!(clamped.w, 800);
        assert_eq!(clamped.h, 600);
        // Right edge sits on the container's right edge.
        assert_eq!(clamped.right(), 1920);
        assert_eq!(clamped.bottom(), 800);
    }

    #[test]
    fn clamp_inside_oversized_shrinks_then_aligns() {
        let huge = Rect::new(5000, 5000, 4000, 3000);
        let small = Rect::new(0, 0, 1280, 720);
        let clamped = huge.clamp_inside(small);
        assert_eq!(clamped, small);
    }
}
