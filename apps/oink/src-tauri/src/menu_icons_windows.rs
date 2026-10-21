//! Native HMENU artwork exported from the actual in-app device menu.
use resvg::{tiny_skia, usvg};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}
impl Size {
    pub fn for_dpi(dpi: u32, menu_check_width: i32, menu_check_height: i32) -> Self {
        let fallback = (16 * dpi.clamp(96, 768) + 48) / 96;
        let metric = |value: i32| {
            u32::try_from(value)
                .ok()
                .filter(|v| (1..=256).contains(v))
                .unwrap_or(fallback)
        };
        // GetSystemMetricsForDpi already returns physical pixels. Scaling its
        // result again would make a 200% monitor's icons four times as large.
        Self {
            width: metric(menu_check_width),
            height: metric(menu_check_height),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub text: [u8; 3],
    pub disabled: [u8; 3],
}
impl Palette {
    pub fn resolve(high_contrast: bool, menu_theme: Option<Self>, system: Self) -> Self {
        // Native menus can remain light while the WebView/taskbar is dark.
        // Their own theme, not the application's preference, owns the colors.
        if high_contrast {
            system
        } else {
            menu_theme.unwrap_or(system)
        }
    }
    pub fn color(self, enabled: bool) -> [u8; 3] {
        if enabled { self.text } else { self.disabled }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Lock,
    Unlock,
    Keyboard,
    Mouse,
    Game,
    Audio,
    Show,
    Pause,
    Resume,
    Quit,
    HideScreens,
    ShowScreens,
}
impl Icon {
    pub fn for_id(id: &str) -> Option<Self> {
        Some(match id {
            "lock" => Self::Lock,
            "unlock" => Self::Unlock,
            "keyboard" => Self::Keyboard,
            "mouse" => Self::Mouse,
            "game" => Self::Game,
            "audio" => Self::Audio,
            "show" => Self::Show,
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "quit" => Self::Quit,
            "hide-screens" => Self::HideScreens,
            "show-screens" => Self::ShowScreens,
            _ => return None,
        })
    }
    fn svg(self) -> &'static str {
        match self {
            Self::Lock => include_str!("../icons/menu/lock.svg"),
            Self::Unlock => include_str!("../icons/menu/unlock.svg"),
            Self::Keyboard => include_str!("../icons/menu/keyboard.svg"),
            Self::Mouse => include_str!("../icons/menu/mouse.svg"),
            Self::Game => include_str!("../icons/menu/game.svg"),
            Self::Audio => include_str!("../icons/menu/audio.svg"),
            Self::Show => include_str!("../icons/menu/show.svg"),
            Self::Pause => include_str!("../icons/menu/pause.svg"),
            Self::Resume => include_str!("../icons/menu/resume.svg"),
            Self::Quit => include_str!("../icons/menu/quit.svg"),
            Self::HideScreens => include_str!("../icons/menu/hide-screens.svg"),
            Self::ShowScreens => include_str!("../icons/menu/show-screens.svg"),
        }
    }
}
/// Top-down premultiplied BGRA bytes (the little-endian DIB representation of
/// 32-bit ARGB). tiny-skia already premultiplies; do not multiply alpha twice.
pub fn rasterize(icon: Icon, size: Size, color: [u8; 3]) -> Result<Vec<u8>, String> {
    let [r, g, b] = color;
    let svg = icon.svg().replace("currentColor", &format!("#{r:02x}{g:02x}{b:02x}"));
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let mut pixels = tiny_skia::Pixmap::new(size.width, size.height).ok_or("invalid menu icon size")?;
    let scale = (size.width as f32 / tree.size().width()).min(size.height as f32 / tree.size().height());
    let x = (size.width as f32 - tree.size().width() * scale) / 2.0;
    let y = (size.height as f32 - tree.size().height() * scale) / 2.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, x, y),
        &mut pixels.as_mut(),
    );
    let mut bytes = pixels.take();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    const LIGHT: Palette = Palette {
        text: [0, 0, 0],
        disabled: [109, 109, 109],
    };
    const DARK: Palette = Palette {
        text: [240, 240, 240],
        disabled: [128, 128, 128],
    };
    #[test]
    fn menu_metrics_are_physical_and_never_scaled_twice() {
        for (dpi, metric) in [(96, 16), (120, 20), (144, 24), (192, 32), (288, 48)] {
            assert_eq!(
                Size::for_dpi(dpi, metric, metric),
                Size {
                    width: metric as u32,
                    height: metric as u32
                }
            );
        }
        assert_eq!(Size::for_dpi(192, 30, 32), Size { width: 30, height: 32 });
        assert_eq!(Size::for_dpi(144, 0, -1), Size { width: 24, height: 24 });
        assert_eq!(Size::for_dpi(0, 0, 0), Size { width: 16, height: 16 });
    }
    #[test]
    fn colors_follow_the_menu_and_high_contrast_overrides_the_theme() {
        assert_eq!(Palette::resolve(false, Some(LIGHT), DARK), LIGHT);
        assert_eq!(Palette::resolve(false, Some(DARK), LIGHT), DARK);
        let contrast = Palette {
            text: [255, 255, 0],
            disabled: [0, 255, 0],
        };
        assert_eq!(Palette::resolve(true, Some(DARK), contrast), contrast);
        assert_eq!(Palette::resolve(false, None, contrast), contrast);
        assert_eq!(DARK.color(false), DARK.disabled);
        assert_eq!(LIGHT.color(true), LIGHT.text);
    }
    #[test]
    fn all_menu_art_renders_at_target_dpi_with_premultiplied_alpha() {
        for id in [
            "lock",
            "unlock",
            "keyboard",
            "mouse",
            "game",
            "audio",
            "show",
            "pause",
            "resume",
            "quit",
            "hide-screens",
            "show-screens",
        ] {
            for edge in [16, 20, 24, 32, 48] {
                let bytes = rasterize(
                    Icon::for_id(id).unwrap(),
                    Size {
                        width: edge,
                        height: edge,
                    },
                    [200, 100, 50],
                )
                .unwrap();
                assert_eq!(bytes.len(), edge as usize * edge as usize * 4);
                assert!(bytes.as_chunks::<4>().0.iter().any(|p| p[3] > 0), "{id}");
                assert!(bytes.as_chunks::<4>().0.iter().any(|p| p[3] == 0), "{id}");
                assert!(bytes.as_chunks::<4>().0.iter().any(|p| p[3] > 0 && p[3] < 255), "{id}");
                for p in bytes.as_chunks::<4>().0 {
                    assert!(p[0] <= p[1] && p[1] <= p[2] && p[2] <= p[3]);
                    // RGB is premultiplied once. Overlapping SVG strokes add
                    // several rounds of 8-bit compositing rounding.
                    assert!(
                        (i32::from(p[2]) * 255 - 200 * i32::from(p[3])).abs() <= 3 * 255,
                        "{id} {edge} {p:?}"
                    );
                }
            }
        }
        assert!(Icon::for_id("keyboard-shared").is_none());
        assert!(Icon::for_id("game-auto").is_none());
    }
}

