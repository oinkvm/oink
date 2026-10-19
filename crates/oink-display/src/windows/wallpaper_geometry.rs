//! Windows 壁纸的纯几何计算；在 macOS 和 Windows 上都跑测试。

use oink_foundation::display::DisplayInfo;
use oink_foundation::geom::{Point, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Position {
    Center,
    Tile,
    Stretch,
    Fit,
    Fill,
    Span,
}

/// 图片在物理桌面坐标中的矩形。Span 覆盖整个桌面；Tile 从桌面左上角重复原尺寸图片。
pub(crate) fn image_rect(position: Position, iw: f64, ih: f64, screen: Rect, desktop: Rect) -> Rect {
    let area = if matches!(position, Position::Span | Position::Tile) {
        desktop
    } else {
        screen
    };
    let (w, h) = match position {
        Position::Stretch => (area.w, area.h),
        Position::Center | Position::Tile => (iw, ih),
        Position::Fill | Position::Span | Position::Fit => {
            let scale = if position == Position::Fit {
                (area.w / iw).min(area.h / ih)
            } else {
                (area.w / iw).max(area.h / ih)
            };
            (iw * scale, ih * scale)
        }
    };
    if position == Position::Tile {
        Rect::new(area.x, area.y, w, h)
    } else {
        Rect::new(area.x + (area.w - w) / 2.0, area.y + (area.h - h) / 2.0, w, h)
    }
}

/// 桌面像素在图片中的归一化坐标；Fit / Center 的留白返回 None。
pub(crate) fn image_uv(position: Position, image: Rect, p: Point) -> Option<(f64, f64)> {
    let (x, y) = (p.x - image.x, p.y - image.y);
    if position == Position::Tile {
        Some((x.rem_euclid(image.w) / image.w, y.rem_euclid(image.h) / image.h))
    } else if image.contains(p) {
        Some((x / image.w, y / image.h))
    } else {
        None
    }
}

/// 设备接口路径大小写不敏感，但返回枚举器原有的 key。只在枚举器退回 GDI key 时用精确屏幕矩形匹配；
/// 不按列表顺序对应（系统可能列出已拔掉的显示器），也不把镜像组另一目标的 key 引入布局。
pub(crate) fn display_for_monitor<'a>(
    path: &str,
    bounds: Rect,
    displays: &'a [DisplayInfo],
) -> Option<&'a DisplayInfo> {
    displays.iter().find(|d| d.key.eq_ignore_ascii_case(path)).or_else(|| {
        displays
            .iter()
            .find(|d| d.key.starts_with(r"\\.\DISPLAY") && d.bounds == bounds)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_fit_stretch_and_center_use_screen_pixels() {
        let screen = Rect::new(-400.0, 30.0, 400.0, 200.0);
        assert_eq!(
            image_rect(Position::Fill, 800.0, 200.0, screen, screen),
            Rect::new(-600.0, 30.0, 800.0, 200.0)
        );
        let fit = image_rect(Position::Fit, 800.0, 200.0, screen, screen);
        assert_eq!(fit, Rect::new(-400.0, 80.0, 400.0, 100.0));
        assert_eq!(image_uv(Position::Fit, fit, Point::new(-300.0, 50.0)), None);
        assert_eq!(
            image_uv(Position::Fit, fit, Point::new(-200.0, 130.0)),
            Some((0.5, 0.5))
        );
        assert_eq!(image_rect(Position::Stretch, 800.0, 200.0, screen, screen), screen);
        assert_eq!(
            image_rect(Position::Center, 100.0, 50.0, screen, screen),
            Rect::new(-250.0, 105.0, 100.0, 50.0)
        );
        assert_eq!(
            image_rect(Position::Center, 800.0, 400.0, screen, screen),
            Rect::new(-600.0, -70.0, 800.0, 400.0)
        );
        let portrait = Rect::new(0.0, 0.0, 200.0, 400.0);
        assert_eq!(
            image_rect(Position::Fill, 100.0, 400.0, portrait, portrait),
            Rect::new(0.0, -200.0, 200.0, 800.0)
        );
    }

    #[test]
    fn span_shares_one_image_with_negative_origins_and_gaps() {
        let desktop = Rect::new(-400.0, -100.0, 1000.0, 400.0);
        let left = Rect::new(-400.0, -100.0, 400.0, 200.0);
        let right = Rect::new(200.0, 100.0, 400.0, 200.0);
        let image = image_rect(Position::Span, 1000.0, 400.0, left, desktop);
        assert_eq!(image, desktop);
        assert_eq!(image_rect(Position::Span, 1000.0, 400.0, right, desktop), image);
        assert_eq!(
            image_uv(Position::Span, image, Point::new(-200.0, 0.0)),
            Some((0.2, 0.25))
        );
        assert_eq!(
            image_uv(Position::Span, image, Point::new(400.0, 200.0)),
            Some((0.8, 0.75))
        );
        // 宽高比不同：在整个桌面上铺满裁剪，而不是各屏单独铺满。
        assert_eq!(
            image_rect(Position::Span, 400.0, 400.0, left, desktop),
            Rect::new(-400.0, -400.0, 1000.0, 1000.0)
        );
    }

    #[test]
    fn tiles_keep_native_size_and_phase_between_monitors() {
        let desktop = Rect::new(-350.0, -100.0, 750.0, 300.0);
        let screen = Rect::new(0.0, 0.0, 400.0, 200.0);
        let image = image_rect(Position::Tile, 100.0, 80.0, screen, desktop);
        assert_eq!(image, Rect::new(-350.0, -100.0, 100.0, 80.0));
        assert_eq!(image_uv(Position::Tile, image, Point::new(0.0, 0.0)), Some((0.5, 0.25)));
        assert_eq!(
            image_uv(Position::Tile, image, Point::new(-351.0, -101.0)),
            Some((0.99, 0.9875))
        );
    }

    #[test]
    fn monitor_mapping_preserves_keys_and_does_not_guess_by_order() {
        let screen = Rect::new(0.0, 0.0, 400.0, 200.0);
        let make = |key: &str| DisplayInfo {
            key: key.into(),
            name: String::new(),
            bounds: screen,
            pixel_width: 400,
            pixel_height: 200,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        };
        let displays = [make(r"\\?\DISPLAY#ABC#id#{guid}")];
        assert_eq!(
            display_for_monitor(r"\\?\display#abc#ID#{GUID}", screen, &displays)
                .unwrap()
                .key,
            displays[0].key
        );
        assert!(display_for_monitor("detached", screen, &displays).is_none());
        let fallback = [make(r"\\.\DISPLAY1")];
        assert_eq!(
            display_for_monitor("interface-path", screen, &fallback).unwrap().key,
            fallback[0].key
        );
        assert!(display_for_monitor("interface-path", Rect::new(0.0, 0.0, 200.0, 400.0), &fallback).is_none());
    }
}
