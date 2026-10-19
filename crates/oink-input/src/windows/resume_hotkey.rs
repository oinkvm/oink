//! One paused-only system hotkey, delivered to the routing thread's message
//! queue. No low-level hook or raw-input listener is needed.
use crate::capture_loop::ResumeHotkey;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VSC_TO_VK_EX, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, MapVirtualKeyW, RegisterHotKey,
    UnregisterHotKey,
};
pub(super) const ID: i32 = 0x4f49;
#[derive(Default)]
pub(super) struct Manager {
    requested: Option<ResumeHotkey>,
    registered: bool,
}
impl Manager {
    pub fn update(&mut self, requested: Option<ResumeHotkey>) {
        if self.requested == requested {
            return;
        }
        self.clear();
        self.requested = requested;
        let Some(chord) = requested else {
            return;
        };
        let Some(entry) = crate::win_keymap::lookup_hid(chord.key) else {
            tracing::error!("paused resume hotkey has no Windows key code; use UI, tray or CLI");
            return;
        };
        let mut mods = MOD_NOREPEAT;
        if chord.control {
            mods |= MOD_CONTROL;
        }
        if chord.alt {
            mods |= MOD_ALT;
        }
        if chord.shift {
            mods |= MOD_SHIFT;
        }
        if chord.meta {
            mods |= MOD_WIN;
        }
        // SAFETY: registration and removal are on the owning message-loop
        // thread. NULL hwnd selects its message queue instead of a hook.
        unsafe {
            let vk = if entry.vk != 0 {
                u32::from(entry.vk)
            } else {
                MapVirtualKeyW(u32::from(entry.scancode), MAPVK_VSC_TO_VK_EX)
            };
            match RegisterHotKey(None, ID, mods, vk) {
                Ok(()) => self.registered = true,
                Err(error) => tracing::error!(%error, "paused resume hotkey unavailable; use UI, tray or CLI"),
            }
        }
    }
    fn clear(&mut self) {
        if self.registered {
            // SAFETY: this thread owns the registration and removes it once.
            unsafe {
                let _ = UnregisterHotKey(None, ID);
            }
            self.registered = false;
        }
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.clear();
    }
}
