//! Oink UI Toast tokens: opaque pop surface, inset hairlines (no drop shadow),
//! 20px superellipse(1.75) corners with radius factor 1.45, 16px padding,
//! 30px accent icon, 12px gaps, 14/600 title and 13/450 description.
use super::{Style, Toast};
use crate::aim::{
    raster::{Canvas, Color, over},
    text::{self, Pen, TextRaster},
};

fn surface_colors(style: Style) -> (Color, Color, Color) {
    let rgb = |v: [u8; 3]| Color::rgb(v[0].into(), v[1].into(), v[2].into());
    let pop = rgb(if style.dark { [49, 47, 44] } else { [255; 3] });
    let fg = rgb(if style.dark { [244, 244, 246] } else { [27, 28, 34] });
    let line = pop.mix(
        if style.dark { rgb([255; 3]) } else { fg },
        if style.dark { 0.15 } else { 0.14 },
    );
    (pop, fg, line)
}

fn shape(c: &mut Canvas, x: f64, y: f64, w: f64, h: f64, r: f64, color: Color) {
    let power = 2_f64.powf(1.75);
    for row in y.floor().max(0.) as usize..((y + h).ceil() as usize).min(c.h) {
        for col in x.floor().max(0.) as usize..((x + w).ceil() as usize).min(c.w) {
            let mut coverage = 0.;
            for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let px = col as f64 + sx - x;
                let py = row as f64 + sy - y;
                let dx = (r - px.min(w - px)).max(0.) / r;
                let dy = (r - py.min(h - py)).max(0.) / r;
                if dx.powf(power) + dy.powf(power) <= 1. {
                    coverage += 0.25;
                }
            }
            let p = &mut c.px[row * c.w + col];
            *p = over(*p, color, coverage);
        }
    }
}
fn lines(text: &str, font: &dyn TextRaster, cap: f64, width: f64, bold: bool, max: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut line = String::new();
    for ch in text.chars() {
        let candidate = format!("{line}{ch}");
        if ch == '\n' || (!line.is_empty() && text::width(font, cap, &candidate, bold) > width) {
            result.push(std::mem::take(&mut line));
            if result.len() == max {
                break;
            }
        }
        if ch != '\n' {
            line.push(ch);
        }
    }
    if !line.is_empty() && result.len() < max {
        result.push(line);
    }
    result
}
pub(super) struct Layout {
    pub logical_width: f64,
    pub height: f64,
    body: f64,
    title: Vec<String>,
    desc: Vec<String>,
}

/// Wrap once at the monitor's resting scale. Animation must not reflow lines.
pub(super) fn layout(toast: &Toast, width: f64, scale: f64, font: &dyn TextRaster) -> Layout {
    let logical_width = width / toast.scale.max(0.5);
    let available = (logical_width - 16. * 2. - 42. - if toast.undo.is_some() { 84. } else { 0. }).max(28.);
    let title = lines(&toast.title, font, 14. * 0.72 * scale, available * scale, true, 3);
    let desc = lines(
        &toast.description,
        font,
        13. * 0.72 * scale,
        available * scale,
        false,
        4,
    );
    let body = title.len() as f64 * 20.3
        + if desc.is_empty() {
            0.
        } else {
            2. + desc.len() as f64 * 19.5
        };
    let height = 32. + body.max(32.);
    Layout {
        logical_width,
        height,
        body,
        title,
        desc,
    }
}

