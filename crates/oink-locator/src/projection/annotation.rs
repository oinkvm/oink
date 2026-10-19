//! Engineering dimension annotations, in UI points before native pixel scaling.
use super::*;
use oink_display::layout::Side;

#[derive(Debug)]
struct Annotation {
    a: f64,
    b: f64,
    d: f64,
    tight: bool,
    tag: Rect,
    bounds: Rect,
    label: String,
}

fn annotation(named: &NamedProjection, font: &dyn TextRaster, unit: f64) -> Annotation {
    let p = &named.geometry;
    let screen = named.screen;
    let vertical = p.side.is_vertical();
    let (w, h) = (screen.w / unit, screen.h / unit);
    let (a, b) = if vertical {
        ((p.clip.y - screen.y) / unit, (p.clip.bottom() - screen.y) / unit)
    } else {
        ((p.clip.x - screen.x) / unit, (p.clip.right() - screen.x) / unit)
    };
    let tight = b - a < 44.0;
    let d = match p.side {
        Side::Top if cfg!(target_os = "macos") => 38.0,
        Side::Bottom if cfg!(windows) => 62.0,
        _ => 18.0,
    };
    let cap = 11.5 * 0.73 * named.pixels_per_unit * unit;
    let room = 220.0_f64.min((w - 16.0).max(1.0));
    let mut label = named.name.clone();
    if text::width(font, cap, &label, false) / (named.pixels_per_unit * unit) + 18.0 > room {
        while !label.is_empty()
            && text::width(font, cap, &format!("{label}…"), false) / (named.pixels_per_unit * unit) + 18.0 > room
        {
            label.pop();
        }
        label.push('…');
    }
    let tw = (text::width(font, cap, &label, false) / (named.pixels_per_unit * unit) + 18.0).min(room);
    let center = (a + b) / 2.0;
    let clamp = |v: f64, size: f64, full: f64| {
        v.clamp(
            8.0_f64.min((full - size).max(0.0) / 2.0),
            (full - size - 8.0).max(8.0_f64.min((full - size).max(0.0) / 2.0)),
        )
    };
    let tag = match p.side {
        Side::Top => Rect::new(
            clamp(center - tw / 2.0, tw, w),
            if tight { d + 9.0 } else { d - 11.0 },
            tw,
            22.0,
        ),
        Side::Bottom => Rect::new(
            clamp(center - tw / 2.0, tw, w),
            if tight { h - d - 31.0 } else { h - d - 11.0 },
            tw,
            22.0,
        ),
        Side::Left => Rect::new(d + 9.0, clamp(center - 11.0, 22.0, h), tw, 22.0),
        Side::Right => Rect::new(w - d - 9.0 - tw, clamp(center - 11.0, 22.0, h), tw, 22.0),
    };
    let tag = Rect::new(
        tag.x.clamp(0.0, (w - tag.w).max(0.0)),
        tag.y.clamp(0.0, (h - tag.h).max(0.0)),
        tag.w,
        tag.h.min(h),
    );
    let lo = (a - if tight { 18.0 } else { 1.0 }).max(0.0);
    let hi = (b + if tight { 18.0 } else { 1.0 }).min(if vertical { h } else { w });
    let edge_bounds = match p.side {
        Side::Top => Rect::new(lo, 0.0, hi - lo, d + 18.0),
        Side::Bottom => Rect::new(lo, h - d - 18.0, hi - lo, d + 18.0),
        Side::Left => Rect::new(0.0, lo, d + 18.0, hi - lo),
        Side::Right => Rect::new(w - d - 18.0, lo, d + 18.0, hi - lo),
    };
    let x = edge_bounds.x.min((tag.x - 12.0).max(0.0));
    let y = edge_bounds.y.min((tag.y - 12.0).max(0.0));
    let bounds = Rect::new(
        x,
        y,
        edge_bounds.right().max((tag.right() + 12.0).min(w)) - x,
        edge_bounds.bottom().max((tag.bottom() + 14.0).min(h)) - y,
    );
    Annotation {
        a,
        b,
        d,
        tight,
        tag,
        bounds,
        label,
    }
}

