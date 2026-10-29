//! 叠加层上的文字。
//!
//! 叠加层的文字是中文（与界面的「瞄准练习」页用词一致），用各平台的系统文字接口画成覆盖率图，再由共用的
//! 画面代码上色、摆放：
//!
//! - macOS：CoreText，系统界面字体（拉丁 / 数字是 SF，中文落到苹方）——与界面里的字体相同；
//! - Windows：GDI，「Microsoft YaHei UI」（微软雅黑）——与界面里的中文字体相同。
//!
//! 两个平台的字形不同（各用自己的系统界面字体，仓库里不带字体文件），但字号、位置、颜色由共用代码按毫米
//! 决定：大小按「大写字母 / 数字的高度」（cap 高）给定，在每块屏幕上按它的像素密度直接画到设备像素，
//! 所以任何缩放比例下都是清晰的。数字按等宽排（[`Pen::tabular_centered`]），计时跳动时不左右晃。
//!
//! [`StrokeText`] 是不依赖系统的后备（只有数字、拉丁字母和几个符号的线段字形）：单元测试用它保证逐像素
//! 确定，系统字体创建失败时也退到它。

use super::raster::{Canvas, Color, over};

/// 一行字的覆盖率图（每像素 0–255），`w × h`。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mask {
    pub w: usize,
    pub h: usize,
    pub alpha: Vec<u8>,
    /// 从图的顶边到「大写字母顶」的距离（像素）。摆放时按它对齐，不同字体的行高不同也对得上。
    pub cap_top: f64,
    /// 这行字的排版宽度，与它在图里的起点（图的左右各留了一点边，墨迹可能略超出排版宽度）。
    pub advance: f64,
    pub left: f64,
}

/// 把一行字画成覆盖率图。实现必须可以在任意线程调用。
pub trait TextRaster: Send + std::fmt::Debug {
    /// `cap`：大写字母 / 数字的高度（像素）；`bold`：较重的字重（数值、标题）。
    fn line(&self, text: &str, cap: f64, bold: bool) -> Mask;
    /// Optional native color rendering onto an opaque destination. `origin` is
    /// the advance's left edge and cap top. False keeps the alpha-mask path for
    /// transparent surfaces or implementations that only provide coverage.
    fn paint_opaque(
        &self,
        _canvas: &mut Canvas,
        _text: &str,
        _cap: f64,
        _bold: bool,
        _origin: (f64, f64),
        _color: Color,
    ) -> bool {
        false
    }
}

/// 线段字形的后备实现（见模块说明）。不认识的字符（包括中文）留空。
#[derive(Clone, Copy, Debug, Default)]
pub struct StrokeText;

impl TextRaster for StrokeText {
    fn line(&self, text: &str, cap: f64, _bold: bool) -> Mask {
        let pad = (cap * 0.15).ceil() + 1.0;
        let w = (super::raster::text_width(cap, text) + pad * 2.0).ceil().max(1.0) as usize;
        let h = (cap + pad * 2.0).ceil().max(1.0) as usize;
        let mut c = Canvas::new(w, h);
        c.text(pad, pad, cap, text, Color::rgb(255.0, 255.0, 255.0));
        Mask {
            w,
            h,
            alpha: c.px.iter().map(|p| (p >> 24) as u8).collect(),
            cap_top: pad,
            advance: super::raster::text_width(cap, text),
            left: pad,
        }
    }
}

/// 本机平台的系统文字；平台没有实现或字体创建失败时是 [`StrokeText`]。
pub fn system() -> Box<dyn TextRaster> {
    #[cfg(any(target_os = "macos", windows))]
    if let Some(t) = platform::SystemText::new() {
        return Box::new(t);
    }
    Box::new(StrokeText)
}

/// System UI text at the Toast's title (600) and description (450) weights.
pub fn system_toast() -> Box<dyn TextRaster> {
    #[cfg(any(target_os = "macos", windows))]
    if let Some(t) = platform::SystemText::toast() {
        return Box::new(t);
    }
    Box::new(StrokeText)
}

