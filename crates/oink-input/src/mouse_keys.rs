//! Session-only no-mouse compensation policy; native I/O lives on an ordinary thread.
//!
//! Why this exists (Windows 11 build 26200, found 2026-10-27; design notes in
//! docs/design/trackpad-contacts.md, "Mouse Keys"):
//!
//! - win32k only counts a pointing device toward `SM_MOUSEPRESENT` if its RIM
//!   device entry has an opened PnP handle (`RIMOpenDev`). Oink's synthetic
//!   `PT_TOUCHPAD` device (`CreateSyntheticPointerDevice2`) has none, so on a PC
//!   without a physical mouse any device-summary refresh — creating it, its first
//!   injected frame, or destroying it (processed ~17 ms later) — makes
//!   `ApplyGatheredDeviceInfoSummaryInformation` publish "no mouse":
//!   `SM_MOUSEPRESENT=0` and `SetGlobalCursorLevel(-1)`. The cursor disappears
//!   system-wide. Third-party touch/pen injection (e.g. a remote-control host)
//!   triggers the same.
//! - Nothing user mode can do re-publishes presence: SendInput moves, reloading
//!   the pointer scheme, ShowCursor/SetCursor, stopping contacts and destroying
//!   the device all leave it hidden. A synthetic `PT_MOUSE` device is rejected
//!   (ERROR_INVALID_PARAMETER). Only a real mouse arriving while the cached
//!   device count is 0, or Mouse Keys (`MKShowMouseCursor` writes present=1,
//!   buttons=2, `SetGlobalCursorLevel(0)`), brings it back. The user chose
//!   Mouse Keys over a virtual HID mouse driver or drawing our own cursor.
//!
//! Why we never turn Mouse Keys OFF again in the same logon session:
//!
//! - Turning Mouse Keys on sets an "MK owns presence" bit in the session.
//!   `MKHideMouseCursor` (Mouse Keys OFF) checks only that bit and then clears
//!   present/buttons and hides the cursor, without recounting devices.
//! - A real mouse arriving while Mouse Keys is on does not clear that bit, and
//!   it raises the cached device count from 0 (our synthetic device also keeps
//!   that count above 0). The "publish presence" branch only runs on a 0 → N
//!   transition of the cached count, so after OFF nothing ever publishes
//!   presence again: mice are attached and moving, but `SM_MOUSEPRESENT` stays 0
//!   and the cursor is invisible. Restarting or disabling/re-enabling every
//!   mouse did not recover it on the test PC; only Mouse Keys ON or logoff does.
//! - This looks like a win32k state inconsistency (device arrival never hands
//!   presence back from Mouse Keys), i.e. most likely a Windows bug; by code
//!   reading it should also hit a plain user who turns Mouse Keys on without a
//!   mouse, plugs one in, then turns Mouse Keys off. Not confirmed by Microsoft.
//! - No public API reports or clears the ownership bit, so "turn it off and see
//!   whether the cursor survives" would strand the user's session. Hence: once
//!   we enable it, pause/exit/recovery keep it on (`Retained`), the native setter
//!   refuses OFF, and logoff restores the user's profile value (we write with
//!   `fWinIni=0`, never persisting). If the user changes Mouse Keys themselves we
//!   stop owning it and never overwrite their choice.
//! - Accepted side effect: while retained, NumLock OFF turns the numeric keypad
//!   into Mouse Keys (NumLock ON still types digits).
pub(crate) const ON: u32 = 1;
const AVAILABLE: u32 = 2;
const HOTKEY: u32 = 4;
const INDICATOR: u32 = 0x20;
const REPLACE_NUMBERS: u32 = 0x80;
// SPI ignores these transient button-selection / mouse-mode bits on SET.
const TRANSIENT: u32 = 0xb300_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings(pub [u32; 6]);
impl Settings {
    pub fn enabled(self) -> bool {
        self.0[0] & ON != 0
    }
    pub fn compensation(mut self) -> Self {
        // Clearing REPLACENUMBERS makes NumLock ON type digits (physical and
        // SendInput keypad input); NumLock OFF still invokes Windows Mouse Keys.
        // MOUSEMODE is read-only: clearing it cannot disable that OFF behavior.
        // Do not toggle NumLock or rewrite incoming keys to disguise this tradeoff.
        self.0[0] = (self.0[0] | ON | AVAILABLE) & !(HOTKEY | INDICATOR | REPLACE_NUMBERS | TRANSIENT);
        self
    }
    pub fn matches(self, other: Self) -> bool {
        let mut a = self;
        let mut b = other;
        a.0[0] &= !TRANSIENT;
        b.0[0] &= !TRANSIENT;
        a == b
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Marker {
    pub original: Settings,
    pub applied: Settings,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    Periodic = 0,
    Sharing = 1,
    DeviceCreate = 2,
    DeviceInject = 3,
    DeviceDestroy = 4,
    DeviceChange = 5,
    Local = 6,
    RealMouse = 7,
    Pause = 8,
    Exit = 9,
    Recovery = 10,
    OwnershipLost = 11,
}

/// Diagnostic action. Retained means the original OFF state remains deferred
/// until logoff; it does not mean we successfully restored user settings.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Enabled = 1,
    Retained = 2,
    Relinquished = 3,
}

pub(crate) trait System {
    fn settings(&self) -> Option<Settings>;
    /// Compensation may only write Mouse Keys ON. Never attempt OFF to test
    /// presence: MKHideMouseCursor can strand the entire interactive session.
    fn enable(&mut self, value: Settings) -> bool;
    fn present(&self) -> bool;
    fn real_mouse(&self) -> bool;
    fn save(&mut self, marker: Marker) -> bool;
    fn clear(&mut self);
    fn event(&mut self, action: Action, reason: Reason, before: bool, after: bool, success: bool);
}

#[derive(Default)]
pub(crate) struct Compensation {
    owned: Option<Marker>,
    relinquished: bool,
    last_retained: Option<Reason>,
}
impl Compensation {
    pub fn owned(&self) -> bool {
        self.owned.is_some()
    }
    pub fn recover(&mut self, system: &mut impl System, marker: Marker) {
        // A new logon/user change no longer matching the compensation ends the
        // lease. If it still matches, adopt the lease WITHOUT writing original
        // OFF: startup recovery must not reproduce the pause/exit cursor trap.
        if !marker.original.enabled()
            && marker.applied.matches(marker.original.compensation())
            && system.settings().is_some_and(|s| s.matches(marker.applied))
        {
            self.owned = Some(marker);
            self.maintain(system, Reason::Recovery);
            self.retained(system, Reason::Recovery);
        } else {
            system.clear();
        }
    }
    pub fn update(&mut self, system: &mut impl System, active: bool, reason: Reason) {
        if !active {
            self.relinquished = false;
        }
        if active && self.relinquished {
            return;
        }
        if let Some(marker) = self.owned {
            let Some(current) = system.settings() else { return };
            if !current.matches(marker.applied) {
                self.owned = None;
                self.relinquished = active;
                system.clear();
                let present = system.present();
                system.event(Action::Relinquished, Reason::OwnershipLost, present, present, true);
                return;
            }
            self.maintain(system, reason);
            if !active {
                self.retained(system, reason);
            } else if system.real_mouse() {
                // Raw/PnP presence and even real physical motion do not expose
                // win32k's Mouse Keys ownership bit. On build 26200, MK OFF
                // unconditionally clears present while that bit is held, even
                // after mice return; the unchanged cached count does not publish
                // presence again. Do not probe OFF or retry it per input frame.
                self.retained(system, Reason::RealMouse);
            }
        } else if active && !system.present() {
            let Some(original) = system.settings() else { return };
            if original.enabled() {
                return;
            }
            let marker = Marker {
                original,
                applied: original.compensation(),
            };
            if !system.save(marker) {
                return;
            }
            self.owned = Some(marker);
            self.last_retained = None;
            let ok = system.enable(marker.applied);
            if !ok {
                self.owned = None;
                system.clear();
            }
            system.event(Action::Enabled, reason, false, system.present(), ok);
        }
    }
    fn maintain(&mut self, system: &mut impl System, reason: Reason) {
        if let Some(marker) = self.owned
            && !system.present()
        {
            let ok = system.enable(marker.applied);
            system.event(Action::Enabled, reason, false, system.present(), ok);
        }
    }
    fn retained(&mut self, system: &mut impl System, reason: Reason) {
        if self.last_retained != Some(reason) {
            let present = system.present();
            system.event(Action::Retained, reason, present, present, present);
            self.last_retained = Some(reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        settings: Settings,
        present: bool,
        real: bool,
        marker: Option<Marker>,
        sets: Vec<Settings>,
        events: Vec<(Action, Reason)>,
        fail: bool,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                settings: Settings([0x3e, 80, 3000, 10, 0, 0]),
                present: false,
                real: false,
                marker: None,
                sets: Vec::new(),
                events: Vec::new(),
                fail: false,
            }
        }
    }
    impl System for Fake {
        fn settings(&self) -> Option<Settings> {
            Some(self.settings)
        }
        fn enable(&mut self, value: Settings) -> bool {
            assert!(value.enabled(), "policy must never write Mouse Keys OFF");
            if self.fail {
                return false;
            }
            self.sets.push(value);
            self.settings = value;
            self.present = true;
            true
        }
        fn present(&self) -> bool {
            self.present
        }
        fn real_mouse(&self) -> bool {
            self.real
        }
        fn save(&mut self, marker: Marker) -> bool {
            self.marker = Some(marker);
            true
        }
        fn clear(&mut self) {
            self.marker = None;
        }
        fn event(&mut self, action: Action, reason: Reason, _: bool, _: bool, _: bool) {
            self.events.push((action, reason));
        }
    }
    #[test]
    fn enable_once_preserves_original_and_numeric_flags() {
        let mut s = Fake::default();
        let original = s.settings;
        let mut c = Compensation::default();
        c.update(&mut s, true, Reason::DeviceInject);
        for _ in 0..5 {
            c.update(&mut s, true, Reason::Periodic);
        }
        assert_eq!(s.sets, vec![original.compensation()]);
        assert_eq!(s.marker.unwrap().original, original);
        assert_eq!(
            s.settings.0[0] & (ON | AVAILABLE | HOTKEY | INDICATOR | REPLACE_NUMBERS),
            ON | AVAILABLE
        );
    }
    #[test]
    fn real_mouse_arrival_and_repeated_physical_input_never_probe_off() {
        let mut s = Fake::default();
        let mut c = Compensation::default();
        c.update(&mut s, true, Reason::Sharing);
        s.real = true;
        for _ in 0..100 {
            c.update(&mut s, true, Reason::DeviceChange);
        }
        assert!(s.present);
        assert!(s.marker.is_some());
        assert_eq!(s.sets.len(), 1);
        assert_eq!(
            s.events
                .iter()
                .filter(|e| **e == (Action::Retained, Reason::RealMouse))
                .count(),
            1
        );
    }
    #[test]
    fn pause_exit_and_disabled_sharing_keep_session_compensation() {
        for real in [false, true] {
            let mut s = Fake::default();
            let original = s.settings;
            let mut c = Compensation::default();
            c.update(&mut s, true, Reason::Sharing);
            s.real = real;
            for reason in [Reason::Pause, Reason::Exit] {
                for _ in 0..4 {
                    c.update(&mut s, false, reason);
                }
                assert!(s.present);
                assert!(s.settings.enabled());
                assert!(c.owned());
                assert!(s.events.contains(&(Action::Retained, reason)));
            }
            assert_eq!(s.marker.unwrap().original, original);
            assert_eq!(s.sets.len(), 1);
        }
    }
    #[test]
    fn asynchronous_destroy_after_pause_or_exit_is_repaired_without_off() {
        for reason in [Reason::Pause, Reason::Exit] {
            let mut s = Fake::default();
            let mut c = Compensation::default();
            c.update(&mut s, true, Reason::Sharing);
            c.update(&mut s, false, reason);
            s.present = false;
            c.update(&mut s, false, reason);
            assert!(s.present);
            assert_eq!(s.sets.len(), 2);
            assert!(s.marker.is_some());
        }
    }
    #[test]
    fn startup_adopts_matching_crash_or_clean_exit_marker_without_off() {
        for present in [false, true] {
            let mut s = Fake::default();
            let original = s.settings;
            Compensation::default().update(&mut s, true, Reason::Sharing);
            let marker = s.marker.unwrap();
            s.present = present;
            let before = s.sets.len();
            let mut c = Compensation::default();
            c.recover(&mut s, marker);
            assert!(s.present);
            assert!(c.owned());
            assert_eq!(s.marker.unwrap().original, original);
            assert_eq!(s.sets.len(), before + usize::from(!present));
            assert!(s.events.contains(&(Action::Retained, Reason::Recovery)));
        }
    }
    #[test]
    fn new_logon_or_user_change_discards_marker_without_mutating_settings() {
        for user_change in [false, true] {
            let mut s = Fake::default();
            let original = s.settings;
            Compensation::default().update(&mut s, true, Reason::Sharing);
            let marker = s.marker.unwrap();
            if user_change {
                s.settings.0[1] += 1;
            } else {
                s.settings = original;
                s.present = false;
            }
            let before = s.settings;
            let sets = s.sets.len();
            let mut c = Compensation::default();
            c.recover(&mut s, marker);
            assert_eq!(s.settings, before);
            assert_eq!(s.sets.len(), sets);
            assert!(!c.owned());
            assert!(s.marker.is_none());
        }
    }
    #[test]
    fn user_enabled_mouse_keys_is_never_claimed_or_modified() {
        let mut s = Fake::default();
        s.settings.0[0] |= ON;
        let mut c = Compensation::default();
        c.update(&mut s, true, Reason::Sharing);
        c.update(&mut s, false, Reason::Exit);
        assert!(s.sets.is_empty());
        assert!(s.marker.is_none());
    }
    #[test]
    fn explicit_user_changes_relinquish_without_overwriting_or_reenabling() {
        for disable in [false, true] {
            let mut s = Fake::default();
            let mut c = Compensation::default();
            c.update(&mut s, true, Reason::Sharing);
            if disable {
                s.settings.0[0] &= !ON;
                s.present = false;
            } else {
                s.settings.0[1] += 1;
            }
            let changed = s.settings;
            for _ in 0..5 {
                c.update(&mut s, true, Reason::Periodic);
            }
            c.update(&mut s, false, Reason::Exit);
            assert_eq!(s.settings, changed);
            assert_eq!(s.sets.len(), 1);
            assert!(s.marker.is_none());
            assert!(s.events.contains(&(Action::Relinquished, Reason::OwnershipLost)));
        }
    }
    #[test]
    fn failed_enable_keeps_user_state_and_drops_unapplied_marker() {
        let mut s = Fake {
            fail: true,
            ..Default::default()
        };
        let original = s.settings;
        let mut c = Compensation::default();
        c.update(&mut s, true, Reason::Sharing);
        assert_eq!(s.settings, original);
        assert!(!c.owned());
        assert!(s.marker.is_none());
    }
}
