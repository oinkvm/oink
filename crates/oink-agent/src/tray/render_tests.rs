use super::*;
use resvg::{tiny_skia, usvg};

fn render(look: TrayLook, paint: TrayPaint, size: u32) -> tiny_skia::Pixmap {
    let tree = usvg::Tree::from_str(&svg(look, paint), &usvg::Options::default()).unwrap();
    let mut pixmap = tiny_skia::Pixmap::new(size, size).unwrap();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(size as f32 / 16., size as f32 / 16.),
        &mut pixmap.as_mut(),
    );
    pixmap
}

fn snapshot(name: &str, actual: &tiny_skia::Pixmap) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tray/images")
        .join(format!("{name}.png"));
    if std::env::var_os("OINK_UPDATE_TRAY_SNAPSHOTS").is_some() {
        actual.save_png(&path).unwrap();
    }
    let expected = tiny_skia::Pixmap::load_png(&path).unwrap();
    assert_eq!(
        (actual.width(), actual.height()),
        (expected.width(), expected.height()),
        "{name}"
    );
    assert_eq!(actual.data(), expected.data(), "pixel snapshot: {name}");
}

const STATES: [(TrayState, &str); 6] = [
    (TrayState::Default, "default"),
    (TrayState::Cursor, "cursor"),
    (TrayState::Locked, "locked"),
    (TrayState::Game, "game"),
    (TrayState::ScreenOff, "screenOff"),
    (TrayState::Paused, "paused"),
];

#[test]
fn tray_pixel_snapshots_cover_states_fills_themes_and_dpi_sizes() {
    for size in [16, 32, 48] {
        for (state, name) in STATES {
            for solid in [false, true] {
                let actual = render(TrayLook { state, solid }, TrayPaint::MacTemplate, size);
                assert!(
                    actual
                        .pixels()
                        .iter()
                        .all(|p| p.red() == 0 && p.green() == 0 && p.blue() == 0)
                );
                snapshot(
                    &format!("mac-{name}-{}-{size}", if solid { "solid" } else { "outline" }),
                    &actual,
                );
            }
        }
    }
    for size in [16, 20, 24, 28, 32, 40, 48] {
        for dark in [false, true] {
            let mut sheet = tiny_skia::Pixmap::new(12 * size, size).unwrap();
            for (i, (state, _)) in STATES.into_iter().enumerate() {
                for solid in [false, true] {
                    let image = render(
                        TrayLook { state, solid },
                        TrayPaint::Windows {
                            dark,
                            accent: [255, 107, 142],
                        },
                        size,
                    );
                    sheet.draw_pixmap(
                        (i * 2 + usize::from(solid)) as i32 * size as i32,
                        0,
                        image.as_ref(),
                        &tiny_skia::PixmapPaint::default(),
                        tiny_skia::Transform::identity(),
                        None,
                    );
                }
            }
            snapshot(
                &format!("windows-{}-{size}", if dark { "dark" } else { "light" }),
                &sheet,
            );
        }
    }
}

#[test]
fn tray_holes_and_outline_interiors_remain_transparent() {
    for paint in [
        TrayPaint::MacTemplate,
        TrayPaint::Windows {
            dark: true,
            accent: [255, 107, 142],
        },
    ] {
        let solid = render(
            TrayLook {
                state: TrayState::Cursor,
                solid: true,
            },
            paint,
            160,
        );
        let outline = render(
            TrayLook {
                state: TrayState::Default,
                solid: false,
            },
            paint,
            160,
        );
        assert_eq!(solid.pixel(30, 50).unwrap().alpha(), 255);
        assert_eq!(outline.pixel(30, 50).unwrap().alpha(), 0);
        for (state, x, y) in [
            (TrayState::Locked, 126, 49),
            (TrayState::Game, 139, 29),
            (TrayState::Game, 145, 36),
        ] {
            let image = render(TrayLook { state, solid: true }, paint, 160);
            assert_eq!(image.pixel(x, y).unwrap().alpha(), 0, "{state:?} badge hole");
        }
    }
}