/// System UI text at medium (500) weight, matching dimension annotation labels.
pub fn system_medium() -> Box<dyn TextRaster> {
    #[cfg(any(target_os = "macos", windows))]
    if let Some(t) = platform::SystemText::medium() {
        return Box::new(t);
    }
    Box::new(StrokeText)
}

#[cfg(target_os = "macos")]
#[path = "text_macos.rs"]
mod platform;

#[cfg(windows)]
#[path = "text_windows.rs"]
mod platform;

fn blit(c: &mut Canvas, m: &Mask, x: f64, y_cap_top: f64, color: Color) {
    let x0 = x.round() as i64;
    let y0 = (y_cap_top - m.cap_top).round() as i64;
    for row in 0..m.h {
        let y = y0 + row as i64;
        if y < 0 || y >= c.h as i64 {
            continue;
        }
        for col in 0..m.w {
            let a = m.alpha[row * m.w + col];
            let x = x0 + col as i64;
            if a == 0 || x < 0 || x >= c.w as i64 {
                continue;
            }
            let p = &mut c.px[y as usize * c.w + x as usize];
            *p = over(*p, color, f64::from(a) / 255.0);
        }
    }
}

/// 一行字的宽度（像素）。
pub fn width(font: &dyn TextRaster, cap: f64, text: &str, bold: bool) -> f64 {
    font.line(text, cap, bold).advance
}

/// 在一块画布上用一种文字实现写字。
#[derive(Debug)]
pub struct Pen<'a> {
    canvas: &'a mut Canvas,
    font: &'a dyn TextRaster,
}

impl<'a> Pen<'a> {
    pub fn new(canvas: &'a mut Canvas, font: &'a dyn TextRaster) -> Self {
        Self { canvas, font }
    }

    /// 以 `cx` 为中线写一行字；`y` 是大写字母顶的位置。返回这行字的宽度。
    pub fn centered(&mut self, cx: f64, y: f64, cap: f64, text: &str, bold: bool, color: Color) -> f64 {
        let m = self.font.line(text, cap, bold);
        if !self
            .font
            .paint_opaque(self.canvas, text, cap, bold, (cx - m.advance / 2.0, y), color)
        {
            blit(self.canvas, &m, cx - m.advance / 2.0 - m.left, y, color);
        }
        m.advance
    }

