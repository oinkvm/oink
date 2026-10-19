//! 小游戏画面的光栅化：抗锯齿的圆、圆环、胶囊线段，以及一套用线段描的字（数字、大写拉丁字母、几个符号）。
//!
//! 像素格式与拖尾相同：预乘 alpha 的 BGRA（`u32`，`0xAARRGGBB`）。像素 `(i, j)` 的中心在 `(i + 0.5, j + 0.5)`。
//!
//! 字不用系统字体：两个平台共用同一份光栅化，画面逐像素一致、可以离线渲染和单元测试，平台层只负责把像素
//! 放到窗口里。代价是只有数字和拉丁字母，计分栏的文字是英文。

/// 直通（非预乘）颜色：`r`、`g`、`b` 为 0–255，`a` 为 0–1。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Color {
    pub const fn rgb(r: f64, g: f64, b: f64) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn alpha(self, a: f64) -> Self {
        Self { a: self.a * a, ..self }
    }

    pub fn mix(self, o: Color, t: f64) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: self.r + (o.r - self.r) * t,
            g: self.g + (o.g - self.g) * t,
            b: self.b + (o.b - self.b) * t,
            a: self.a + (o.a - self.a) * t,
        }
    }
}

/// 把颜色 `c`（再乘覆盖率 `coverage`）叠到预乘像素 `dst` 上（source-over）。
pub fn over(dst: u32, c: Color, coverage: f64) -> u32 {
    let a = (c.a * coverage).clamp(0.0, 1.0);
    if a <= 0.0 {
        return dst;
    }
    let inv = 1.0 - a;
    let ch = |shift: u32, v: f64| -> u32 {
        let d = f64::from((dst >> shift) & 0xFF);
        ((v * a + d * inv).round().clamp(0.0, 255.0) as u32) << shift
    };
    let alpha = ((a * 255.0 + f64::from(dst >> 24) * inv).round().clamp(0.0, 255.0) as u32) << 24;
    alpha | ch(16, c.r) | ch(8, c.g) | ch(0, c.b)
}

/// 预乘像素叠预乘像素（source-over）。
pub fn over_px(dst: u32, src: u32) -> u32 {
    let a = src >> 24;
    if a == 255 {
        return src;
    }
    if a == 0 {
        return dst;
    }
    let inv = 255 - a;
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        let c = ((src >> shift) & 255) + (((dst >> shift) & 255) * inv + 127) / 255;
        out |= c.min(255) << shift;
    }
    out
}

