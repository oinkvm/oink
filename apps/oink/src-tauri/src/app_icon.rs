//! Runtime icons belong to the UI process; bundle/Finder artwork stays static.
use std::sync::Mutex;

use oink_agent::{
    api::UiPreferences,
    config::{ThemeAccent, ThemeMode},
};
use tauri::{AppHandle, Manager};

use crate::icon_render::{Appearance, Artwork, render};

#[derive(Default)]
pub(crate) struct AppIcon {
    settings: Mutex<Settings>,
    #[cfg(target_os = "macos")]
    applied: Mutex<Option<Appearance>>,
    #[cfg(windows)]
    windows: Mutex<std::collections::HashMap<String, platform::WindowIcons>>,
    #[cfg(windows)]
    tray: Mutex<Option<(Appearance, u32, oink_agent::tray::TrayLook)>>,
}

#[derive(Default)]
struct Settings {
    mode: ThemeMode,
    accent: ThemeAccent,
    system_dark: bool,
    #[cfg(windows)]
    tray_presentation: oink_agent::tray::TrayPresentation,
}

impl Settings {
    fn appearance(&self) -> Appearance {
        Appearance::resolve(self.mode, &self.accent, self.system_dark)
    }
}

pub(crate) fn initialize(app: &AppHandle) {
    // Read once before exposing the tray; subscriptions supply subsequent changes.
    if let Some(api) = &app.state::<crate::app::Backend>().api {
        match api.ui_preferences() {
            Ok(value) => preferences(app, &value),
            Err(error) => tracing::warn!(%error, "initial application appearance unavailable"),
        }
        #[cfg(windows)]
        match api.device_cards() {
            Ok(view) => {
                app.state::<AppIcon>()
                    .settings
                    .lock()
                    .unwrap()
                    .tray_presentation
                    .update(&view);
            }
            Err(error) => tracing::warn!(%error, "initial tray state unavailable"),
        }
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        let watched = app.clone();
        oink_agent::appearance::watch_system_theme(move |dark| {
            theme_changed(&watched, if dark { tauri::Theme::Dark } else { tauri::Theme::Light });
        });
    }
}

pub(crate) fn preferences(app: &AppHandle, preferences: &UiPreferences) {
    let preferences = preferences.clone();
    crate::app::on_main(app, move |app| {
        let state = app.state::<AppIcon>();
        let mut settings = state.settings.lock().unwrap();
        settings.mode = preferences.theme_mode;
        settings.accent = preferences.theme_accent;
        drop(settings);
        refresh(app);
    });
}

pub(crate) fn theme_changed(app: &AppHandle, theme: tauri::Theme) {
    crate::app::on_main(app, move |app| {
        app.state::<AppIcon>().settings.lock().unwrap().system_dark = theme == tauri::Theme::Dark;
        refresh(app);
    });
}