    /// 同 [`Pen::centered`]，但数字等宽：每个数字占「0」的宽度，在自己的格子里居中（计时、得分变化时
    /// 整行不左右晃动）。其它字符按自己的宽度。
    pub fn tabular_centered(&mut self, cx: f64, y: f64, cap: f64, text: &str, bold: bool, color: Color) {
        let cell = self.font.line("0", cap, bold).advance;
        let glyphs: Vec<(Mask, f64)> = text
            .chars()
            .map(|ch| {
                let m = self.font.line(ch.encode_utf8(&mut [0; 4]), cap, bold);
                let advance = if ch.is_ascii_digit() { cell } else { m.advance };
                (m, advance)
            })
            .collect();
        let total: f64 = glyphs.iter().map(|g| g.1).sum();
        let mut pen = cx - total / 2.0;
        for (m, advance) in glyphs {
            blit(self.canvas, &m, pen + (advance - m.advance) / 2.0 - m.left, y, color);
            pen += advance;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(m: &Mask) -> usize {
        m.alpha.iter().filter(|a| **a > 0).count()
    }

    #[test]
    fn stroke_fallback_draws_digits_at_the_requested_cap_height() {
        let m = StrokeText.line("42", 20.0, false);
        assert_eq!(m.alpha.len(), m.w * m.h);
        assert!(lit(&m) > 50);
        // 有像素的行从 cap 顶附近开始，高约 20。
        let rows: Vec<usize> = (0..m.h)
            .filter(|r| m.alpha[r * m.w..(r + 1) * m.w].iter().any(|a| *a > 0))
            .collect();
        let (top, bottom) = (*rows.first().unwrap() as f64, *rows.last().unwrap() as f64 + 1.0);
        assert!((top - m.cap_top).abs() <= 2.5, "{top} {}", m.cap_top);
        assert!((bottom - top - 20.0).abs() <= 4.0, "{}", bottom - top);
    }

    #[test]
    fn tabular_digits_keep_the_line_width_and_stay_inside_the_canvas() {
        let ink = |text: &str| {
            let mut c = Canvas::new(200, 40);
            Pen::new(&mut c, &StrokeText).tabular_centered(
                100.0,
                8.0,
                16.0,
                text,
                true,
                Color::rgb(255.0, 255.0, 255.0),
            );
            let xs: Vec<usize> = (0..200).filter(|x| (0..40).any(|y| c.px[y * 200 + x] != 0)).collect();
            (*xs.first().unwrap(), *xs.last().unwrap())
        };
        // 「1」比别的数字窄，但格子一样宽：最后一位从 8 变成 1，前面的字不动。
        let (l8, _) = ink("0:48");
        let (l1, _) = ink("0:41");
        assert_eq!(l8, l1);
        let (l, r) = ink("100%");
        assert!(l > 20 && r < 180);
    }

    #[test]
    fn centered_text_is_centered_and_clipped_safely() {
        let mut c = Canvas::new(120, 30);
        let w = Pen::new(&mut c, &StrokeText).centered(60.0, 6.0, 14.0, "0:42", false, Color::rgb(255.0, 255.0, 255.0));
        assert_eq!(w, width(&StrokeText, 14.0, "0:42", false));
        let xs: Vec<usize> = (0..120).filter(|x| (0..30).any(|y| c.px[y * 120 + x] != 0)).collect();
        let mid = (*xs.first().unwrap() + *xs.last().unwrap()) as f64 / 2.0;
        assert!((mid - 60.0).abs() <= 2.0, "{mid}");
        // 画到画布外面不出错。
        Pen::new(&mut c, &StrokeText).centered(-50.0, -20.0, 40.0, "888", true, Color::rgb(1.0, 2.0, 3.0));
        Pen::new(&mut c, &StrokeText).centered(500.0, 500.0, 40.0, "888", true, Color::rgb(1.0, 2.0, 3.0));
    }

    /// 系统文字：中文要真的画出来（不是空白），字号按 cap 高缩放。只在有系统实现的平台上检查。
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn system_text_renders_chinese_at_the_requested_size() {
        let font = system();
        let small = font.line("命中率", 20.0, false);
        let big = font.line("命中率", 40.0, false);
        assert!(lit(&small) > 100, "Chinese glyphs are drawn: {}", lit(&small));
        // 三个汉字：宽约 3 个字身（字身 ≈ cap / 0.7）。
        assert!(small.w > 60 && small.w < 130, "{}", small.w);
        assert!((big.w as f64 / small.w as f64 - 2.0).abs() < 0.15);
        // 数字的高度就是 cap 高。
        let d = font.line("0", 40.0, true);
        let rows: Vec<usize> = (0..d.h)
            .filter(|r| d.alpha[r * d.w..(r + 1) * d.w].iter().any(|a| *a > 128))
            .collect();
        let (top, bottom) = (*rows.first().unwrap() as f64, *rows.last().unwrap() as f64 + 1.0);
        assert!((bottom - top - 40.0).abs() <= 4.0, "digit height {}", bottom - top);
        assert!((top - d.cap_top).abs() <= 3.0, "cap top {top} vs {}", d.cap_top);
        // 粗体比常规墨多。
        assert!(lit(&font.line("得分", 30.0, true)) >= lit(&font.line("得分", 30.0, false)));
        // 空串不出错。
        let e = font.line("", 20.0, false);
        assert_eq!(e.alpha.len(), e.w * e.h);
    }
}
