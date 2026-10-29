//! Windows：用 GDI 把一行字画进内存位图。
//!
//! 字体「Microsoft YaHei UI」（微软雅黑，Windows 自带的简体中文界面字体，拉丁与数字的字形与 Segoe UI 相近）。
//! Transparent overlays use grayscale coverage. Toasts instead draw native
//! ClearType directly onto their opaque card/button fill: RGB edge coverage and
//! the system's color-dependent smoothing must not be flattened into a mask.
//! Font sizes are device pixels; neither path depends on a window/DC DPI.

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, SIZE};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    FONT_QUALITY, FW_BOLD, FW_MEDIUM, FW_NORMAL, GGO_METRICS, GLYPHMETRICS, GdiFlush, GetGlyphOutlineW,
    GetTextExtentPoint32W, GetTextMetricsW, HDC, HFONT, MAT2, OUT_TT_PRECIS, SelectObject, SetBkColor, SetBkMode,
    SetTextColor, TEXTMETRICW, TRANSPARENT, TextOutW,
};
use windows::core::w;

use super::{Mask, TextRaster};
use crate::aim::raster::{Canvas, Color};

/// 内存 DC + 选入的字体；离开作用域时还原并删除。
struct Font {
    dc: HDC,
    font: HFONT,
}

impl Font {
    fn new(bold: bool, em: f64, medium: u8) -> Option<Self> {
        Self::with_quality(bold, em, medium, ANTIALIASED_QUALITY)
    }
    fn with_quality(bold: bool, em: f64, medium: u8, quality: FONT_QUALITY) -> Option<Self> {
        // SAFETY: 标准的 GDI 对象创建；失败时释放已创建的对象。高度为负：按字身（em）的像素数。
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let font = CreateFontW(
                -(em.round().max(1.0) as i32),
                0,
                0,
                0,
                if medium == 2 {
                    if bold { 600 } else { 450 }
                } else if bold {
                    FW_BOLD.0 as i32
                } else if medium == 1 {
                    FW_MEDIUM.0 as i32
                } else {
                    FW_NORMAL.0 as i32
                },
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_TT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                quality,
                0,
                w!("Microsoft YaHei UI"),
            );
            if font.is_invalid() {
                let _ = DeleteDC(dc);
                return None;
            }
            SelectObject(dc, font.into());
            Some(Self { dc, font })
        }
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        // SAFETY: 本对象创建的 DC 与字体；删除 DC 后字体不再被选入。
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.font.into());
        }
    }
}

/// cap 高与字身的比例：量「H」的墨迹高度（按字重各算一次）。
fn cap_ratio(bold: bool, medium: u8) -> Option<f64> {
    static RATIO: [OnceLock<Option<f64>>; 6] = [const { OnceLock::new() }; 6];
    *RATIO[usize::from(bold) + usize::from(medium) * 2].get_or_init(|| {
        let f = Font::new(bold, 200.0, medium)?;
        let mut gm = GLYPHMETRICS::default();
        let identity = MAT2 {
            eM11: windows::Win32::Graphics::Gdi::FIXED { fract: 0, value: 1 },
            eM22: windows::Win32::Graphics::Gdi::FIXED { fract: 0, value: 1 },
            ..Default::default()
        };
        // SAFETY: 只取度量（缓冲区为空）；输出参数是本地变量。
        let r = unsafe { GetGlyphOutlineW(f.dc, u32::from(b'H'), GGO_METRICS, &mut gm, 0, None, &identity) };
        (r != u32::MAX && gm.gmBlackBoxY > 20).then(|| f64::from(gm.gmBlackBoxY) / 200.0)
    })
}

#[derive(Debug)]
pub(super) struct SystemText {
    medium: u8,
}

impl SystemText {
    pub(super) fn toast() -> Option<Self> {
        (cap_ratio(false, 2).is_some() && cap_ratio(true, 2).is_some()).then_some(Self { medium: 2 })
    }
    pub(super) fn medium() -> Option<Self> {
        cap_ratio(false, 1).map(|_| Self { medium: 1 })
    }
    pub(super) fn new() -> Option<Self> {
        (cap_ratio(false, 0).is_some() && cap_ratio(true, 0).is_some()).then_some(Self { medium: 0 })
    }
}

impl TextRaster for SystemText {
    fn paint_opaque(
        &self,
        canvas: &mut Canvas,
        text: &str,
        cap: f64,
        bold: bool,
        origin: (f64, f64),
        color: Color,
    ) -> bool {
        self.medium == 2
            && paint_toast(canvas, text, cap, bold, origin, color).unwrap_or_else(|| {
                tracing::warn!("native toast text drawing failed");
                false
            })
    }
    fn line(&self, text: &str, cap: f64, bold: bool) -> Mask {
        render(text, cap, bold, self.medium).unwrap_or_default()
    }
}

