//! Input-driven session-idle accounting, isolated from keyboard delivery.
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

// Keep the source tag across conversion. Quartz on macOS 26.6.2 retains the
// first EventSourceUserData value when it is set again on the same event.
pub(super) const TAG: i64 = 0x4f494451;
const SYNTHETIC: u64 = 0x2000_0000;
// Provenance: the internal per-event marker behind the deprecated public
// CGEnableEventStateCombining(false), NOT IOHID's unrelated high-keycode encoding.
// SLEnableEventStateCombining clears the main connection's eventState byte at
// 0x185c64f00 (14.7.8) / 0x1870140ac (26.6.2); SLSPostKeyboardEvent then ORs
// bit 31 at 0x185c8618c / 0x187168a28 when that byte is false. SLSPostEventRecord
// and sHIDContinuation also apply it. Meaning: do not combine this synthetic
// event's keyboard state with the session/hardware state. The same mechanism
// handles suppressed physical HID input (sHIDContinuation: 0x185d28c30 /
// 0x186f12bc4); it is not synthetic-only or an idle-specific feature.
// We do NOT call the
// deprecated connection-wide API: it clears connection key records and affects
// other injected events on Oink's main connection. Mark only this idle pulse.
// The flag itself is undocumented, absent from the public SDK. SkyLight's
// _CGXUpdateScoreboard shows source ID 0 (CombinedSession) + flags bit 31 skips
// modifier/key-state scoreboard writes, but still records input time/count.
// Both versions check bit 31 ONLY when sourceID == 0: sourceID != 0 branches
// PAST that test into ordinary state writes. The annotation accounting call
// passes w1=0 (14.7.8: 0x185f72fa4; 26.6.2: 0x186ee4b1c), so our converted
// carrier takes the CombinedSession branch regardless of its private source.
// Unslid arm64e addresses checked:
// - macOS 14.7.8 (23H730): function 0x185dc48c4, bit test 0x185dc4940
//   -> 0x185dc4a54; aggregate time 0x185dc4c28, event count 0x185dc4cbc.
// - macOS 26.6.2 (25G83): function 0x186dda654, bit test 0x186dda6d0
//   -> 0x186dda7e0; time/count updates remain downstream of that branch.
// SkyLight's _SLEventPost ORs SYNTHETIC at 0x185d5b824 (14.7.8) and
// 0x187171170 (26.6.2). This is observed implementation, not a supported API.
// loginwindow's ScreenSaverDaemon reads this idle time through
// CGSSecondsSinceLastInputEvent(-1), not HIDIdleTime (26.6.2 call: 0x100085524).
// The 26.6.2 native held/release-before/press-before probe reset CGS idle to
// ~0.20s without changing the expected session/HID modifier state or leaking
// events past Annotated; Shift was restored. 14.7.8 is STATIC-ONLY evidence:
// no already-authorized generic GUI probe host was available on that machine.
// filter_callback logs modifier differences for detection if a future macOS
// changes this behavior; it never writes a stale snapshot back as a repair.
// Bit 31 also affects coalescing/repeat dropping, so is NOT globally inert:
// CGSCanCoalesceEvents rejects different types / bit31 XOR (14.7.8:
// 0x185c11a20/30; 26.6.2: 0x186cfd138/148). Session queue mask 0x098000e0
// excludes FlagsChanged; rejection preserves distinct queue items, no new wait.
// should_discard_event only examines its own event; FlagsChanged bypasses the
// repeat-key test (14.7.8: 0x185fabebc–c4; 26.6.2: 0x186e74d94–a4). Since
// the pulse changes no key state and is dropped at Annotated, it cannot change
// an adjacent real key's repeat-drop decision. Ordinary tap/queue work remains;
// see group-power-mirroring.md for the bounded argument and exact call sites.
// Mouse/button and other accounting may still update for other event types.
// Annotated's system bit31 clear is event+0x80, NOT flags(+0x38): we must drop
// the whole tagged event, not rely on the OS hiding/clearing its private flags.
const SKIP_COMBINED_KEY_STATE: u64 = 0x8000_0000;
static AVAILABLE: AtomicBool = AtomicBool::new(false);
thread_local! { static FILTER: RefCell<Option<Filter>> = const { RefCell::new(None) }; }
struct Filter {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    last: Option<Instant>,
}