pub(super) fn paint(
    toast: &Toast,
    style: Style,
    layout: &Layout,
    scale: f64,
    font: &dyn TextRaster,
    button: (f64, f64),
) -> (Vec<u32>, usize, usize) {
    let logical_width = layout.logical_width;
    let height = layout.height;
    let body = layout.body;
    let title = &layout.title;
    let desc = &layout.desc;
    let w = (logical_width * scale).ceil() as usize;
    let h = (height * scale).ceil() as usize;
    let mut c = Canvas::new(w, h);
    let rgb = |v: [u8; 3]| Color::rgb(v[0].into(), v[1].into(), v[2].into());
    let (pop, fg, line) = surface_colors(style);
    let secondary = rgb(if style.dark { [174, 172, 168] } else { [95, 96, 106] });
    shape(&mut c, 0., 0., w as f64, h as f64, 29. * scale, line);
    shape(
        &mut c,
        scale,
        scale,
        w as f64 - 2. * scale,
        h as f64 - 2. * scale,
        28. * scale,
        pop,
    );
    // The prototype's inset 1.5px top highlight stays within the surface.
    let hi = pop.mix(rgb([255; 3]), if style.dark { 0.05 } else { 0.9 });
    for row in scale.ceil() as usize..(2.5 * scale).ceil() as usize {
        for col in (29. * scale).ceil() as usize..w.saturating_sub((29. * scale).ceil() as usize) {
            c.px[row * w + col] = over(c.px[row * w + col], hi, 1.);
        }
    }
    let accent = rgb(style.accent);
    let icon = super::color::accent_text(style.accent, style.dark);
    c.disc(31. * scale, height * scale / 2., 15. * scale, |_, _, _| {
        accent.mix(rgb(if style.dark { [42, 41, 39] } else { [255; 3] }), 0.84)
    });
    // The default Toast glyph, copied as four cubic Bezier segments from
    // oink-ui's 20x20 path; round 2.2px strokes in its 18px icon viewport.
    let curves = [
        [(4., 4.5), (4., 11.), (8.5, 15.5), (13., 15.5)],
        [(13., 15.5), (15.8, 15.5), (17.5, 13.5), (17.5, 11.3)],
        [(17.5, 11.3), (17.5, 9.1), (15.7, 7.5), (13.5, 7.5)],
        [(13.5, 7.5), (11.5, 7.5), (10.1, 8.9), (9.9, 10.7)],
    ];
    for points in curves {
        let at = |t: f64| {
            let u = 1. - t;
            let axis = |get: fn((f64, f64)) -> f64| {
                u.powi(3) * get(points[0])
                    + 3. * u * u * t * get(points[1])
                    + 3. * u * t * t * get(points[2])
                    + t.powi(3) * get(points[3])
            };
            (
                (22. + axis(|p| p.0) * 0.9) * scale,
                (height / 2. - 9. + axis(|p| p.1) * 0.9) * scale,
            )
        };
        for step in 0..12 {
            c.capsule(
                at(f64::from(step) / 12.),
                at(f64::from(step + 1) / 12.),
                0.99 * scale,
                icon,
            );
        }
    }
    let mut y = (height - body) / 2. + 4.;
    for line in title {
        let size = 14. * 0.72 * scale;
        let width = text::width(font, size, line, true);
        Pen::new(&mut c, font).centered(58. * scale + width / 2., y * scale, size, line, true, fg);
        y += 20.3;
    }
    if !desc.is_empty() {
        y += 2.;
    }
    for line in desc {
        let size = 13. * 0.72 * scale;
        let width = text::width(font, size, line, false);
        Pen::new(&mut c, font).centered(58. * scale + width / 2., y * scale, size, line, false, secondary);
        y += 19.5;
    }
    if let Some(label) = &toast.undo {
        let x = (logical_width - 88.) * scale;
        // Toast actions use the button's circular pill corners, independently
        // of the surrounding card's superellipse corner shape.
        let (hover, shrink) = button;
        let center = x + 36. * scale;
        let fill = rgb(if style.dark { [51, 50, 46] } else { [236, 236, 239] });
        let hover_fill = rgb(if style.dark { [65, 63, 58] } else { [224, 224, 229] });
        c.capsule(
            (center - 20. * scale * shrink, height * scale / 2.),
            (center + 20. * scale * shrink, height * scale / 2.),
            16. * scale * shrink,
            fill.mix(hover_fill, hover),
        );
        Pen::new(&mut c, font).centered(
            center,
            (height / 2. - 4.7 * shrink) * scale,
            13. * 0.72 * scale * shrink,
            label,
            true,
            fg,
        );
    }
    (c.px, w, h)
}
