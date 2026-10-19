//! 桌面壁纸缩略图（布局界面把它画成屏幕的背景）。
//!
//! 平台实现经 [`crate::DisplayBackend::wallpapers`] 给出每块屏幕当前壁纸的缩略图：按屏幕宽高比绘制
//! （macOS 铺满；Windows 遵循系统填充方式），长边 [`LONG_EDGE`] 像素的 JPEG。这里是与平台无关的类型和尺寸计算。
//!
//! **隐私**：壁纸可能是用户的私人照片。缩略图只放在内存里（本机界面、组内设备的界面），不写日志、
//! 不进诊断数据；[`Thumbnail`] 的 `Debug` 只输出尺寸与字节数。

#[cfg(any(windows, test))]
#[path = "windows/wallpaper_geometry.rs"]
pub(crate) mod windows_geometry;

/// 缩略图长边的像素数。
pub const LONG_EDGE: u32 = 480;
/// 缩略图 JPEG 的目标上限（字节）。超过时降低压缩质量重编码。
pub const TARGET_BYTES: usize = 40 * 1024;

/// 一张缩略图（JPEG）。
#[derive(Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u16,
    pub height: u16,
    pub jpeg: Vec<u8>,
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thumbnail({}x{}, {} B)", self.width, self.height, self.jpeg.len())
    }
}

/// 一块屏幕的壁纸缩略图。拿不到静态图片的屏幕不出现在列表里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayWallpaper {
    /// [`oink_foundation::display::DisplayInfo::key`]。
    pub key: String,
    pub thumbnail: Thumbnail,
}

/// 宽高为 `w × h`（任意单位）的屏幕的缩略图像素尺寸：宽高比不变，长边 [`LONG_EDGE`]，短边至少 1。
/// 尺寸无效时为 `None`。
pub fn thumbnail_size(w: f64, h: f64) -> Option<(u32, u32)> {
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return None;
    }
    let long = f64::from(LONG_EDGE);
    let (tw, th) = if w >= h {
        (long, long * h / w)
    } else {
        (long * w / h, long)
    };
    Some(((tw.round() as u32).max(1), (th.round() as u32).max(1)))
}

/// 把 `iw × ih` 的图片铺满 `tw × th` 的画面（等比放缩、居中、超出的部分裁掉）时，图片在画面坐标里的
/// 矩形 `(x, y, w, h)`；`x`、`y` ≤ 0。
pub fn cover_rect(iw: f64, ih: f64, tw: f64, th: f64) -> (f64, f64, f64, f64) {
    let scale = (tw / iw).max(th / ih);
    let (w, h) = (iw * scale, ih * scale);
    ((tw - w) / 2.0, (th - h) / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_keeps_aspect_with_long_edge() {
        assert_eq!(thumbnail_size(1728.0, 1117.0), Some((480, 310)));
        assert_eq!(thumbnail_size(1080.0, 1920.0), Some((270, 480)));
        assert_eq!(thumbnail_size(100.0, 100.0), Some((480, 480)));
        assert_eq!(thumbnail_size(100_000.0, 1.0), Some((480, 1)));
        assert_eq!(thumbnail_size(0.0, 100.0), None);
        assert_eq!(thumbnail_size(f64::NAN, 100.0), None);
    }

    #[test]
    fn cover_centres_and_crops_the_longer_side() {
        // 图片比画面宽：高度贴合，左右各裁掉一半多出的部分。
        assert_eq!(cover_rect(800.0, 200.0, 400.0, 200.0), (-200.0, 0.0, 800.0, 200.0));
        // 图片比画面高：宽度贴合，上下裁。
        assert_eq!(cover_rect(100.0, 400.0, 200.0, 200.0), (0.0, -300.0, 200.0, 800.0));
        // 宽高比相同：正好铺满（小图放大）。
        assert_eq!(cover_rect(48.0, 30.0, 480.0, 300.0), (0.0, 0.0, 480.0, 300.0));
    }

    #[test]
    fn debug_does_not_print_image_bytes() {
        let t = Thumbnail {
            width: 2,
            height: 1,
            jpeg: vec![0xAB; 5],
        };
        assert_eq!(format!("{t:?}"), "Thumbnail(2x1, 5 B)");
    }
}
