//! Monocle — degenerate layout where every window gets the full work area.
//!
//! Used for `toggle_monocle` (i3's fullscreen analogue). The "focused" window
//! is on top and visible; the rest sit underneath at the same rect. The
//! caller is responsible for hiding all-but-one so reading the layout output
//! is uniform regardless of mode.

use crate::rect::Rect;

/// Place every leaf at `work_area`. The caller is responsible for hiding
/// all-but-one.
pub fn compute<T: Copy>(leaves: &[T], work_area: Rect, gap: i32) -> Vec<(T, Rect)> {
    let r = work_area.inset(gap);
    leaves.iter().map(|t| (*t, r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_get_same_rect() {
        let pos = compute(&[1u32, 2, 3], Rect::new(0, 0, 1000, 800), 0);
        assert_eq!(pos.len(), 3);
        for (_, r) in &pos {
            assert_eq!(*r, Rect::new(0, 0, 1000, 800));
        }
    }
}
