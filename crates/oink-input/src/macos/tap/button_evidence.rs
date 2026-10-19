//! Button-only, listen-only HID evidence alongside the unchanged session capture tap.
use super::*;
use crate::button_diagnostics::{self as evidence, Layer, TimestampUnit, Transition};

thread_local! { static PORT: Cell<*const CFMachPort> = const { Cell::new(std::ptr::null()) }; }

pub(super) fn boundary(layer: Layer, available: bool) {
    evidence::tap_boundary(layer, available, now_event_ns());
}

pub(super) fn observe(layer: Layer, ty: CGEventType, ev: &CGEvent) {
    let (number, pressed) = match ty {
        CGEventType::LeftMouseDown => (0, true),
        CGEventType::LeftMouseUp => (0, false),
        CGEventType::RightMouseDown => (1, true),
        CGEventType::RightMouseUp => (1, false),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => (
            CGEvent::integer_value_field(Some(ev), CGEventField::MouseEventButtonNumber) as u32,
            ty == CGEventType::OtherMouseDown,
        ),
        _ => return,
    };
    let button = button_from_number(i64::from(number));
    let pid = CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUnixProcessID);
    let own = pid == i64::from(std::process::id())
        || CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == MAGIC;
    evidence::transition(Transition {
        layer,
        button,
        pressed,
        event_timestamp: CGEvent::timestamp(Some(ev)),
        timestamp_unit: TimestampUnit::Nanoseconds,
        source_state: Some(CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceStateID)),
        physical: pid == 0,
        own,
        system: if evidence::enabled() {
            evidence::system_state(button)
        } else {
            Default::default()
        },
    });
}

pub(super) fn install(run_loop: &CFRunLoop, shared: &'static Shared) -> Option<CapturePort> {
    boundary(Layer::HidTap, false);
    let mask = [
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
    ]
    .iter()
    .fold(0, |mask, ty| mask | (1 << ty.0));
    // SAFETY: callback borrows events and returns them untouched. No keyboard mask,
    // permissions request, routing callback or input correction is involved.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::HIDEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(callback),
            shared as *const Shared as *mut c_void,
        )
    }?;
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        CFMachPort::invalidate(&port);
        return None;
    };
    PORT.with(|slot| slot.set(CFRetained::as_ptr(&port).as_ptr()));
    // SAFETY: the exported mode constant is valid for this live run loop.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&port, true);
    boundary(Layer::HidTap, CGEvent::tap_is_enabled(&port));
    Some((port, source))
}
pub(super) fn remove(run_loop: &CFRunLoop, (port, source): CapturePort) {
    boundary(Layer::HidTap, false);
    PORT.with(|slot| slot.set(std::ptr::null()));
    CGEvent::tap_enable(&port, false);
    // SAFETY: this source was installed on this loop in the common modes.
    run_loop.remove_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CFMachPort::invalidate(&port);
}
unsafe extern "C-unwind" fn callback(
    _: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    context: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: the same process-lifetime context as the active session tap.
    let shared = unsafe { &*context.cast::<Shared>() };
    if !shared.capture_permitted() {
        return event.as_ptr();
    }
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        boundary(Layer::HidTap, false);
        PORT.with(|slot| {
            // SAFETY: install retains this port on this run loop; remove clears
            // the thread-local pointer before invalidating or releasing it.
            if let Some(port) = unsafe { slot.get().as_ref() } {
                // Re-enable only this listen-only observer, never the input capture tap.
                CGEvent::tap_enable(port, true);
                boundary(Layer::HidTap, CGEvent::tap_is_enabled(port));
            }
        });
    } else {
        // SAFETY: borrowed by CoreGraphics for the duration of this callback.
        observe(Layer::HidTap, ty, unsafe { event.as_ref() });
    }
    event.as_ptr()
}
