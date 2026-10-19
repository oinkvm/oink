//! Match each wheel hook to a pending or already-dispatched raw report.
//! Both delivery orders were measured on build 26200; neither is an API promise.
//! Never drains the raw queue or records keys, text, or pointer coordinates.
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use windows::Win32::UI::Input::{GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT};
use windows::Win32::UI::WindowsAndMessaging::{
    MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, WM_INPUT, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
};

thread_local! { static PEEKING: Cell<bool> = const { Cell::new(false) }; }
// A second hook can arrive before this queued report's WM_INPUT dispatch.
// One report must never identify two callbacks, even with equal coarse ticks.
thread_local! { static CLAIMED: Cell<Option<isize>> = const { Cell::new(None) }; }
thread_local! { static LEDGER: RefCell<Ledger> = RefCell::new(Ledger::default()); }

pub(super) fn dispatched(handle: isize) {
    CLAIMED.with(|claimed| {
        if claimed.get() == Some(handle) {
            claimed.set(None);
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    PhysicalMouse,
    DeviceLess,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wheel {
    horizontal: bool,
    delta: i16,
    extra: usize,
    time: u32,
}

#[derive(Default)]
struct Ledger {
    raw: VecDeque<(Instant, Wheel, usize)>,
    hooks: VecDeque<(Instant, Wheel)>,
}

impl Ledger {
    fn prune(&mut self, now: Instant) {
        // This bounds matching history, not input suppression. A late or
        // unmatched report remains Unknown and is diagnosed by the caller.
        let stale = |at: Instant| now.saturating_duration_since(at) > Duration::from_secs(1);
        while self.raw.front().is_some_and(|r| stale(r.0)) {
            self.raw.pop_front();
        }
        while self.hooks.front().is_some_and(|h| stale(h.0)) {
            self.hooks.pop_front();
        }
    }
    fn on_hook(&mut self, wheel: Wheel, now: Instant) -> Option<Origin> {
        self.prune(now);
        if let Some(index) = self.raw.iter().position(|r| r.1 == wheel) {
            let (_, raw, device) = self.raw.remove(index)?;
            return Some(identify(wheel, raw, device));
        }
        if self.hooks.len() == 128 {
            self.hooks.pop_front();
        }
        self.hooks.push_back((now, wheel));
        None
    }
    fn on_raw(&mut self, wheel: Wheel, device: usize, now: Instant) {
        self.prune(now);
        if let Some(index) = self.hooks.iter().position(|h| h.1 == wheel) {
            self.hooks.remove(index);
            return; // The corresponding hook already made its decision.
        }
        if self.raw.len() == 128 {
            self.raw.pop_front();
        }
        self.raw.push_back((now, wheel, device));
    }
}

pub(super) fn processed(raw: &RAWINPUT, message_time: u32) {
    if raw.header.dwType != 0 {
        return;
    }
    // SAFETY: complete RAWINPUT whose header identifies the mouse union.
    let mouse = unsafe { raw.data.mouse };
    // SAFETY: button union contains flags and data for a RAWMOUSE.
    let buttons = unsafe { mouse.Anonymous.Anonymous };
    if !matches!(buttons.usButtonFlags & 0x0c00, 0x0400 | 0x0800) {
        return;
    }
    let wheel = Wheel {
        horizontal: buttons.usButtonFlags & 0x0800 != 0,
        delta: buttons.usButtonData as i16,
        extra: mouse.ulExtraInformation as usize,
        time: message_time,
    };
    LEDGER.with(|ledger| {
        ledger
            .borrow_mut()
            .on_raw(wheel, raw.header.hDevice.0 as usize, Instant::now())
    });
}

fn identify(hook: Wheel, raw: Wheel, device: usize) -> Origin {
    if device == usize::MAX || hook != raw {
        Origin::Unknown
    } else if device == 0 {
        Origin::DeviceLess
    } else {
        Origin::PhysicalMouse
    }
}

pub(super) fn hook(message: u32, info: &MSLLHOOKSTRUCT, required: bool) -> Origin {
    if !matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
        return Origin::Unknown;
    }
    let wheel = Wheel {
        horizontal: message == WM_MOUSEHWHEEL,
        delta: (info.mouseData >> 16) as u16 as i16,
        extra: info.dwExtraInfo,
        time: info.time,
    };
    // Record even outside native ownership so its later raw report cannot be
    // mistaken for a new hook with an identical coarse timestamp and delta.
    let processed = LEDGER.with(|ledger| ledger.borrow_mut().on_hook(wheel, Instant::now()));
    if !required {
        return processed.unwrap_or(Origin::Unknown);
    }
    if let Some(origin) = processed {
        return origin;
    }
    // PeekMessage may dispatch sent messages. Do not recurse into another peek.
    if PEEKING.with(|p| p.replace(true)) {
        return Origin::Unknown;
    }
    let mut origin = Origin::Unknown;
    // SAFETY: sizing call does not consume raw reports. PM_NOREMOVE leaves the
    // queued message intact; its raw handle remains valid until normal dispatch.
    unsafe {
        let header = size_of::<RAWINPUTHEADER>() as u32;
        let mut pending = MSG::default();
        let queued = PeekMessageW(&mut pending, None, WM_INPUT, WM_INPUT, PM_NOREMOVE).as_bool();
        if queued && pending.message == WM_INPUT {
            let handle = HRAWINPUT(pending.lParam.0 as _);
            let mut size = 0;
            if GetRawInputData(handle, RID_INPUT, None, &mut size, header) == 0 && size >= header {
                let mut storage = vec![0u64; (size as usize).max(size_of::<RAWINPUT>()).div_ceil(8)];
                let data = storage.as_mut_ptr().cast();
                if GetRawInputData(handle, RID_INPUT, Some(data), &mut size, header) != u32::MAX {
                    let input = &*(data as *const RAWINPUT);
                    if size as usize >= size_of::<RAWINPUT>()
                        && input.header.dwType == 0
                        && CLAIMED.with(|claimed| claimed.get() != Some(pending.lParam.0))
                    {
                        let mouse = input.data.mouse;
                        let buttons = mouse.Anonymous.Anonymous;
                        if matches!(buttons.usButtonFlags & 0x0c00, 0x0400 | 0x0800) {
                            origin = identify(
                                Wheel {
                                    horizontal: message == WM_MOUSEHWHEEL,
                                    delta: (info.mouseData >> 16) as u16 as i16,
                                    extra: info.dwExtraInfo,
                                    time: info.time,
                                },
                                Wheel {
                                    horizontal: buttons.usButtonFlags & 0x0800 != 0,
                                    delta: buttons.usButtonData as i16,
                                    extra: mouse.ulExtraInformation as usize,
                                    time: pending.time,
                                },
                                input.header.hDevice.0 as usize,
                            );
                            if origin != Origin::Unknown {
                                CLAIMED.with(|claimed| claimed.set(Some(pending.lParam.0)));
                            }
                        }
                    }
                }
            }
        }
    }
    PEEKING.with(|p| p.set(false));
    origin
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn reports_match_once_in_both_delivery_orders_without_reusing_old_mouse_input() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let mouse = Wheel {
            horizontal: false,
            delta: 120,
            extra: 0,
            time: 42,
        };
        // Raw dispatch before LL callback (observed with the native backend).
        ledger.on_raw(mouse, 7, now);
        assert_eq!(ledger.on_hook(mouse, now), Some(Origin::PhysicalMouse));
        // An identical later callback cannot borrow that old physical report.
        assert_eq!(ledger.on_hook(mouse, now), None);
        // LL callback before raw dispatch (observed in the external injector).
        // This later report retires the second hook; it is not fresh evidence.
        ledger.on_raw(mouse, 0, now);
        assert!(ledger.raw.is_empty());
        assert!(ledger.hooks.is_empty());
        ledger.on_raw(mouse, 0, now);
        assert_eq!(ledger.on_hook(mouse, now), Some(Origin::DeviceLess));
    }
    #[test]
    fn expired_report_is_not_evidence_for_a_new_hook() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let wheel = Wheel {
            horizontal: true,
            delta: -120,
            extra: 0,
            time: u32::MAX,
        };
        ledger.on_raw(wheel, 7, now);
        assert_eq!(ledger.on_hook(wheel, now + Duration::from_secs(2)), None);
    }
    #[test]
    fn origin_requires_the_same_axis_delta_time_and_tag() {
        let wheel = Wheel {
            horizontal: false,
            delta: -120,
            extra: 0,
            time: 42,
        };
        assert_eq!(identify(wheel, wheel, 7), Origin::PhysicalMouse);
        assert_eq!(identify(wheel, wheel, 0), Origin::DeviceLess);
        assert_eq!(identify(wheel, wheel, usize::MAX), Origin::Unknown);
        for other in [
            Wheel {
                horizontal: true,
                ..wheel
            },
            Wheel { delta: 120, ..wheel },
            Wheel { extra: 1, ..wheel },
            Wheel { time: 43, ..wheel },
        ] {
            assert_eq!(identify(wheel, other, 7), Origin::Unknown);
            assert_eq!(identify(wheel, other, 0), Origin::Unknown);
        }
    }
}