fn render(text: &str, cap: f64, bold: bool, medium: u8) -> Option<Mask> {
    let f = Font::new(bold, cap.max(1.0) / cap_ratio(bold, medium)?, medium)?;
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    let mut tm = TEXTMETRICW::default();
    // SAFETY: 有效的 DC；输出参数是本地变量。
    unsafe {
        if !wide.is_empty() && !GetTextExtentPoint32W(f.dc, &wide, &mut size).as_bool() {
            return None;
        }
        if !GetTextMetricsW(f.dc, &mut tm).as_bool() {
            return None;
        }
    }
    let pad = (cap * 0.08).ceil().max(1.0) as i32;
    let (w, h) = ((size.cx + pad * 2).max(1), tm.tmHeight.max(1));
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: 自上而下的 32 位 DIB，w × h；选入 DC 后画字，读完像素再选回原位图并删除。新建的 DIB 全零（黑）。
    let alpha = unsafe {
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(f.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            return None;
        }
        let old = SelectObject(f.dc, bitmap.into());
        SetBkColor(f.dc, COLORREF(0));
        SetTextColor(f.dc, COLORREF(0x00FF_FFFF));
        let _ = TextOutW(f.dc, pad, 0, &wide);
        let _ = GdiFlush();
        let px = std::slice::from_raw_parts(bits.cast::<u32>(), (w * h) as usize);
        let alpha: Vec<u8> = px.iter().map(|p| (p >> 8) as u8).collect();
        SelectObject(f.dc, old);
        let _ = DeleteObject(bitmap.into());
        alpha
    };
    Some(Mask {
        w: w as usize,
        h: h as usize,
        alpha,
        cap_top: f64::from(tm.tmAscent) - cap,
        advance: f64::from(size.cx),
        left: f64::from(pad),
    })
}

/// Keep GDI-compatible metrics/hinting and the existing CJK UI face/weights.
/// Draw onto the actual opaque colors so Windows owns ClearType's contrast and
/// RGB coverage. A white-on-black mask recolored afterward cannot reproduce it.
/// No arbitrary gamma/weight boost, font-smoothing setting change, or bitmap scale.
fn paint_toast(c: &mut Canvas, text: &str, cap: f64, bold: bool, origin: (f64, f64), color: Color) -> Option<bool> {
    if color.a != 1. || text.is_empty() {
        return Some(false);
    }
    let f = Font::with_quality(bold, cap.max(1.) / cap_ratio(bold, 2)?, 2, CLEARTYPE_QUALITY)?;
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    let mut tm = TEXTMETRICW::default();
    // SAFETY: owned memory DC; initialized output structures.
    unsafe {
        if !GetTextExtentPoint32W(f.dc, &wide, &mut size).as_bool() || !GetTextMetricsW(f.dc, &mut tm).as_bool() {
            return None;
        }
    }
    let pad = (cap * 0.08).ceil().max(1.) as i32;
    let x = origin.0.round() as i32 - pad;
    let y = (origin.1 - (f64::from(tm.tmAscent) - cap)).round() as i32;
    let w = (size.cx + pad * 2).max(1) as usize;
    let h = tm.tmHeight.max(1) as usize;
    // ClearType needs a known opaque background. Transparent overlays and any
    // clipped/rounded-edge text retain the grayscale compositor's alpha semantics.
    if x < 0 || y < 0 || x as usize + w > c.w || y as usize + h > c.h {
        return Some(false);
    }
    let (x, y) = (x as usize, y as usize);
    if (y..y + h).any(|row| c.px[row * c.w + x..row * c.w + x + w].iter().any(|p| p >> 24 != 255)) {
        return Some(false);
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: top-down 32-bit DIB of exactly w*h pixels; selected only while
    // owned. GDI writes RGB but does not preserve alpha, so restore opaque alpha.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(f.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            return None;
        }
        let old = SelectObject(f.dc, bitmap.into());
        for row in 0..h {
            std::ptr::copy_nonoverlapping(
                c.px.as_ptr().add((y + row) * c.w + x),
                bits.cast::<u32>().add(row * w),
                w,
            );
        }
        SetBkMode(f.dc, TRANSPARENT);
        SetTextColor(
            f.dc,
            COLORREF(color.r.round() as u32 | (color.g.round() as u32) << 8 | (color.b.round() as u32) << 16),
        );
        let drawn = TextOutW(f.dc, pad, 0, &wide).as_bool();
        let flushed = GdiFlush().as_bool();
        if drawn && flushed {
            let pixels = std::slice::from_raw_parts(bits.cast::<u32>(), w * h);
            for row in 0..h {
                for col in 0..w {
                    c.px[(y + row) * c.w + x + col] = 0xff000000 | (pixels[row * w + col] & 0xffffff);
                }
            }
        }
        SelectObject(f.dc, old);
        let _ = DeleteObject(bitmap.into());
        (drawn && flushed).then_some(true)
    }
}

#[cfg(test)]
mod toast_tests {
    use super::*;
    use crate::aim::{
        raster::{Canvas, Color},
        text::Pen,
    };

    #[test]
    fn toast_uses_native_subpixel_color_on_opaque_fill() {
        let font = SystemText::toast().unwrap();
        let mut c = Canvas::new(500, 100);
        c.px.fill(0xffffffff);
        Pen::new(&mut c, &font).centered(
            250.,
            25.,
            20.16,
            "已禁用本机键盘 Ctrl+Alt",
            true,
            Color::rgb(0., 0., 0.),
        );
        assert!(
            c.px.iter().any(|p| ((p >> 16) & 255) != (p & 255)),
            "native ClearType RGB edges must survive; a grayscale coverage mask loses them"
        );
        assert!(c.px.iter().all(|p| p >> 24 == 255), "GDI must not erase card alpha");
    }
}

#[cfg(test)]
mod opaque_tests {
    use super::*;
    #[test]
    fn native_text_never_overwrites_transparent_pixels() {
        let mut c = Canvas::new(200, 80);
        c.px.fill(0x80402010);
        let before = c.px.clone();
        assert_eq!(
            paint_toast(&mut c, "撤销", 10.08, true, (30., 25.), Color::rgb(0., 0., 0.)),
            Some(false)
        );
        assert_eq!(c.px, before);
        c.px.fill(0xffffffff);
        let before = c.px.clone();
        assert_eq!(
            paint_toast(&mut c, "撤销", 10.08, true, (-20., 25.), Color::rgb(0., 0., 0.)),
            Some(false)
        );
        assert_eq!(c.px, before);
    }
}