fn tagged(event: &CGEvent, tag: i64) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData) == tag
        && CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID)
            == i64::from(std::process::id())
}

pub(super) fn install(run_loop: &CFRunLoop) {
    // SAFETY: callback needs no context; the run-loop owner retains its port/source.
    let Some(port) = (unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::AnnotatedSessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            1 << CGEventType::FlagsChanged.0,
            Some(filter_callback),
            std::ptr::null_mut(),
        )
    }) else {
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        CFMachPort::invalidate(&port);
        return;
    };
    // SAFETY: exported CoreFoundation constant.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&port, true);
    let available = CGEvent::tap_is_enabled(&port);
    FILTER.with(|slot| {
        *slot.borrow_mut() = Some(Filter {
            port,
            source,
            last: None,
        })
    });
    AVAILABLE.store(available, Ordering::Release);
}

pub(super) fn remove(run_loop: &CFRunLoop) {
    AVAILABLE.store(false, Ordering::Release);
    FILTER.with(|slot| {
        if let Some(filter) = slot.borrow_mut().take() {
            CGEvent::tap_enable(&filter.port, false);
            // SAFETY: source belongs to this run loop.
            run_loop.remove_source(Some(&filter.source), unsafe { kCFRunLoopCommonModes });
            CFMachPort::invalidate(&filter.port);
        }
    });
}

/// Called only by ResetUserIdle after genuine local/peer input, never a timer,
/// audio declaration or a mirrored power assertion. No modifier snapshot is queued.
pub(super) fn request() {
    if !AVAILABLE.load(Ordering::Acquire) {
        return;
    }
    let Some(source) = CGEventSource::new(CGEventSourceStateID::Private) else {
        return;
    };
    // Match the native probe and ordinary injector: a carrier must not start a
    // local-input suppression interval. This source posts only Null at Session.
    CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
    let Some(event) = CGEvent::new(Some(&source)) else {
        return;
    };
    CGEvent::set_type(Some(&event), CGEventType::Null);
    CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, TAG);
    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
}

fn admit(last: Option<Instant>, now: Instant, available: bool, allowed: bool) -> bool {
    available && allowed && super::activity::due(last, now)
}

fn refresh(
    event: &CGEvent,
    last: &mut Option<Instant>,
    now: Instant,
    available: bool,
    allowed: bool,
    current_flags: impl FnOnce() -> CGEventFlags,
) -> bool {
    if !admit(*last, now, available, allowed) {
        return false;
    }
    prepare(event, current_flags());
    *last = Some(now);
    true
}

fn prepare(event: &CGEvent, current: CGEventFlags) {
    CGEvent::set_type(Some(event), CGEventType::FlagsChanged);
    CGEvent::set_integer_value_field(Some(event), CGEventField::KeyboardEventKeycode, 0x38);
    CGEvent::set_flags(
        Some(event),
        CGEventFlags(current.0 | SYNTHETIC | SKIP_COMBINED_KEY_STATE),
    );
}