pub(crate) fn refresh(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    dock(app, false);
    #[cfg(windows)]
    {
        let state = app.state::<AppIcon>();
        state.settings.lock().unwrap().system_dark = oink_agent::appearance::system_dark();
        if let Some(tray) = app.tray_by_id("main") {
            let settings = state.settings.lock().unwrap();
            let appearance = tray_appearance(&settings);
            let key = (appearance, platform::tray_size(app), settings.tray_presentation.look);
            drop(settings);
            let mut applied = state.tray.lock().unwrap();
            if *applied != Some(key) {
                match tray_image(app).and_then(|icon| tray.set_icon(Some(icon)).map_err(|e| e.to_string())) {
                    Ok(()) => *applied = Some(key),
                    Err(error) => tracing::warn!(%error, "could not update tray icon"),
                }
            }
        }
        for window in app.webview_windows().values() {
            prepare_window(app, window);
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn dock(app: &AppHandle, force: bool) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        crate::app::on_main(app, move |app| dock(app, force));
        return;
    };
    // Set the image before switching to Regular, and reapply on every return to
    // Regular: AppKit can restore the bundle image during policy transitions.
    // All NSApplication/NSImage access stays on Tauri's main event loop.
    let state = app.state::<AppIcon>();
    let mut settings = state.settings.lock().unwrap();
    settings.system_dark = oink_agent::appearance::system_dark(mtm);
    let appearance = settings.appearance();
    drop(settings);
    let mut applied = state.applied.lock().unwrap();
    if force || *applied != Some(appearance) {
        match platform::apply(mtm, appearance) {
            Ok(()) => *applied = Some(appearance),
            Err(error) => tracing::warn!(%error, "could not update Dock icon"),
        }
    }
}

#[cfg(windows)]
pub(crate) fn tray_created(app: &AppHandle) {
    if let Err(error) = platform::watch_tray_metrics(app) {
        tracing::warn!(%error, "tray display notifications unavailable");
    }
    refresh(app);
}

#[cfg(windows)]
pub(crate) fn tray_image(app: &AppHandle) -> Result<tauri::image::Image<'static>, String> {
    let state = app.state::<AppIcon>();
    let mut settings = state.settings.lock().unwrap();
    settings.system_dark = oink_agent::appearance::system_dark();
    let appearance = tray_appearance(&settings);
    let look = settings.tray_presentation.look;
    drop(settings);
    let png = crate::icon_render::render_tray(look, appearance, platform::tray_size(app))?
        .encode_png()
        .map_err(|e| e.to_string())?;
    tauri::image::Image::from_bytes(&png).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn tray_appearance(settings: &Settings) -> Appearance {
    Appearance {
        dark: oink_agent::appearance::taskbar_dark(),
        accent: settings.appearance().accent,
    }
}

#[cfg(windows)]
pub(crate) fn device_cards(app: &AppHandle, view: &oink_agent::device_cards::DeviceCardsView) {
    let view = view.clone();
    crate::app::on_main(app, move |app| {
        let state = app.state::<AppIcon>();
        let mut settings = state.settings.lock().unwrap();
        if !settings.tray_presentation.update(&view) {
            return;
        }
        drop(settings);
        refresh(app);
    });
}

#[cfg(windows)]
pub(crate) fn prepare_window(app: &AppHandle, window: &tauri::WebviewWindow) {
    let state = app.state::<AppIcon>();
    let mut settings = state.settings.lock().unwrap();
    settings.system_dark = oink_agent::appearance::system_dark();
    let appearance = settings.appearance();
    drop(settings);
    if let Err(error) = platform::apply(&mut state.windows.lock().unwrap(), window, appearance) {
        tracing::warn!(%error, "could not update window/taskbar icons");
    }
}

#[cfg(windows)]
pub(crate) fn closed(app: &AppHandle, label: &str) {
    // WM_SETICON borrows HICONs. Keep them alive until replacement or WM_DESTROY.
    app.state::<AppIcon>().windows.lock().unwrap().remove(label);
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    pub(super) fn apply(mtm: MainThreadMarker, appearance: Appearance) -> Result<(), String> {
        let pixels = render(Artwork::Mac, appearance, 1024)?;
        let png = pixels.encode_png().map_err(|e| e.to_string())?;
        let data = NSData::with_bytes(&png);
        let image = NSImage::initWithData(NSImage::alloc(), &data).ok_or("could not decode application icon")?;
        // SAFETY: valid retained NSImage, set and retained by NSApplication on
        // its main thread. This changes only the running process's Dock image.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::collections::HashMap;
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{
            HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi},
            WindowsAndMessaging::{
                CreateIconFromResourceEx, DestroyIcon, HICON, ICON_BIG, ICON_SMALL, LR_DEFAULTCOLOR, SM_CXSMICON,
                SendMessageW, WM_SETICON,
            },
        },
    };

    pub(super) fn watch_tray_metrics(app: &AppHandle) -> Result<(), String> {
        use windows::Win32::{Foundation::HWND, UI::Shell::SetWindowSubclass};
        let tray = app.tray_by_id("main").ok_or("tray unavailable")?;
        let app = app.clone();
        tray.with_inner_tray_icon(move |tray| {
            let data = Box::into_raw(Box::new(app));
            // Observe the tray's existing owner window: it receives display
            // broadcasts even when no WebView exists. Never create a window or
            // poll DPI, and leave Explorer recreation to tray-icon itself.
            // SAFETY: this runs on the HWND's owning UI thread. The subclass
            // owns data until WM_NCDESTROY, or frees it on registration failure.
            if unsafe { SetWindowSubclass(HWND(tray.window_handle()), Some(tray_proc), 0x4f494e4b, data as usize) }
                .as_bool()
            {
                Ok(())
            } else {
                // SAFETY: failed registration never took ownership of our box.
                unsafe { drop(Box::from_raw(data)) };
                Err("could not observe tray owner window".to_owned())
            }
        })
        .map_err(|e| e.to_string())?
    }

    unsafe extern "system" fn tray_proc(
        hwnd: windows::Win32::Foundation::HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
        id: usize,
        data: usize,
    ) -> windows::Win32::Foundation::LRESULT {
        use windows::Win32::UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass},
            WindowsAndMessaging::{WM_DISPLAYCHANGE, WM_DPICHANGED, WM_NCDESTROY, WM_SETTINGCHANGE},
        };
        // SAFETY: SetWindowSubclass supplies our live AppHandle box. Forward
        // first so native tray recreation/metrics settle before rendering.
        unsafe {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            if msg == WM_NCDESTROY {
                let _ = RemoveWindowSubclass(hwnd, Some(tray_proc), id);
                drop(Box::from_raw(data as *mut AppHandle));
            } else if matches!(msg, WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE) {
                let app = &*(data as *const AppHandle);
                crate::app::on_main(app, refresh);
            }
            result
        }
    }

    pub(super) fn tray_size(app: &AppHandle) -> u32 {
        use windows::Win32::{
            Foundation::POINT,
            Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
            UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        };
        if let Some(rect) = app.tray_by_id("main").and_then(|tray| tray.rect().ok().flatten()) {
            let position = rect.position.to_physical::<i32>(1.0);
            // Shell tray rectangles use physical pixels. Select the actual tray
            // monitor rather than the primary monitor when their DPIs differ.
            // SAFETY: read-only lookup using the live shell tray rectangle.
            let monitor = unsafe {
                MonitorFromPoint(
                    POINT {
                        x: position.x,
                        y: position.y,
                    },
                    MONITOR_DEFAULTTONEAREST,
                )
            };
            let (mut x, mut y) = (0, 0);
            // SAFETY: read-only monitor/metric queries with valid output storage.
            if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_ok() {
                // SAFETY: x is the DPI returned for the tray's live monitor.
                return unsafe { GetSystemMetricsForDpi(SM_CXSMICON, x) as u32 };
            }
        }
        small_size()
    }

    fn small_size() -> u32 {
        // SAFETY: system DPI and metrics are read-only, including tray-only startup.
        unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) as u32 }
    }

    struct Icon(usize);
    impl Icon {
        fn new(appearance: Appearance, size: u32) -> Result<Self, String> {
            let png = render(Artwork::Windows, appearance, size)?
                .encode_png()
                .map_err(|e| e.to_string())?;
            // SAFETY: Windows 10+ accepts PNG RT_ICON data. The API copies it;
            // the returned non-shared HICON is owned until DestroyIcon.
            unsafe { CreateIconFromResourceEx(&png, true, 0x00030000, size as i32, size as i32, LR_DEFAULTCOLOR) }
                .map(|icon| Self(icon.0 as usize))
                .map_err(|e| e.to_string())
        }
    }
    impl Drop for Icon {
        fn drop(&mut self) {
            // SAFETY: this is our non-shared HICON, no longer used by a window.
            let _ = unsafe { DestroyIcon(HICON(self.0 as _)) };
        }
    }

    pub(super) struct WindowIcons {
        appearance: Appearance,
        dpi: u32,
        _small: Icon,
        _big: Icon,
    }

    pub(super) fn apply(
        icons: &mut HashMap<String, WindowIcons>,
        window: &tauri::WebviewWindow,
        appearance: Appearance,
    ) -> Result<(), String> {
        let hwnd = window.hwnd().map_err(|e| e.to_string())?;
        // SAFETY: a live Tauri HWND, accessed on its owning UI thread.
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if icons
            .get(window.label())
            .is_some_and(|old| old.appearance == appearance && old.dpi == dpi)
        {
            return Ok(());
        }
        // Small caption icons use the monitor's exact 16/24/32/48px metrics.
        // Retain a 256px big icon so taskbar/Alt-Tab previews can downsample
        // without magnifying a 32px image. Tauri set_icon sets only ICON_SMALL.
        // SAFETY: pure metric lookup for the live window DPI.
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) } as u32;
        let small = Icon::new(appearance, size)?;
        let big = Icon::new(appearance, 256)?;
        // SAFETY: synchronous messages borrow our live HICONs. Replace both
        // before dropping the previous pair; never destroy Tao's bundle icons.
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small.0 as isize)),
            );
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(big.0 as isize)),
            );
        }
        icons.insert(
            window.label().to_owned(),
            WindowIcons {
                appearance,
                dpi,
                _small: small,
                _big: big,
            },
        );
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::{
            Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW},
            UI::WindowsAndMessaging::{GetIconInfo, ICONINFO},
        };

        #[test]
        fn png_icons_decode_at_caption_and_shell_sizes() {
            for size in [16, 24, 32, 48, 256] {
                let icon = Icon::new(
                    Appearance {
                        dark: true,
                        accent: [18, 171, 205],
                    },
                    size,
                )
                .unwrap();
                let mut info = ICONINFO::default();
                // SAFETY: owned live HICON and correctly sized output structs.
                // GetIconInfo returns new bitmap copies, deleted below.
                unsafe {
                    GetIconInfo(HICON(icon.0 as _), &mut info).unwrap();
                    let mut bitmap = BITMAP::default();
                    assert_ne!(
                        GetObjectW(
                            info.hbmColor.into(),
                            std::mem::size_of::<BITMAP>() as i32,
                            Some((&mut bitmap as *mut BITMAP).cast())
                        ),
                        0
                    );
                    assert_eq!((bitmap.bmWidth, bitmap.bmHeight), (size as i32, size as i32));
                    let _ = DeleteObject(info.hbmColor.into());
                    let _ = DeleteObject(info.hbmMask.into());
                }
            }
        }
    }
}
