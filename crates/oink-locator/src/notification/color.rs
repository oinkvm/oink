//! The custom-accent text color follows oink-ui/core/color.ts: retain hue in
//! OKLab, reduce chroma to fit sRGB and reach 4.6:1 against white or dark card.
use crate::aim::raster::Color;
fn linear(v: f64) -> f64 {
    let v = v / 255.;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}
fn encoded(v: f64) -> f64 {
    255. * if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1. / 2.4) - 0.055
    }
}
fn luminance(rgb: [f64; 3]) -> f64 {
    linear(rgb[0]) * 0.2126 + linear(rgb[1]) * 0.7152 + linear(rgb[2]) * 0.0722
}
fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let a = luminance(a);
    let b = luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
fn lab(rgb: [f64; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(linear);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ]
}
fn rgb([light, a, b]: [f64; 3]) -> [f64; 3] {
    let l = (light + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m = (light - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s = (light - 0.0894841775 * a - 1.291485548 * b).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ]
}
fn at_lightness(base: [f64; 3], light: f64) -> [f64; 3] {
    let [_, a, b] = base;
    let chroma = a.hypot(b);
    let hue = b.atan2(a);
    let at = |c: f64| rgb([light, c * hue.cos(), c * hue.sin()]);
    let in_gamut = |v: [f64; 3]| v.into_iter().all(|c| (-1e-4..=1. + 1e-4).contains(&c));
    let (mut lo, mut hi) = (0., chroma);
    if in_gamut(at(chroma)) {
        lo = chroma;
    } else {
        for _ in 0..20 {
            let mid = (lo + hi) / 2.;
            if in_gamut(at(mid)) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
    }
    at(lo).map(|v| encoded(v.clamp(0., 1.)))
}
pub(super) fn accent_text(accent: [u8; 3], dark: bool) -> Color {
    let accent = accent.map(f64::from);
    let bg = if dark { [42., 41., 39.] } else { [255.; 3] };
    let mut best = accent;
    if contrast(accent, bg) < 4.6 {
        let base = lab(accent);
        let (mut lo, mut hi) = (0., 1.);
        for _ in 0..24 {
            let mid = (lo + hi) / 2.;
            let light = if dark {
                base[0] + (1. - base[0]) * mid
            } else {
                base[0] * (1. - mid)
            };
            let candidate = at_lightness(base, light);
            if contrast(candidate, bg) >= 4.6 {
                best = candidate;
                hi = mid;
            } else {
                lo = mid;
            }
        }
    }
    Color::rgb(best[0].round(), best[1].round(), best[2].round())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accents_retain_readable_ink_in_both_appearances() {
        for rgb in [[255, 107, 142], [255; 3], [0; 3], [46, 123, 255]] {
            for dark in [false, true] {
                let ink = accent_text(rgb, dark);
                assert!(contrast([ink.r, ink.g, ink.b], if dark { [42., 41., 39.] } else { [255.; 3] }) > 4.5);
            }
        }
    }
}