/// Handle before business translation, modifier reconciliation or own-injection
/// rewriting. A queued request is always consumed when the gate/filter is closed.
pub(super) fn at_session(ty: CGEventType, event: &CGEvent, allowed: impl FnOnce() -> bool) -> Option<bool> {
    if ty == CGEventType::FlagsChanged && tagged(event, TAG) {
        return Some(false);
    }
    if ty != CGEventType::Null || !tagged(event, TAG) {
        return None;
    }
    FILTER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(filter) = slot.as_mut() else {
            return Some(false);
        };
        let now = Instant::now();
        // macOS 26.6.2 loginwindow's _checkUserIdleDuringReset reads
        // CGSSecondsSinceLastInputEvent(-1), not IORegistry HIDIdleTime. Both
        // DeclareUserActivity and HID Null leave that CGS timer running. A
        // FlagsChanged reaches its idle accounting before the Annotated tap.
        // Convert the Null *here*, in stream order, using CURRENT local session
        // flags. Reading Combined alone is insufficient: a native held/race
        // probe showed an earlier modifier can still await CGS accounting, so a
        // pulse would replay stale flags and undo that release/press. Bit 31
        // (SKIP_COMBINED_KEY_STATE) skips CGS modifier/key-state writes even in that
        // ordering; the timestamp still advances. Never post through HID (that could
        // overwrite a concurrently changed physical modifier). Captured physical
        // modifiers and local session modifiers intentionally remain distinct.
        // 0x20000000 is Quartz's observed synthetic-event bit, not a modifier;
        // retain it when replacing the Null. No KeyDown/Up or MouseMoved is sent.
        // The downstream filter prevents application key/hover delivery, but
        // earlier global taps can still observe this marked FlagsChanged.
        Some(refresh(
            event,
            &mut filter.last,
            now,
            CGEvent::tap_is_enabled(&filter.port),
            allowed(),
            || CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState),
        ))
    })
}

unsafe extern "C-unwind" fn filter_callback(
    _: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    _: *mut c_void,
) -> *mut CGEvent {
    if matches!(
        ty,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        // Fail closed until capture is reinstalled. Never emit while disabled.
        AVAILABLE.store(false, Ordering::Release);
        return event.as_ptr();
    }
    // SAFETY: Quartz supplies a valid event for the callback duration.
    if ty == CGEventType::FlagsChanged && tagged(unsafe { event.as_ref() }, TAG) {
        // SAFETY: the event remains valid throughout this callback.
        let before = CGEvent::flags(Some(unsafe { event.as_ref() }));
        let after = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState);
        if modifier_state_changed(before, after) {
            // Detection only: a preceding genuine transition can account after
            // Session sampled flags. Never replay/repair that snapshot here.
            tracing::warn!(
                before = before.0,
                after = after.0,
                "session modifier state differs across idle pulse"
            );
        }
        std::ptr::null_mut()
    } else {
        event.as_ptr()
    }
}