/// The UI's on-accent token chooses white below OKLab L=.65, dark ink above it.
fn on_accent(rgb: [u8; 3]) -> Color {
    let [r, g, b] = rgb.map(|v| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    });
    let l = 0.2104542553 * (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt()
        + 0.793617785 * (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt()
        - 0.0040720468 * (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    if l < 0.65 {
        Color::rgb(255.0, 255.0, 255.0)
    } else {
        Color::rgb(27.0, 28.0, 34.0)
    }
}

pub(super) fn raster(named: &NamedProjection, rgb: [u8; 3], font: &dyn TextRaster) -> Option<Frame> {
    let scale = named.pixels_per_unit;
    if !scale.is_finite() || !(0.5..=4.0).contains(&scale) {
        return None;
    }
    let unit = platform::annotation_unit(named.screen);
    let a = annotation(named, font, unit);
    let screen = named.screen;
    let x0 = ((screen.x + a.bounds.x * unit) * scale).floor() / scale;
    let y0 = ((screen.y + a.bounds.y * unit) * scale).floor() / scale;
    let w = (((screen.x + a.bounds.right() * unit) - x0) * scale).ceil() as usize;
    let h = (((screen.y + a.bounds.bottom() * unit) - y0) * scale).ceil() as usize;
    if w == 0 || h == 0 || w.checked_mul(h)? > 32_000_000 {
        return None;
    }
    let px = scale * unit;
    let offset = ((screen.x - x0) * scale, (screen.y - y0) * scale);
    let color = Color::rgb(rgb[0].into(), rgb[1].into(), rgb[2].into());
    let side = named.geometry.side;
    let (sw, sh) = (screen.w / unit, screen.h / unit);
    let point = |t: f64, d: f64| {
        let (x, y) = match side {
            Side::Top => (t, d),
            Side::Bottom => (t, sh - d),
            Side::Left => (d, t),
            Side::Right => (sw - d, t),
        };
        (x * px + offset.0, y * px + offset.1)
    };
    let mut c = Canvas::new(w, h);
    // Paint only the narrow wash band, not the label's entire bounding window.
    let (g0, g1) = (point(a.a, 0.0), point(a.b, a.d + 18.0));
    let (gx0, gx1) = (
        g0.0.min(g1.0).floor().max(0.0) as usize,
        (g0.0.max(g1.0).ceil() as usize).min(w),
    );
    let (gy0, gy1) = (
        g0.1.min(g1.1).floor().max(0.0) as usize,
        (g0.1.max(g1.1).ceil() as usize).min(h),
    );
    for y in gy0..gy1 {
        for x in gx0..gx1 {
            let (xx, yy) = ((x as f64 + 0.5 - offset.0) / px, (y as f64 + 0.5 - offset.1) / px);
            let d = match side {
                Side::Top => yy,
                Side::Bottom => sh - yy,
                Side::Left => xx,
                Side::Right => sw - xx,
            };
            c.px[y * w + x] = over(0, color.alpha(0.16), (1.0 - d / (a.d + 18.0)).clamp(0.0, 1.0));
        }
    }
    let (e0, e1) = (point(a.a, 0.0), point(a.b, 3.0));
    c.round_rect(
        e0.0.min(e1.0),
        e0.1.min(e1.1),
        (e1.0 - e0.0).abs(),
        (e1.1 - e0.1).abs(),
        1.5 * px,
        color,
    );
    for t in [a.a, a.b] {
        c.capsule(point(t, 0.0), point(t, a.d + 5.0), 0.5 * px, color.alpha(0.7));
    }
    let extra = if a.tight { 18.0 } else { 0.0 };
    c.capsule(point(a.a - extra, a.d), point(a.b + extra, a.d), 0.75 * px, color);
    // Arrow triangles are 8×7. Short spans point inward
    // from outside the extension lines; long spans point out from within them.
    for (tip, dir) in [
        (a.a, if a.tight { -1.0 } else { 1.0 }),
        (a.b, if a.tight { 1.0 } else { -1.0 }),
    ] {
        for row in 0..(8.0 * px).ceil() as usize {
            let t = (row as f64 + 0.5) / px;
            let half = (t / 8.0 * 3.5).min(3.5);
            c.capsule(
                point(tip + dir * t, a.d - half),
                point(tip + dir * t, a.d + half),
                0.5,
                color,
            );
        }
    }
    let r = a.tag;
    // Soft two-part shadow under the pill, clipped to this physical monitor.
    let shadow_x0 = ((r.x - 12.0) * px + offset.0).floor().max(0.0) as usize;
    let shadow_x1 = (((r.right() + 12.0) * px + offset.0).ceil() as usize).min(w);
    let shadow_y0 = ((r.y - 12.0) * px + offset.1).floor().max(0.0) as usize;
    let shadow_y1 = (((r.bottom() + 14.0) * px + offset.1).ceil() as usize).min(h);
    for y in shadow_y0..shadow_y1 {
        for x in shadow_x0..shadow_x1 {
            let xx = (x as f64 + 0.5 - offset.0) / px;
            let yy = (y as f64 + 0.5 - offset.1) / px;
            let sd = |dy: f64| {
                let qx = (xx - r.center().x).abs() - (r.w / 2.0 - 11.0);
                let qy = (yy - r.center().y - dy).abs();
                qx.max(0.0).hypot(qy) - 11.0
            };
            let alpha =
                0.16 * (-sd(1.0).max(0.0).powi(2) / 2.0).exp() + 0.12 * (-sd(3.0).max(0.0).powi(2) / 32.0).exp();
            c.px[y * w + x] = over(c.px[y * w + x], Color::rgb(20.0, 22.0, 30.0).alpha(alpha), 1.0);
        }
    }
    c.round_rect(
        r.x * px + offset.0,
        r.y * px + offset.1,
        r.w * px,
        r.h * px,
        11.0 * px,
        color,
    );
    Pen::new(&mut c, font).centered(
        r.center().x * px + offset.0,
        (r.center().y - 11.5 * 0.73 / 2.0) * px + offset.1,
        11.5 * 0.73 * px,
        &a.label,
        false,
        on_accent(rgb),
    );
    Some(Frame {
        bounds: Rect::new(x0, y0, w as f64 / scale, h as f64 / scale),
        width: w,
        height: h,
        pixels: c.px,
        handles: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_display::layout::ScreenId;
    use oink_foundation::DeviceId;
    fn named(side: Side, a: f64, b: f64, scale: f64) -> NamedProjection {
        let screen = Rect::new(-800.0, 100.0, 800.0, 600.0);
        let clip = if side.is_vertical() {
            Rect::new(screen.x, screen.y + a, screen.w, b - a)
        } else {
            Rect::new(screen.x + a, screen.y, b - a, screen.h)
        };
        NamedProjection {
            name: "Neighbour computer with a very long name".into(),
            pixels_per_unit: scale,
            screen,
            geometry: Projection {
                host: ScreenId::new(DeviceId([1; 32]), "host"),
                neighbour: ScreenId::new(DeviceId([2; 32]), "peer"),
                side,
                rect: clip,
                clip,
                visible: clip,
                reference_per_native_unit: 1.0,
            },
        }
    }
    #[test]
    fn dimension_labels_stay_horizontal_clamped_and_short_arrows_move_outside() {
        for side in Side::ALL {
            for (a, b) in [(0.0, 0.5), (0.0, 20.0), (100.0, 400.0), (580.0, 600.0)] {
                let n = named(side, a, b, 2.0);
                let d = annotation(&n, &text::StrokeText, 1.0);
                assert_eq!(d.tight, b - a < 44.0);
                assert!(d.tag.x >= 0.0 && d.tag.right() <= n.screen.w);
                assert!(d.tag.y >= 0.0 && d.tag.bottom() <= n.screen.h);
                assert!(d.tag.w > d.tag.h);
                let f = raster(&n, [46, 123, 255], &text::StrokeText).unwrap();
                assert!(f.bounds.x >= n.screen.x && f.bounds.y >= n.screen.y);
                assert!(f.bounds.right() <= n.screen.right() && f.bounds.bottom() <= n.screen.bottom());
                assert!(f.pixels.iter().any(|p| *p >> 24 == 255));
                assert!(
                    f.pixels
                        .iter()
                        .all(|p| [0, 8, 16].into_iter().all(|s| (p >> s) & 255 <= p >> 24))
                );
            }
        }
    }
    #[test]
    fn on_accent_matches_ui_token_lightness_threshold() {
        assert_eq!(on_accent([46, 123, 255]), Color::rgb(255.0, 255.0, 255.0));
        assert_eq!(on_accent([255, 220, 80]), Color::rgb(27.0, 28.0, 34.0));
    }
}