/// 一块画布（初始全透明）。
#[derive(Clone, Debug)]
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0; w * h],
        }
    }

    /// 对范围 `[x0, x1) × [y0, y1)`（裁剪到画布内）的每个像素，按到形状的有符号距离 `sd(px, py)`
    /// （内部为负）算覆盖率并上色。
    fn shade(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, mut f: impl FnMut(f64, f64) -> Option<(Color, f64)>) {
        let xs = x0.floor().max(0.0) as usize;
        let ys = y0.floor().max(0.0) as usize;
        let xe = (x1.ceil().max(0.0) as usize).min(self.w);
        let ye = (y1.ceil().max(0.0) as usize).min(self.h);
        for y in ys..ye {
            for x in xs..xe {
                if let Some((c, sd)) = f(x as f64 + 0.5, y as f64 + 0.5) {
                    let cov = (0.5 - sd).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        let p = &mut self.px[y * self.w + x];
                        *p = over(*p, c, cov);
                    }
                }
            }
        }
    }

    /// 实心圆；`paint(d)` 给出离圆心 `d` 处的颜色。
    pub fn disc(&mut self, cx: f64, cy: f64, r: f64, paint: impl Fn(f64, f64, f64) -> Color) {
        if r <= 0.0 {
            return;
        }
        self.shade(cx - r - 1.0, cy - r - 1.0, cx + r + 1.0, cy + r + 1.0, |x, y| {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            (d < r + 0.5).then(|| (paint(x - cx, y - cy, d), d - r))
        });
    }

    /// 圆环：中心线半径 `r`，线宽 `width`。
    pub fn ring(&mut self, cx: f64, cy: f64, r: f64, width: f64, c: Color) {
        let half = width / 2.0;
        if half <= 0.0 || r <= 0.0 {
            return;
        }
        let o = r + half + 1.0;
        self.shade(cx - o, cy - o, cx + o, cy + o, |x, y| {
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            let sd = (d - r).abs() - half;
            (sd < 0.5).then_some((c, sd))
        });
    }

    /// 两端圆头的线段（胶囊），半径 `r`。
    pub fn capsule(&mut self, a: (f64, f64), b: (f64, f64), r: f64, c: Color) {
        if r <= 0.0 {
            return;
        }
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = dx * dx + dy * dy;
        self.shade(
            a.0.min(b.0) - r - 1.0,
            a.1.min(b.1) - r - 1.0,
            a.0.max(b.0) + r + 1.0,
            a.1.max(b.1) + r + 1.0,
            |x, y| {
                let t = if len2 > 0.0 {
                    (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let d = ((x - a.0 - dx * t).powi(2) + (y - a.1 - dy * t).powi(2)).sqrt();
                (d < r + 0.5).then_some((c, d - r))
            },
        );
    }

    /// 圆角矩形。
    pub fn round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, radius: f64, c: Color) {
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy, hx, hy) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        self.shade(x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0, |px, py| {
            let qx = (px - cx).abs() - hx;
            let qy = (py - cy).abs() - hy;
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let sd = outside + qx.max(qy).min(0.0) - r;
            (sd < 0.5).then_some((c, sd))
        });
    }

    /// 写一行字：左上角 `(x, y)`，大写字母高 `cap`（像素）。返回这行字的宽度。不认识的字符按空格。
    pub fn text(&mut self, x: f64, y: f64, cap: f64, s: &str, c: Color) -> f64 {
        let u = cap / GLYPH_H;
        let stroke = stroke_radius(cap);
        let mut pen = x;
        for ch in s.chars() {
            for line in glyph(ch) {
                for seg in line.windows(2) {
                    self.capsule(
                        (pen + seg[0].0 * u, y + seg[0].1 * u),
                        (pen + seg[1].0 * u, y + seg[1].1 * u),
                        stroke,
                        c,
                    );
                }
                if line.len() == 1 {
                    let p = (pen + line[0].0 * u, y + line[0].1 * u);
                    self.capsule(p, p, stroke * 1.25, c);
                }
            }
            pen += advance(ch) * u;
        }
        pen - x
    }

    /// 以 `cx` 为中线写一行字。
    pub fn text_centered(&mut self, cx: f64, y: f64, cap: f64, s: &str, c: Color) {
        let w = text_width(cap, s);
        self.text(cx - w / 2.0, y, cap, s, c);
    }
}

/// 字的格子：宽 4、高 6（y 向下，0 是大写字母的顶，6 是基线）。
const GLYPH_W: f64 = 4.0;
const GLYPH_H: f64 = 6.0;
/// 字间距（格子单位）。
const GLYPH_GAP: f64 = 1.7;

fn stroke_radius(cap: f64) -> f64 {
    (cap * 0.075).max(0.6)
}

fn advance(ch: char) -> f64 {
    match ch {
        ' ' => 3.0,
        '.' | ':' => 1.6 + GLYPH_GAP,
        '1' | 'I' => 3.0 + GLYPH_GAP,
        _ => GLYPH_W + GLYPH_GAP,
    }
}

/// 一行字的宽度（像素；不含最后一个字后面的字间距）。
pub fn text_width(cap: f64, s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let u = cap / GLYPH_H;
    (s.chars().map(advance).sum::<f64>() - GLYPH_GAP) * u
}

type Line = &'static [(f64, f64)];