fn modifier_state_changed(before: CGEventFlags, after: CGEventFlags) -> bool {
    // Standard modifier flags and device-side modifier bits; omit event metadata.
    const MODIFIERS: u64 = 0x00ff_ffff & !CGEventFlags::MaskNonCoalesced.0;
    (before.0 ^ after.0) & MODIFIERS != 0
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::time::Duration;

    fn request_event() -> CFRetained<CGEvent> {
        let event = CGEvent::new(None).unwrap();
        CGEvent::set_type(Some(&event), CGEventType::Null);
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUnixProcessID,
            i64::from(std::process::id()),
        );
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, TAG);
        event
    }
    fn filtered(event: &CGEvent) -> bool {
        // SAFETY: the event lives through this synchronous callback; no user context.
        unsafe {
            filter_callback(
                std::ptr::null_mut(),
                CGEvent::r#type(Some(event)),
                NonNull::from(event),
                std::ptr::null_mut(),
            )
            .is_null()
        }
    }

    #[test]
    fn held_modifiers_survive_pulses_without_local_or_remote_key_delivery() {
        let now = Instant::now();
        // Both sides of Shift, Ctrl, Option, Command, plus Fn/Caps lock. These
        // are *session* flags; the captured/remote physical ledger is separate.
        for bits in [
            0x20002, 0x20004, 0x40001, 0x42000, 0x80020, 0x80040, 0x100008, 0x100010, 0x800000, 0x10000,
        ] {
            let mut last = None;
            for second in 0..3 {
                let event = request_event();
                assert!(refresh(
                    &event,
                    &mut last,
                    now + Duration::from_secs(second),
                    true,
                    true,
                    || CGEventFlags(bits)
                ));
                assert_eq!(
                    CGEvent::flags(Some(&event)).0 & !(SYNTHETIC | SKIP_COMBINED_KEY_STATE),
                    bits
                );
                assert!(filtered(&event), "no local key event leaves Annotated");
                assert_eq!(
                    at_session(CGEventType::FlagsChanged, &event, || panic!(
                        "pulse must bypass business routing"
                    )),
                    Some(false)
                );
            }
        }
    }

    #[test]
    fn racing_modifier_change_uses_state_at_session_not_request_time() {
        for (queued, current) in [(0x20002, 0), (0, 0x20004), (0x20002, 0x20004)] {
            let event = request_event();
            CGEvent::set_flags(Some(&event), CGEventFlags(queued));
            let mut last = None;
            assert!(refresh(&event, &mut last, Instant::now(), true, true, || CGEventFlags(
                current
            )));
            assert_eq!(
                CGEvent::flags(Some(&event)).0 & !(SYNTHETIC | SKIP_COMBINED_KEY_STATE),
                current
            );
            assert_ne!(
                CGEvent::flags(Some(&event)).0 & SKIP_COMBINED_KEY_STATE,
                0,
                "a stale flags snapshot must never be allowed to update CombinedSession"
            );
            assert!(filtered(&event));
            // A real change behind the pulse remains an ordinary event. Neither
            // the Session interception nor the Annotated filter consumes it.
            let real = request_event();
            CGEvent::set_type(Some(&real), CGEventType::FlagsChanged);
            CGEvent::set_integer_value_field(Some(&real), CGEventField::EventSourceUserData, 0);
            CGEvent::set_integer_value_field(Some(&real), CGEventField::EventSourceUnixProcessID, 0);
            CGEvent::set_flags(Some(&real), CGEventFlags(queued));
            assert_eq!(at_session(CGEventType::FlagsChanged, &real, || panic!()), None);
            assert!(!filtered(&real));
            assert_eq!(CGEvent::flags(Some(&real)).0, queued);
        }
    }

    #[test]
    fn unavailable_filter_pause_lock_or_disabled_wake_cannot_emit_or_consume_budget() {
        for (available, allowed) in [(false, true), (true, false), (false, false)] {
            let event = request_event();
            let mut last = None;
            assert!(!refresh(
                &event,
                &mut last,
                Instant::now(),
                available,
                allowed,
                || panic!("closed gate must not read/post flags")
            ));
            assert_eq!(last, None);
            assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::Null);
        }
        // A request arriving after filter withdrawal is swallowed even before
        // querying the owner, so pause cannot leak a queued synthetic key.
        let event = request_event();
        assert_eq!(at_session(CGEventType::Null, &event, || panic!()), Some(false));
    }

    #[test]
    fn event_driven_one_hz_throttle_never_schedules_a_renewal() {
        let now = Instant::now();
        let mut last = None;
        for (ms, expected) in [
            (0, true),
            (500, false),
            (999, false),
            (1000, true),
            (1001, false),
            (60_000, true),
        ] {
            let event = request_event();
            assert_eq!(
                refresh(&event, &mut last, now + Duration::from_millis(ms), true, true, || {
                    CGEventFlags(0)
                }),
                expected
            );
            assert_eq!(filtered(&event), expected);
        }
        assert_eq!(last, Some(now + Duration::from_secs(60)));
    }

    #[test]
    fn modifier_detection_ignores_pulse_metadata_but_observes_press_release_and_side() {
        assert!(!modifier_state_changed(
            CGEventFlags(0xa002_0102),
            CGEventFlags(0x2002_0002)
        ));
        for (before, after) in [(0x20002, 0), (0, 0x20002), (0x20002, 0x20004)] {
            assert!(modifier_state_changed(CGEventFlags(before), CGEventFlags(after)));
        }
    }

    #[test]
    fn filter_drops_only_our_pid_and_exact_pulse_kind_and_tag() {
        let event = request_event();
        prepare(&event, CGEventFlags(0));
        assert!(filtered(&event));
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
        assert!(!filtered(&event));
        let ordinary = CGEvent::new(None).unwrap();
        CGEvent::set_integer_value_field(
            Some(&ordinary),
            CGEventField::EventSourceUserData,
            super::super::inject::MAGIC,
        );
        prepare(&ordinary, CGEventFlags(0));
        assert!(!filtered(&ordinary));
    }
}
