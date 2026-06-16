//! Conversions between Win32 `RECT` and the layout-engine `Rect`.

use crate::Rect;
use windows::Win32::Foundation::RECT;

pub trait ToRect {
    fn to_rect(self) -> Rect;
}

impl ToRect for RECT {
    #[inline]
    fn to_rect(self) -> Rect {
        Rect::from_ltrb(self.left, self.top, self.right, self.bottom)
    }
}

pub trait ToWinRect {
    fn to_win_rect(self) -> RECT;
}

impl ToWinRect for Rect {
    #[inline]
    fn to_win_rect(self) -> RECT {
        RECT {
            left: self.x,
            top: self.y,
            right: self.x + self.w,
            bottom: self.y + self.h,
        }
    }
}
