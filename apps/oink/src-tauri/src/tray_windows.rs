//! Shell notification selection opens the native menu; double-click opens Oink.
#[derive(Clone, Copy, Debug)]
enum Input {
    LeftDown,
    Select,
    KeyboardSelect,
    ContextMenu,
    DoubleClick,
}
#[derive(Debug, PartialEq)]
enum Action {
    None,
    Menu,
    Show,
}
#[derive(Default)]
struct Clicks {
    suppress_release: bool,
}
impl Clicks {
    fn input(&mut self, input: Input) -> Action {
        match input {
            Input::LeftDown => {
                self.suppress_release = false;
                Action::None
            }
            Input::DoubleClick => {
                // Shell still sends the second click's selection/release after
                // WM_LBUTTONDBLCLK. It must not reopen the menu we just closed.
                self.suppress_release = true;
                Action::Show
            }
            Input::Select if std::mem::take(&mut self.suppress_release) => Action::None,
            Input::Select => Action::Menu,
            Input::KeyboardSelect | Input::ContextMenu => {
                self.suppress_release = false;
                Action::Menu
            }
        }
    }
}

#[cfg(windows)]
pub use platform::{install, set_menu};
#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    use tauri::{
        AppHandle,
        menu::{ContextMenu, Menu},
    };
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
            UI::{
                Shell::{
                    DefSubclassProc, NIF_SHOWTIP, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NINF_KEY,
                    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, RemoveWindowSubclass, SetWindowSubclass, Shell_NotifyIconW,
                },
                WindowsAndMessaging::{
                    EndMenu, GetCursorPos, HMENU, PostMessageW, RegisterWindowMessageW, SetForegroundWindow,
                    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TrackPopupMenu, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_DPICHANGED,
                    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP,
                    WM_SETTINGCHANGE, WM_THEMECHANGED,
                },
            },
        },
        core::w,
    };

    // tray-icon 0.25 (via our pinned Tauri 2.12) exposes its owner HWND but not
    // the callback message. Keep this adapter aligned when upgrading Tauri.
    // We own only its notification callback; menu command dispatch, icon/DPI
    // updates, and Explorer recreation still belong to tray-icon/muda.
    const TRAY_CALLBACK: u32 = 6002;
    // Shell headers define this as a macro, omitted by windows-rs metadata.
    const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
    const SUBCLASS: usize = 0x4f494e43;
    thread_local! {
        static MENU: RefCell<Option<Rc<MenuBundle>>> = const { RefCell::new(None) };
    }
    struct MenuBundle {
        // Drop icons (detach their handles) while the menu is still retained.
        icons: RefCell<crate::menu_icons_windows::MenuIcons>,
        menu: Menu<tauri::Wry>,
    }
    struct State {
        active: RefCell<Option<Rc<MenuBundle>>>,
        anchor: Cell<POINT>,
        app: AppHandle,
        clicks: RefCell<Clicks>,
        icon_id: Cell<Option<u32>>,
        version4: Cell<bool>,
        tracking: Cell<bool>,
        show_after_tracking: Cell<bool>,
        taskbar_created: u32,
    }

    pub fn set_menu(menu: Menu<tauri::Wry>, model: &oink_agent::native_controls::Model) -> tauri::Result<()> {
        let handle = HMENU(menu.hpopupmenu()? as *mut _);
        let icons = RefCell::new(crate::menu_icons_windows::MenuIcons::new(handle, model));
        MENU.with(|slot| *slot.borrow_mut() = Some(Rc::new(MenuBundle { icons, menu })));
        Ok(())
    }

    pub fn install(app: &AppHandle) -> tauri::Result<()> {
        let tray = app
            .tray_by_id("main")
            .expect("tray created before native click adapter");
        let app = app.clone();
        tray.with_inner_tray_icon(move |tray| {
            // Disable the library's legacy button-up popup on BOTH buttons:
            // otherwise a v4 notification or double-click release can show a
            // second menu after this subclass has already handled selection.
            tray.set_show_menu_on_left_click(false);
            tray.set_show_menu_on_right_click(false);
            let state = Rc::new(State {
                active: RefCell::new(None),
                anchor: Cell::new(POINT::default()),
                app,
                clicks: RefCell::new(Clicks::default()),
                icon_id: Cell::new(None),
                version4: Cell::new(false),
                tracking: Cell::new(false),
                show_after_tracking: Cell::new(false),
                // SAFETY: static UTF-16 window-message name.
                taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            });
            let raw = Rc::into_raw(state);
            // SAFETY: the HWND and subclass run on this UI thread. The raw Rc
            // is owned until WM_NCDESTROY, or reclaimed on registration failure.
            if unsafe { SetWindowSubclass(HWND(tray.window_handle()), Some(window_proc), SUBCLASS, raw as usize) }
                .as_bool()
            {
                Ok(())
            } else {
                // SAFETY: failed registration never took ownership of this Rc.
                unsafe {
                    drop(Rc::from_raw(raw));
                }
                Err(tauri::Error::Io(std::io::Error::last_os_error()))
            }
        })?
    }

    fn version4(hwnd: HWND, state: &State) {
        let Some(id) = state.icon_id.get() else { return };
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: id,
            ..Default::default()
        };
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: live notification owner and Shell-provided icon id. Tauri
        // creates this icon without a GUID. The first legacy callback supplies
        // its otherwise-private id; typically it is the initial mouse move.
        // Handle that first event in legacy format, then use v4 NIN_SELECT /
        // WM_CONTEXTMENU (including keyboard activation) from then on.
        state
            .version4
            .set(unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool());
        if state.version4.get() {
            // v4 otherwise suppresses the standard tooltip in favour of rich
            // popup notifications. Retain Tauri's existing tooltip text.
            data.uFlags = NIF_SHOWTIP;
            // SAFETY: same live icon; NIF_SHOWTIP does not replace szTip.
            let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        }
    }

    fn popup(hwnd: HWND, state: &State, point: POINT) {
        if state.tracking.get() {
            return;
        }
        // Keep the old native menu alive across the nested tracking loop even
        // if a backend publication installs a replacement during that loop.
        let menu = MENU.with(|slot| slot.borrow().clone());
        let Some(menu) = menu else { return };
        let Ok(handle) = menu.menu.hpopupmenu() else { return };
        if let Err(error) = menu.icons.borrow_mut().refresh(hwnd, point) {
            tracing::warn!(%error, "native menu icon update failed");
        }
        state.anchor.set(point);
        *state.active.borrow_mut() = Some(menu.clone());
        state.tracking.set(true);
        // SAFETY: menu remains retained, hwnd is its live native command owner.
        // No timer: a single selection opens immediately. Shell double-click
        // notifications can reenter this subclass while TrackPopupMenu runs.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                HMENU(handle as *mut _),
                TPM_BOTTOMALIGN | TPM_LEFTALIGN,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            );
            // Shell requires this benign message to finish notification-menu
            // foreground handling (including dismissing by clicking outside).
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        }
        state.tracking.set(false);
        state.active.borrow_mut().take();
        if state.show_after_tracking.replace(false) {
            crate::app::show_main(&state.app);
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
        id: usize,
        data: usize,
    ) -> LRESULT {
        // SAFETY: subclass data owns a live Rc<State> on this UI thread.
        // The extra Rc keeps state alive if the HWND is destroyed from inside
        // the native menu's nested message loop. No RefCell borrow spans it.
        let state = unsafe {
            let pointer = data as *const State;
            Rc::increment_strong_count(pointer);
            Rc::from_raw(pointer)
        };
        if msg == TRAY_CALLBACK {
            let modern = state.version4.get();
            let notification = if modern { lp.0 as u32 & 0xffff } else { lp.0 as u32 };
            if !modern {
                state.icon_id.set(Some(wp.0 as u32));
                version4(hwnd, &state);
            }
            let input = match notification {
                WM_LBUTTONDOWN => Some(Input::LeftDown),
                WM_LBUTTONDBLCLK => Some(Input::DoubleClick),
                NIN_SELECT => Some(Input::Select),
                NIN_KEYSELECT => Some(Input::KeyboardSelect),
                WM_CONTEXTMENU => Some(Input::ContextMenu),
                WM_LBUTTONUP if !modern => Some(Input::Select),
                WM_RBUTTONUP if !modern => Some(Input::ContextMenu),
                _ => None,
            };
            if let Some(input) = input {
                let action = state.clicks.borrow_mut().input(input);
                match action {
                    Action::None => {}
                    Action::Show if state.tracking.get() => {
                        state.show_after_tracking.set(true);
                        // SAFETY: cancel only our active menu on its UI thread.
                        let _ = unsafe { EndMenu() };
                    }
                    Action::Show => crate::app::show_main(&state.app),
                    Action::Menu => {
                        let mut point = POINT::default();
                        if modern {
                            // v4 packs signed screen coordinates in wParam;
                            // unlike cursor position this also anchors keyboard
                            // activation at the actual notification icon.
                            point.x = wp.0 as u16 as i16 as i32;
                            point.y = (wp.0 >> 16) as u16 as i16 as i32;
                        } else {
                            // SAFETY: writable POINT; this only reads cursor position.
                            if unsafe { GetCursorPos(&mut point) }.is_err() {
                                return LRESULT(0);
                            }
                        }
                        popup(hwnd, &state, point);
                    }
                }
            }
            // The dependency understands legacy lParam only. Do not forward
            // packed v4 values into its mouse-event/popup implementation.
            return LRESULT(0);
        }
        // SAFETY: forward unchanged arguments along the native subclass chain.
        let result = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
        if matches!(
            msg,
            WM_THEMECHANGED | WM_SETTINGCHANGE | WM_DPICHANGED | WM_DISPLAYCHANGE
        ) {
            let active = state.active.borrow().clone();
            if let Some(menu) = active
                && let Err(error) = menu.icons.borrow_mut().refresh(hwnd, state.anchor.get())
            {
                tracing::warn!(%error, "native menu appearance refresh failed");
            }
        }
        if msg == WM_NCDESTROY {
            // SAFETY: detach once, reclaim the registration's Rc; this callback
            // still owns its separate Rc through any enclosing menu loop.
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(window_proc), id);
                drop(Rc::from_raw(data as *const State));
            }
        } else if msg == state.taskbar_created {
            // NIM_ADD resets the version; reapply only after tray-icon has
            // recreated the icon on Explorer's TaskbarCreated broadcast.
            state.version4.set(false);
            version4(hwnd, &state);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_selection_and_both_context_menu_sources_are_immediate() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.input(Input::LeftDown), Action::None);
        assert_eq!(clicks.input(Input::Select), Action::Menu);
        assert_eq!(clicks.input(Input::ContextMenu), Action::Menu);
        assert_eq!(clicks.input(Input::KeyboardSelect), Action::Menu);
    }
    #[test]
    fn double_click_does_not_reopen_menu_on_second_release() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.input(Input::LeftDown), Action::None);
        assert_eq!(clicks.input(Input::Select), Action::Menu);
        assert_eq!(clicks.input(Input::DoubleClick), Action::Show);
        assert_eq!(clicks.input(Input::Select), Action::None);
        assert_eq!(clicks.input(Input::LeftDown), Action::None);
        assert_eq!(clicks.input(Input::Select), Action::Menu);
    }
    #[test]
    fn missing_second_release_does_not_swallow_next_gesture() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.input(Input::DoubleClick), Action::Show);
        assert_eq!(clicks.input(Input::LeftDown), Action::None);
        assert_eq!(clicks.input(Input::Select), Action::Menu);
        assert_eq!(clicks.input(Input::DoubleClick), Action::Show);
        assert_eq!(clicks.input(Input::KeyboardSelect), Action::Menu);
    }
}
