//! Input-driven session-idle accounting with a filtered zero-delta scroll carrier.
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, CGPoint, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

// Keep the source tag across conversion. Quartz retains the first user-data
// value on macOS 26.6.2, so the Null request already carries the final tag.
pub(super) const TAG: i64 = 0x4f494451;
const SYNTHETIC: u64 = 0x2000_0000;
// The per-event form of event-state-combining suppression. The Session
// continuation's CombinedSession accounting skips modifier/key-state writes
// when bit 31 is set, while recording input time. This is observed SkyLight
// behavior, not a public SDK contract. Keep the Annotated filter mandatory.
const SKIP_COMBINED_KEY_STATE: u64 = 0x8000_0000;
const PULSE_TYPE: CGEventType = CGEventType::ScrollWheel;
// Clear the scroll representations, including fractional/pixel deltas
// and phases, so even earlier scroll observers receive a zero-amount event.
const SCROLL_FIELDS: [CGEventField; 13] = [
    CGEventField::ScrollWheelEventDeltaAxis1,
    CGEventField::ScrollWheelEventDeltaAxis2,
    CGEventField::ScrollWheelEventDeltaAxis3,
    CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
    CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
    CGEventField::ScrollWheelEventFixedPtDeltaAxis3,
    CGEventField::ScrollWheelEventPointDeltaAxis1,
    CGEventField::ScrollWheelEventPointDeltaAxis2,
    CGEventField::ScrollWheelEventPointDeltaAxis3,
    CGEventField::ScrollWheelEventScrollPhase,
    CGEventField::ScrollWheelEventMomentumPhase,
    CGEventField::ScrollWheelEventScrollCount,
    CGEventField::ScrollWheelEventIsContinuous,
];

// A FlagsChanged carrier reached input-method observers before our Annotated
// filter and acted as a standalone Shift. CombinedSessionState can still lag
// the preceding real key at Session. Bit 31 protects the CGS scoreboard, but
// observers interpret the event itself. Use a zero-delta scroll
// event with metadata-only flags and drop it before application delivery.
// Sample its location at conversion, so queued requests carry no old position.
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
            1 << PULSE_TYPE.0,
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
    current_position: impl FnOnce() -> Option<CGPoint>,
) -> bool {
    if !admit(*last, now, available, allowed) {
        return false;
    }
    let Some(position) = current_position() else {
        return false;
    };
    prepare(event, position);
    *last = Some(now);
    true
}

fn prepare(event: &CGEvent, position: CGPoint) {
    CGEvent::set_type(Some(event), PULSE_TYPE);
    CGEvent::set_location(Some(event), position);
    CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventDeltaX, 0);
    CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventDeltaY, 0);
    CGEvent::set_integer_value_field(Some(event), CGEventField::KeyboardEventKeycode, 0);
    for field in SCROLL_FIELDS {
        CGEvent::set_double_value_field(Some(event), field, 0.);
    }
    CGEvent::set_flags(Some(event), CGEventFlags(SYNTHETIC | SKIP_COMBINED_KEY_STATE));
}

/// Handle before business translation, modifier reconciliation or own-injection
/// rewriting. A queued request is always consumed when the gate/filter is closed.
pub(super) fn at_session(ty: CGEventType, event: &CGEvent, allowed: impl FnOnce() -> bool) -> Option<bool> {
    if ty == PULSE_TYPE && tagged(event, TAG) {
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
        // A Null carrier alone does not advance CGS session idle. Conversion
        // stays in stream order and has no keyboard transition for earlier
        // observers to interpret. The Annotated filter consumes the pulse
        // after idle accounting. Read the current pointer position here.
        Some(refresh(
            event,
            &mut filter.last,
            now,
            CGEvent::tap_is_enabled(&filter.port),
            allowed(),
            || CGEvent::new(None).map(|snapshot| CGEvent::location(Some(&snapshot))),
        ))
    })
}

fn recover_disabled(ty: CGEventType, available: &AtomicBool, enable: impl FnOnce() -> bool) -> bool {
    if !matches!(
        ty,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        return false;
    }
    available.store(false, Ordering::Release);
    let enabled = enable();
    available.store(enabled, Ordering::Release);
    tracing::warn!(?ty, enabled, "idle pulse filter recovery attempted");
    true
}