/// 一个字的笔画：若干条折线（只有一个点的是一个圆点）。窄字（`1`、`I`、`.`、`:`）靠左画在更窄的格子里。
fn glyph(ch: char) -> &'static [Line] {
    const OCT: Line = &[
        (0., 1.),
        (1., 0.),
        (3., 0.),
        (4., 1.),
        (4., 5.),
        (3., 6.),
        (1., 6.),
        (0., 5.),
        (0., 1.),
    ];
    match ch.to_ascii_uppercase() {
        '0' | 'O' => &[OCT],
        '1' => &[&[(0.2, 1.3), (1.5, 0.), (1.5, 6.)], &[(0., 6.), (3., 6.)]],
        '2' => &[&[(0., 1.), (1., 0.), (3., 0.), (4., 1.), (4., 2.4), (0., 6.), (4., 6.)]],
        '3' => &[
            &[(0., 1.), (1., 0.), (3., 0.), (4., 1.), (4., 2.), (3., 3.), (1.5, 3.)],
            &[(3., 3.), (4., 4.), (4., 5.), (3., 6.), (1., 6.), (0., 5.)],
        ],
        '4' => &[&[(3., 6.), (3., 0.), (0., 4.2), (4., 4.2)]],
        '5' => &[&[
            (4., 0.),
            (0., 0.),
            (0., 2.6),
            (3., 2.6),
            (4., 3.6),
            (4., 5.),
            (3., 6.),
            (1., 6.),
            (0., 5.),
        ]],
        '6' => &[&[
            (4., 1.),
            (3., 0.),
            (1., 0.),
            (0., 1.),
            (0., 5.),
            (1., 6.),
            (3., 6.),
            (4., 5.),
            (4., 3.8),
            (3., 2.8),
            (0., 2.8),
        ]],
        '7' => &[&[(0., 0.), (4., 0.), (1.5, 6.)]],
        '8' => &[
            &[
                (1., 0.),
                (3., 0.),
                (4., 1.),
                (4., 2.),
                (3., 3.),
                (1., 3.),
                (0., 2.),
                (0., 1.),
                (1., 0.),
            ],
            &[
                (1., 3.),
                (0., 4.),
                (0., 5.),
                (1., 6.),
                (3., 6.),
                (4., 5.),
                (4., 4.),
                (3., 3.),
            ],
        ],
        '9' => &[&[
            (0., 5.),
            (1., 6.),
            (3., 6.),
            (4., 5.),
            (4., 1.),
            (3., 0.),
            (1., 0.),
            (0., 1.),
            (0., 2.2),
            (1., 3.2),
            (4., 3.2),
        ]],
        'A' => &[
            &[(0., 6.), (0., 2.), (2., 0.), (4., 2.), (4., 6.)],
            &[(0., 4.), (4., 4.)],
        ],
        'B' => &[
            &[(0., 0.), (0., 6.), (3., 6.), (4., 5.), (4., 4.), (3., 3.), (0., 3.)],
            &[(0., 0.), (3., 0.), (4., 1.), (4., 2.), (3., 3.)],
        ],
        'C' => &[&[
            (4., 1.),
            (3., 0.),
            (1., 0.),
            (0., 1.),
            (0., 5.),
            (1., 6.),
            (3., 6.),
            (4., 5.),
        ]],
        'D' => &[&[(0., 0.), (0., 6.), (2.5, 6.), (4., 4.5), (4., 1.5), (2.5, 0.), (0., 0.)]],
        'E' => &[&[(4., 0.), (0., 0.), (0., 6.), (4., 6.)], &[(0., 3.), (3., 3.)]],
        'F' => &[&[(4., 0.), (0., 0.), (0., 6.)], &[(0., 3.), (3., 3.)]],
        'G' => &[&[
            (4., 1.),
            (3., 0.),
            (1., 0.),
            (0., 1.),
            (0., 5.),
            (1., 6.),
            (3., 6.),
            (4., 5.),
            (4., 3.2),
            (2.2, 3.2),
        ]],
        'H' => &[&[(0., 0.), (0., 6.)], &[(4., 0.), (4., 6.)], &[(0., 3.), (4., 3.)]],
        'I' => &[&[(0., 0.), (3., 0.)], &[(1.5, 0.), (1.5, 6.)], &[(0., 6.), (3., 6.)]],
        'J' => &[&[(4., 0.), (4., 5.), (3., 6.), (1., 6.), (0., 5.)]],
        'K' => &[&[(0., 0.), (0., 6.)], &[(4., 0.), (0., 3.6)], &[(1.3, 2.5), (4., 6.)]],
        'L' => &[&[(0., 0.), (0., 6.), (4., 6.)]],
        'M' => &[&[(0., 6.), (0., 0.), (2., 3.), (4., 0.), (4., 6.)]],
        'N' => &[&[(0., 6.), (0., 0.), (4., 6.), (4., 0.)]],
        'P' => &[&[(0., 6.), (0., 0.), (3., 0.), (4., 1.), (4., 2.2), (3., 3.2), (0., 3.2)]],
        'Q' => &[OCT, &[(2.5, 4.5), (4., 6.)]],
        'R' => &[
            &[(0., 6.), (0., 0.), (3., 0.), (4., 1.), (4., 2.2), (3., 3.2), (0., 3.2)],
            &[(2., 3.2), (4., 6.)],
        ],
        'S' => &[&[
            (4., 1.),
            (3., 0.),
            (1., 0.),
            (0., 1.),
            (0., 2.),
            (1., 3.),
            (3., 3.),
            (4., 4.),
            (4., 5.),
            (3., 6.),
            (1., 6.),
            (0., 5.),
        ]],
        'T' => &[&[(0., 0.), (4., 0.)], &[(2., 0.), (2., 6.)]],
        'U' => &[&[(0., 0.), (0., 5.), (1., 6.), (3., 6.), (4., 5.), (4., 0.)]],
        'V' => &[&[(0., 0.), (2., 6.), (4., 0.)]],
        'W' => &[&[(0., 0.), (1., 6.), (2., 3.), (3., 6.), (4., 0.)]],
        'X' => &[&[(0., 0.), (4., 6.)], &[(4., 0.), (0., 6.)]],
        'Y' => &[&[(0., 0.), (2., 3.), (4., 0.)], &[(2., 3.), (2., 6.)]],
        'Z' => &[&[(0., 0.), (4., 0.), (0., 6.), (4., 6.)]],
        '%' => &[
            &[(0.3, 6.), (3.7, 0.)],
            &[(0., 0.2), (1.3, 0.2), (1.3, 1.7), (0., 1.7), (0., 0.2)],
            &[(2.7, 4.3), (4., 4.3), (4., 5.8), (2.7, 5.8), (2.7, 4.3)],
        ],
        '.' => &[&[(0.8, 5.9)]],
        ':' => &[&[(0.8, 1.9)], &[(0.8, 5.0)]],
        '-' => &[&[(0.6, 3.), (3.4, 3.)]],
        '+' => &[&[(0.4, 3.), (3.6, 3.)], &[(2., 1.4), (2., 4.6)]],
        '/' => &[&[(0.5, 6.), (3.5, 0.)]],
        '(' => &[&[(2.6, -0.4), (1.4, 1.2), (1.4, 4.8), (2.6, 6.4)]],
        ')' => &[&[(1.4, -0.4), (2.6, 1.2), (2.6, 4.8), (1.4, 6.4)]],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(c: &Canvas) -> usize {
        c.px.iter().filter(|p| **p >> 24 > 0).count()
    }

    fn premultiplied(c: &Canvas) -> bool {
        c.px.iter().all(|p| {
            let a = p >> 24;
            (p >> 16) & 255 <= a && (p >> 8) & 255 <= a && p & 255 <= a
        })
    }

    #[test]
    fn a_disc_is_round_antialiased_and_premultiplied() {
        let mut c = Canvas::new(40, 40);
        c.disc(20., 20., 10., |_, _, _| Color::rgb(40., 200., 220.).alpha(0.8));
        assert!(premultiplied(&c));
        // 面积约 π r²。
        let area: f64 = c.px.iter().map(|p| f64::from(p >> 24) / 255. / 0.8).sum();
        assert!((area - std::f64::consts::PI * 100.).abs() < 3.0, "{area}");
        // 外接方块的角上没有；圆心不透明度 0.8；边缘有中间值（抗锯齿）。
        assert_eq!(c.px[11 * 40 + 11], 0);
        assert_eq!(c.px[20 * 40 + 20] >> 24, 204);
        assert!(c.px.iter().any(|p| (1..200).contains(&(p >> 24))));
        // 对称。
        assert_eq!(c.px[20 * 40 + 12], c.px[20 * 40 + 27]);
        assert_eq!(c.px[12 * 40 + 20], c.px[27 * 40 + 20]);
    }

    #[test]
    fn ring_capsule_and_round_rect_cover_what_they_should() {
        let mut c = Canvas::new(60, 60);
        c.ring(30., 30., 20., 4., Color::rgb(255., 255., 255.));
        assert_eq!(c.px[30 * 60 + 30], 0, "hollow");
        assert_eq!(c.px[30 * 60 + 50] >> 24, 255, "on the ring");
        assert_eq!(c.px[30 * 60 + 56], 0, "outside");
        let mut c = Canvas::new(60, 20);
        c.capsule((10., 10.), (50., 10.), 3., Color::rgb(255., 0., 0.));
        assert_eq!(c.px[10 * 60 + 30], 0xFFFF_0000);
        assert_eq!(c.px[10 * 60 + 4], 0);
        assert!(c.px[10 * 60 + 8] >> 24 > 0, "round cap");
        assert_eq!(c.px[2 * 60 + 30], 0);
        let mut c = Canvas::new(40, 30);
        c.round_rect(5., 5., 30., 20., 6., Color::rgb(0., 0., 255.));
        assert_eq!(c.px[15 * 40 + 20], 0xFF00_00FF);
        assert_eq!(c.px[5 * 40 + 5], 0, "rounded corner");
        assert_eq!(c.px[2 * 40 + 20], 0);
        assert!(premultiplied(&c));
    }

    #[test]
    fn clipping_outside_the_canvas_is_safe() {
        let mut c = Canvas::new(10, 10);
        c.disc(-5., -5., 8., |_, _, _| Color::rgb(1., 2., 3.));
        c.disc(100., 100., 8., |_, _, _| Color::rgb(1., 2., 3.));
        c.capsule((-20., 5.), (30., 5.), 2., Color::rgb(9., 9., 9.));
        c.ring(5., 5., 40., 3., Color::rgb(9., 9., 9.));
        c.text(-30., -3., 12., "SCORE 42%", Color::rgb(255., 255., 255.));
        assert!(premultiplied(&c));
    }

    #[test]
    fn every_hud_character_has_strokes_and_text_width_matches() {
        for ch in "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ%.:-+/()".chars() {
            assert!(!glyph(ch).is_empty(), "{ch}");
            // 每个字单独画出来有像素、不超出自己的格子太多。
            let mut c = Canvas::new(40, 40);
            let w = c.text(8., 8., 24., &ch.to_string(), Color::rgb(255., 255., 255.));
            assert!(lit(&c) > 10, "{ch}");
            assert!((w - (advance(ch) * 4.0)).abs() < 1e-9);
            let right = (0..40).rev().find(|x| (0..40).any(|y| c.px[y * 40 + x] != 0)).unwrap();
            assert!(right as f64 <= 8. + advance(ch) * 4.0, "{ch} overflows: {right}");
        }
        assert!(glyph(' ').is_empty() && glyph('汉').is_empty());
        assert_eq!(glyph('a'), glyph('A'));
        // 宽度 = 各字的步进之和减去最后的字间距；居中写的字左右对称地落在中线两侧。
        let w = text_width(12., "TIME 0:42");
        let mut c = Canvas::new(200, 30);
        c.text_centered(100., 8., 12., "TIME 0:42", Color::rgb(255., 255., 255.));
        let xs: Vec<usize> = (0..200).filter(|x| (0..30).any(|y| c.px[y * 200 + x] != 0)).collect();
        let (l, r) = (*xs.first().unwrap() as f64, *xs.last().unwrap() as f64 + 1.0);
        assert!(
            (l - (100. - w / 2.)).abs() < 2.5 && (r - (100. + w / 2.)).abs() < 2.5,
            "{l} {r} {w}"
        );
        assert_eq!(text_width(12., ""), 0.0);
    }

    #[test]
    fn digits_are_distinct() {
        let render = |ch: char| {
            let mut c = Canvas::new(30, 40);
            c.text(4., 4., 30., &ch.to_string(), Color::rgb(255., 255., 255.));
            c.px
        };
        let digits: Vec<Vec<u32>> = "0123456789".chars().map(render).collect();
        for i in 0..10 {
            for j in i + 1..10 {
                let diff = digits[i].iter().zip(&digits[j]).filter(|(a, b)| a != b).count();
                assert!(diff > 40, "{i} vs {j}: {diff}");
            }
        }
    }

    #[test]
    fn source_over_matches_for_colors_and_pixels() {
        let dst = 0xFF10_2030;
        assert_eq!(over(dst, Color::rgb(255., 255., 255.), 0.0), dst);
        assert_eq!(over(dst, Color::rgb(200., 100., 50.), 1.0), 0xFFC8_6432);
        assert_eq!(over_px(dst, 0), dst);
        assert_eq!(over_px(dst, 0xFFAA_BBCC), 0xFFAA_BBCC);
        // 半透明白叠在不透明底上：各通道在两者之间，仍不透明。
        let half = over_px(dst, 0x8080_8080);
        assert_eq!(half >> 24, 255);
        assert!((half >> 16) & 255 > 0x10 && (half >> 16) & 255 < 0xFF);
    }
}
