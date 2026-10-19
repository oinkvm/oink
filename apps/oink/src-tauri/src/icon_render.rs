//! B2 artwork, with only the tile, cursor and tail paints changed.
use oink_agent::config::{ThemeAccent, ThemeMode};
use resvg::{tiny_skia, usvg};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Appearance {
    pub dark: bool,
    pub accent: [u8; 3],
}

impl Appearance {
    pub fn resolve(mode: ThemeMode, accent: &ThemeAccent, system_dark: bool) -> Self {
        Self {
            dark: mode == ThemeMode::Dark || (mode == ThemeMode::System && system_dark),
            accent: accent.resolved(mode, system_dark).rgb(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Artwork {
    #[cfg(any(target_os = "macos", test))]
    Mac,
    #[cfg(any(windows, test))]
    Windows,
}

pub(crate) fn render(artwork: Artwork, appearance: Appearance, size: u32) -> Result<tiny_skia::Pixmap, String> {
    let source = match artwork {
        #[cfg(any(target_os = "macos", test))]
        Artwork::Mac => include_str!("../icons/appicon-macos.svg"),
        #[cfg(any(windows, test))]
        Artwork::Windows => include_str!("../icons/appicon-windows.svg"),
    };
    let [r, g, b] = appearance.accent;
    let svg = source
        .replace("#1B1E28", if appearance.dark { "#000000" } else { "#FFFFFF" })
        .replace(
            "#FFFFFF\" stroke=\"#FFFFFF",
            if appearance.dark {
                "#FFFFFF\" stroke=\"#FFFFFF"
            } else {
                "#000000\" stroke=\"#000000"
            },
        )
        .replace("#2E7BFF", &format!("#{r:02X}{g:02X}{b:02X}"));
    render_svg(&svg, size)
}

#[cfg(any(windows, test))]
pub(crate) fn render_tray(
    look: oink_agent::tray::TrayLook,
    appearance: Appearance,
    size: u32,
) -> Result<tiny_skia::Pixmap, String> {
    let svg = oink_agent::tray::svg(
        look,
        oink_agent::tray::TrayPaint::Windows {
            dark: appearance.dark,
            accent: appearance.accent,
        },
    );
    render_svg(&svg, size)
}

fn render_svg(svg: &str, size: u32) -> Result<tiny_skia::Pixmap, String> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let mut pixels = tiny_skia::Pixmap::new(size, size).ok_or("invalid icon dimensions")?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixels.as_mut(),
    );
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_agent::config::ThemeColor;

    #[test]
    fn native_tray_render_uses_shared_designer_artwork() {
        use oink_agent::tray::{TrayLook, TrayState};
        let solid = render_tray(
            TrayLook {
                state: TrayState::Cursor,
                solid: true,
            },
            Appearance {
                dark: false,
                accent: [255, 107, 142],
            },
            32,
        )
        .unwrap();
        let hollow = render_tray(
            TrayLook::default(),
            Appearance {
                dark: false,
                accent: [255, 107, 142],
            },
            32,
        )
        .unwrap();
        assert_ne!(solid.data(), hollow.data());
        let paused = render_tray(
            TrayLook {
                state: TrayState::Paused,
                solid: false,
            },
            Appearance {
                dark: false,
                accent: [255, 107, 142],
            },
            32,
        )
        .unwrap();
        assert_ne!(paused.data(), hollow.data());
    }

    #[test]
    fn recolours_only_the_three_paints_in_both_platform_artworks() {
        for artwork in [Artwork::Mac, Artwork::Windows] {
            for dark in [false, true] {
                for accent in [[0xE7, 0x36, 0x81], [0x12, 0xAB, 0xCD]] {
                    let image = render(artwork, Appearance { dark, accent }, 1024).unwrap();
                    let (tail, cursor) = match artwork {
                        // Interior points in the tail's horizontal curl and cursor.
                        Artwork::Mac => ((632, 734), (330, 430)),
                        Artwork::Windows => ((684, 829), (250, 390)),
                    };
                    let pixel = |(x, y)| {
                        let p = image.pixel(x, y).unwrap().demultiply();
                        [p.red(), p.green(), p.blue(), p.alpha()]
                    };
                    assert_eq!(pixel(tail), [accent[0], accent[1], accent[2], 255]);
                    let bg = if dark { 0 } else { 255 };
                    assert_eq!(pixel((512, 180)), [bg, bg, bg, 255]);
                    let fg = 255 - bg;
                    assert_eq!(pixel(cursor), [fg, fg, fg, 255]);
                    assert_eq!(pixel((0, 0))[3], 0);
                }
            }
        }
    }

    #[test]
    fn renders_taskbar_sizes_and_preserves_geometry_across_accents() {
        for size in [16, 24, 32, 48, 256] {
            let a = render(
                Artwork::Windows,
                Appearance {
                    dark: false,
                    accent: [255, 0, 0],
                },
                size,
            )
            .unwrap();
            let b = render(
                Artwork::Windows,
                Appearance {
                    dark: true,
                    accent: [0, 255, 0],
                },
                size,
            )
            .unwrap();
            assert_eq!(a.width(), size);
            assert_eq!(a.height(), size);
            assert_eq!(
                a.pixels().iter().map(|p| p.alpha()).collect::<Vec<_>>(),
                b.pixels().iter().map(|p| p.alpha()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn fresh_native_icons_use_pink_in_both_appearances() {
        for dark in [false, true] {
            let appearance = Appearance::resolve(ThemeMode::System, &ThemeAccent::default(), dark);
            assert_eq!(appearance.accent, [255, 107, 142]);
            for artwork in [Artwork::Mac, Artwork::Windows] {
                let image = render(artwork, appearance, 1024).unwrap();
                let (x, y) = match artwork {
                    Artwork::Mac => (632, 734),
                    Artwork::Windows => (684, 829),
                };
                let tail = image.pixel(x, y).unwrap().demultiply();
                assert_eq!(
                    [tail.red(), tail.green(), tail.blue(), tail.alpha()],
                    [255, 107, 142, 255]
                );
            }
        }
    }

    #[test]
    fn effective_mode_selects_the_matching_accent_including_system_changes() {
        let pair = ThemeAccent {
            light: ThemeColor::try_from("#12ABCD".to_owned()).unwrap(),
            dark: ThemeColor::try_from("#EF4591".to_owned()).unwrap(),
        };
        for (mode, system, dark) in [
            (ThemeMode::Light, true, false),
            (ThemeMode::Dark, false, true),
            (ThemeMode::System, false, false),
            (ThemeMode::System, true, true),
        ] {
            let appearance = Appearance::resolve(mode, &pair, system);
            assert_eq!(appearance.dark, dark);
            assert_eq!(appearance.accent, if dark { pair.dark.rgb() } else { pair.light.rgb() });
        }
    }
}