unsafe extern "C-unwind" fn filter_callback(
    _: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    _: *mut c_void,
) -> *mut CGEvent {
    if recover_disabled(ty, &AVAILABLE, || {
        FILTER.with(|slot| {
            slot.borrow().as_ref().is_some_and(|filter| {
                CGEvent::tap_enable(&filter.port, true);
                CGEvent::tap_is_enabled(&filter.port)
            })
        })
    }) {
        return event.as_ptr();
    }
    // SAFETY: Quartz supplies a valid event for the callback duration.
    if ty == PULSE_TYPE && tagged(unsafe { event.as_ref() }, TAG) {
        std::ptr::null_mut()
    } else {
        event.as_ptr()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn disabled_filter_recovers_immediately_and_failed_recovery_stays_closed() {
        for ty in [CGEventType::TapDisabledByTimeout, CGEventType::TapDisabledByUserInput] {
            for enabled in [true, false] {
                let available = AtomicBool::new(true);
                assert!(recover_disabled(ty, &available, || {
                    assert!(!available.load(Ordering::Acquire), "closed during the OS call");
                    enabled
                }));
                assert_eq!(available.load(Ordering::Acquire), enabled);
                let event = request_event();
                let mut last = None;
                assert_eq!(
                    refresh(&event, &mut last, Instant::now(), enabled, true, || {
                        assert!(enabled, "failed enable must not construct a pulse");
                        Some(CGPoint::new(0., 0.))
                    }),
                    enabled
                );
                assert_eq!(last.is_some(), enabled);
            }
        }
        let available = AtomicBool::new(true);
        assert!(!recover_disabled(PULSE_TYPE, &available, || panic!("ordinary event")));
        assert!(available.load(Ordering::Acquire));
    }

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
    fn idle_carrier_has_no_keyboard_transition_even_with_stale_shift_flags() {
        for stale in [0, 0x20002, 0x20004, 0x20006, 0x40001, 0x10000] {
            let event = request_event();
            CGEvent::set_flags(Some(&event), CGEventFlags(stale));
            CGEvent::set_integer_value_field(
                Some(&event),
                CGEventField::KeyboardEventKeycode,
                if stale & 4 != 0 { 60 } else { 56 },
            );
            CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventDeltaX, 12);
            for field in SCROLL_FIELDS {
                CGEvent::set_double_value_field(Some(&event), field, 3.5);
            }
            let position = CGPoint::new(123., 456.);
            let mut last = None;
            assert!(refresh(&event, &mut last, Instant::now(), true, true, || Some(
                position
            )));
            assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::ScrollWheel);
            assert_eq!(
                CGEvent::integer_value_field(Some(&event), CGEventField::KeyboardEventKeycode),
                0
            );
            assert_eq!(CGEvent::flags(Some(&event)).0, SYNTHETIC | SKIP_COMBINED_KEY_STATE);
            assert_eq!(CGEvent::location(Some(&event)), position);
            for field in [CGEventField::MouseEventDeltaX, CGEventField::MouseEventDeltaY] {
                assert_eq!(CGEvent::integer_value_field(Some(&event), field), 0);
            }
            for field in SCROLL_FIELDS {
                assert_eq!(CGEvent::double_value_field(Some(&event), field), 0.);
            }
            assert!(filtered(&event));
            assert_eq!(
                at_session(PULSE_TYPE, &event, || panic!("pulse entered routing")),
                Some(false)
            );
        }
    }

    #[test]
    fn failed_position_sample_does_not_emit_or_spend_activity_budget() {
        let event = request_event();
        let mut last = None;
        assert!(!refresh(&event, &mut last, Instant::now(), true, true, || None));
        assert_eq!(last, None);
        assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::Null);
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
                || panic!("closed gate must not sample a carrier")
            ));
            assert_eq!(last, None);
            assert_eq!(CGEvent::r#type(Some(&event)), CGEventType::Null);
        }
        // A request arriving after filter withdrawal is swallowed even before
        // querying the owner, so pause cannot leak a queued carrier.
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
                    Some(CGPoint::new(0., 0.))
                }),
                expected
            );
            assert_eq!(filtered(&event), expected);
        }
        assert_eq!(last, Some(now + Duration::from_secs(60)));
    }

    #[test]
    fn filter_drops_only_our_pid_and_exact_pulse_kind_and_tag() {
        let event = request_event();
        prepare(&event, CGPoint::new(0., 0.));
        assert!(filtered(&event));
        for ty in [
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::MouseMoved,
        ] {
            CGEvent::set_type(Some(&event), ty);
            assert!(!filtered(&event));
        }
        CGEvent::set_type(Some(&event), PULSE_TYPE);
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
        assert!(!filtered(&event));
        let ordinary = CGEvent::new(None).unwrap();
        CGEvent::set_integer_value_field(
            Some(&ordinary),
            CGEventField::EventSourceUserData,
            super::super::inject::MAGIC,
        );
        prepare(&ordinary, CGPoint::new(0., 0.));
        assert!(!filtered(&ordinary));
    }
}