#[cfg(windows)]
pub use platform::MenuIcons;
#[cfg(windows)]
mod platform {
    use super::*;
    use windows::{
        Win32::{
            Foundation::{COLORREF, HWND, POINT},
            Graphics::Gdi::*,
            UI::{
                Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
                Controls::*,
                HiDpi::*,
                WindowsAndMessaging::*,
            },
        },
        core::w,
    };

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Appearance {
        dpi: u32,
        size: Size,
        palette: Palette,
    }
    fn rgb(value: COLORREF) -> [u8; 3] {
        [value.0 as u8, (value.0 >> 8) as u8, (value.0 >> 16) as u8]
    }
    fn appearance(owner: HWND, point: POINT) -> Appearance {
        // SAFETY: read-only monitor/theme queries with initialized output buffers;
        // owner is the live tray HWND, and theme handles are closed below.
        unsafe {
            let mut dpi = GetDpiForWindow(owner).max(96);
            let mut y = dpi;
            let _ = GetDpiForMonitor(
                MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST),
                MDT_EFFECTIVE_DPI,
                &mut dpi,
                &mut y,
            );
            let size = Size::for_dpi(
                dpi,
                GetSystemMetricsForDpi(SM_CXMENUCHECK, dpi),
                GetSystemMetricsForDpi(SM_CYMENUCHECK, dpi),
            );
            let system = Palette {
                text: rgb(COLORREF(GetSysColor(COLOR_MENUTEXT))),
                disabled: rgb(COLORREF(GetSysColor(COLOR_GRAYTEXT))),
            };
            let mut hc = HIGHCONTRASTW {
                cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
                ..Default::default()
            };
            let _ = SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                hc.cbSize,
                Some((&mut hc as *mut HIGHCONTRASTW).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let theme = OpenThemeDataForDpi(Some(owner), w!("Menu"), dpi);
            let themed = if theme.is_invalid() {
                None
            } else {
                let normal = GetThemeColor(theme, MENU_POPUPITEM.0, MPI_NORMAL.0, TMT_TEXTCOLOR);
                let disabled = GetThemeColor(theme, MENU_POPUPITEM.0, MPI_DISABLED.0, TMT_TEXTCOLOR);
                let _ = CloseThemeData(theme);
                normal.ok().zip(disabled.ok()).map(|(a, b)| Palette {
                    text: rgb(a),
                    disabled: rgb(b),
                })
            };
            Appearance {
                dpi,
                size,
                palette: Palette::resolve(hc.dwFlags.contains(HCF_HIGHCONTRASTON), themed, system),
            }
        }
    }
    struct Bitmap(HBITMAP);
    impl Bitmap {
        fn new(icon: Icon, size: Size, color: [u8; 3]) -> Result<Self, String> {
            let bytes = rasterize(icon, size, color)?;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.width as i32,
                    biHeight: -(size.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            // SAFETY: initialized top-down 32bpp descriptor; successful allocation
            // supplies width * height * 4 writable bytes, exactly bytes.len().
            unsafe {
                let bitmap =
                    Self(CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).map_err(|e| e.to_string())?);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), bits.cast(), bytes.len());
                Ok(bitmap)
            }
        }
    }
    impl Drop for Bitmap {
        fn drop(&mut self) {
            // SAFETY: owned DIB, detached from its menu before deletion.
            let _ = unsafe { DeleteObject(self.0.into()) };
        }
    }
    struct Entry {
        position: u32,
        icon: Icon,
        enabled: bool,
        bitmap: Option<Bitmap>,
    }
    pub struct MenuIcons {
        menu: HMENU,
        entries: Vec<Entry>,
        appearance: Option<Appearance>,
    }
    impl MenuIcons {
        pub fn new(menu: HMENU, model: &oink_agent::native_controls::Model) -> Self {
            let rows = model.groups.iter().enumerate().map(|(i, g)| (i, g.id, true)).chain(
                model
                    .tasks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (model.groups.len() + 1 + i, t.id, t.enabled)),
            );
            let entries = rows
                .filter_map(|(position, id, enabled)| {
                    Icon::for_id(id).map(|icon| Entry {
                        position: position as u32,
                        icon,
                        enabled,
                        bitmap: None,
                    })
                })
                .collect();
            Self {
                menu,
                entries,
                appearance: None,
            }
        }
        pub fn refresh(&mut self, owner: HWND, point: POINT) -> Result<(), String> {
            let appearance = appearance(owner, point);
            if self.appearance == Some(appearance) {
                return Ok(());
            }
            self.appearance = None;
            for entry in &mut self.entries {
                let bitmap = Bitmap::new(entry.icon, appearance.size, appearance.palette.color(entry.enabled))?;
                set_bitmap(self.menu, entry.position, bitmap.0)?;
                // Replace only after native installation succeeds; partially
                // refreshed menus retain every bitmap still referenced by HMENU.
                entry.bitmap = Some(bitmap);
            }
            self.appearance = Some(appearance);
            Ok(())
        }
    }
    fn set_bitmap(menu: HMENU, position: u32, bitmap: HBITMAP) -> Result<(), String> {
        let info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            hbmpItem: bitmap,
            ..Default::default()
        };
        // SAFETY: retained live menu and owned bitmap. MIIM_BITMAP alone preserves
        // native check/radio state, submenu handles, labels and command IDs.
        unsafe { SetMenuItemInfoW(menu, position, true, &info) }.map_err(|e| e.to_string())
    }
    impl Drop for MenuIcons {
        fn drop(&mut self) {
            for entry in &mut self.entries {
                // A still-live menu must never reference freed memory. On an
                // unexpected detach failure, retain this small GDI allocation.
                if set_bitmap(self.menu, entry.position, HBITMAP::default()).is_err()
                    && let Some(bitmap) = entry.bitmap.take()
                {
                    std::mem::forget(bitmap);
                }
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn item_bitmaps_preserve_native_checks_and_radio_style() {
            // SAFETY: this test owns an unattached popup menu. It never displays
            // a window, posts input, or accesses the interactive desktop.
            unsafe {
                let menu = CreatePopupMenu().unwrap();
                let item = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_STRING,
                    dwTypeData: windows::core::PWSTR(w!("Keyboard").as_ptr() as *mut _),
                    fType: MFT_RADIOCHECK,
                    fState: MFS_CHECKED,
                    wID: 42,
                    ..Default::default()
                };
                InsertMenuItemW(menu, 0, true, &item).unwrap();
                let bitmap = Bitmap::new(Icon::Keyboard, Size { width: 24, height: 24 }, [30, 30, 30]).unwrap();
                set_bitmap(menu, 0, bitmap.0).unwrap();
                let mut actual = MENUITEMINFOW {
                    cbSize: item.cbSize,
                    fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_BITMAP,
                    ..Default::default()
                };
                GetMenuItemInfoW(menu, 0, true, &mut actual).unwrap();
                assert_eq!(actual.hbmpItem, bitmap.0);
                assert_eq!(actual.fType, MFT_RADIOCHECK);
                assert_eq!(actual.fState, MFS_CHECKED);
                assert_eq!(actual.wID, 42);
                set_bitmap(menu, 0, HBITMAP::default()).unwrap();
                drop(bitmap);
                DestroyMenu(menu).unwrap();
            }
        }
    }
}
