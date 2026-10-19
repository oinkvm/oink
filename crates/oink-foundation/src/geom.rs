//! 基本几何类型。数值单位由使用处说明（本机全局坐标、参考比例、视平面坐标等）。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

/// 轴对齐矩形，`(x, y)` 为左上角，y 向下。
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

impl Size {
    pub const fn new(w: f64, h: f64) -> Self {
        Self { w, h }
    }
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// 左闭右开：`[x, x+w) × [y, y+h)`，与光标坐标的取值习惯一致。
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// 把点限制在矩形内（右、下边界取 `right - ε`）。
    pub fn clamp(&self, p: Point) -> Point {
        const EPS: f64 = 1e-6;
        Point::new(
            p.x.clamp(self.x, self.right() - EPS),
            p.y.clamp(self.y, self.bottom() - EPS),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(!r.contains(Point::new(10.0, 5.0)));
        assert!(r.contains(r.clamp(Point::new(99.0, -3.0))));
    }
}
