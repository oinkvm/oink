//! Ordered native HID mouse input for system typing visibility and keyboard/TSM
//! activation. Native events retain their OS-produced coordinates and identity.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::InputError;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventSourceStateID, CGEventTapLocation, CGEventType};

#[derive(Clone, Copy)]
#[repr(C)]
struct IogPoint {
    x: i16,
    y: i16,
}
#[repr(C, align(8))]
struct NxEventData([u8; 48]);
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDPostEvent(
        connection: u32,
        event_type: u32,
        location: IogPoint,
        data: *const NxEventData,
        version: u32,
        flags: u32,
        options: u32,
    ) -> i32;
}

#[derive(Clone, Copy)]
enum Kind {
    Hid {
        dx: i64,
        dy: i64,
        flags: CGEventFlags,
    },
    HidButton {
        ty: CGEventType,
        button: u32,
        count: i32,
        flags: CGEventFlags,
        submitted: Option<u64>,
    },
    Cg(CGEventType),
}
struct Flight {
    /// CLOCK_UPTIME_RAW, the clock of freshly posted HID/CG events.
    started: u64,
    kind: Kind,
    complete: bool,
    cancelled: bool,
}
impl Flight {
    fn accepts(&self, hid: bool, ty: CGEventType, timestamp: u64) -> bool {
        !self.complete
            && !self.cancelled
            && match self.kind {
                Kind::Hid { .. } => hid && ty == CGEventType::MouseMoved && timestamp >= self.started,
                Kind::HidButton { .. } => false,
                Kind::Cg(expected) => !hid && ty == expected && timestamp == self.started,
            }
    }

    fn accepts_button(&self, ty: CGEventType, button: u32, timestamp: u64) -> bool {
        !self.complete
            && !self.cancelled
            && timestamp >= self.started
            && matches!(self.kind, Kind::HidButton { ty: expected, button: expected_button, submitted, .. }
                if ty == expected && button == expected_button && submitted.is_none_or(|end| timestamp <= end))
    }
}

/// Native submission supplies button data, but the OS rebuilds clickState and
/// eventNum before Session. Restore the intended count in the matched callback;
/// retain the OS event number and location. eventNum is not an ACK identifier.
#[derive(Clone, Copy)]
pub(super) struct NativeButton {
    pub number: u32,
    pub pressed: bool,
    pub count: i32,
    pub event_number: i16,
    pub flags: CGEventFlags,
}

impl NativeButton {
    fn event_type(self) -> CGEventType {
        match (self.number, self.pressed) {
            (0, true) => CGEventType::LeftMouseDown,
            (0, false) => CGEventType::LeftMouseUp,
            (1, true) => CGEventType::RightMouseDown,
            (1, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        }
    }

    fn write(self, data: &mut NxEventData) {
        // SDK v2 NXEventData.mouse offsets: subx/suby 0/1 (already filled),
        // eventNum 2, click 4, pressure 8, buttonNumber 9, subType 10 (zero).
        data.0[2..4].copy_from_slice(&self.event_number.to_ne_bytes());
        data.0[4..8].copy_from_slice(&self.count.to_ne_bytes());
        data.0[8] = if self.pressed { 255 } else { 0 };
        data.0[9] = self.number as u8;
    }
}

#[derive(Default)]
struct Delivery {
    generation: u64,
    active: bool,
    flight: Option<Flight>,
}

/// Returned only after OS submission, so callers update their button ledger
/// even if the subsequent session acknowledgement times out.
pub(super) struct Receipt<'a> {
    motion: &'a Motion,
    started: Option<u64>,
    pub position: Option<CGPoint>,
}
impl Receipt<'_> {
    pub fn wait(self) -> Result<(), InputError> {
        self.started.map_or(Ok(()), |started| self.motion.wait(started))
    }
    /// Held-source transitions require a real delivery observer. An accepted
    /// post without an active tap cannot authorize the next native phase.
    pub fn wait_observed(self) -> Result<(), InputError> {
        self.started
            .ok_or_else(|| InputError::Platform("held source event has no delivery observer".into()))
            .and_then(|started| self.motion.wait(started))
    }
}

/// Shared only with the session tap. Its callback never locks Injector.state.
#[derive(Default)]
pub(super) struct Motion {
    delivery: Mutex<Delivery>,
    delivered: Condvar,
    warned_range: AtomicBool,
    position_epoch: AtomicU64,
    position_context: AtomicU64,
}
impl Motion {
    pub fn new_generation(&self) -> u64 {
        let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        state.generation += 1;
        state.active = false;
        if let Some(f) = state.flight.as_mut() {
            f.cancelled = true;
        }
        self.delivered.notify_all();
        self.reset_position_context();
        state.generation
    }
    pub fn set_active(&self, generation: u64, active: bool) {
        let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != generation {
            return;
        }
        state.active = active;
        if !active {
            if let Some(f) = state.flight.as_mut() {
                f.cancelled = true;
            }
            self.reset_position_context();
        }
        self.delivered.notify_all();
    }
    pub fn invalidate_position(&self) {
        self.position_epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn position_epoch(&self) -> u64 {
        self.position_epoch.load(Ordering::Acquire)
    }
    /// Mode/capture changes abandon a failed old target. Ordinary physical input
    /// only invalidates pending coordinates; it cannot confirm a failed post.
    pub fn reset_position_context(&self) {
        self.position_context.fetch_add(1, Ordering::AcqRel);
        self.invalidate_position();
    }
    pub fn position_context(&self) -> u64 {
        self.position_context.load(Ordering::Acquire)
    }
    fn begin(&self, kind: Kind) -> Result<Option<u64>, InputError> {
        let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        if !state.active || super::tap::in_callback() {
            return Ok(None);
        }
        if state.flight.is_some() {
            return Err(InputError::Platform("input delivery is already pending".into()));
        }
        let started = super::tap::now_event_ns();
        state.flight = Some(Flight {
            started,
            kind,
            complete: false,
            cancelled: false,
        });
        Ok(Some(started))
    }
    fn wait(&self, started: u64) -> Result<(), InputError> {
        let state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        // Bounds failed OS delivery, not cursor hiding. Never replay a timed-out
        // post via the other API. Cancellation shares this mutex with the predicate.
        let (mut state, _) = self
            .delivered
            .wait_timeout_while(state, Duration::from_millis(250), |state| {
                state
                    .flight
                    .as_ref()
                    .is_some_and(|f| f.started == started && !f.complete && !f.cancelled)
            })
            .unwrap_or_else(|e| e.into_inner());
        let complete = if state.flight.as_ref().is_some_and(|f| f.started == started) {
            state.flight.take().is_some_and(|f| f.complete && !f.cancelled)
        } else {
            false
        };
        if complete {
            Ok(())
        } else {
            Err(InputError::Platform(
                "input post was not acknowledged by the session tap".into(),
            ))
        }
    }
    pub fn submit_cg(&self, event: &CGEvent, tap: CGEventTapLocation) -> Result<Receipt<'_>, InputError> {
        let started = self.begin_cg(CGEvent::r#type(Some(event)))?;
        if let Some(started) = started {
            CGEvent::set_timestamp(Some(event), started);
        }
        CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, super::inject::MAGIC);
        CGEvent::post(tap, Some(event));
        Ok(Receipt {
            motion: self,
            started,
            position: None,
        })
    }
    fn begin_cg(&self, ty: CGEventType) -> Result<Option<u64>, InputError> {
        // Secure Event Input deliberately hides ordinary keys from session
        // taps; a missing echo says nothing about password-field delivery.
        // On macOS 14.7.8 the lock screen echoed Shift but not arrow keys;
        // waiting for each missing echo stalled the shared input queue 250 ms
        // per key edge and delayed subsequent pointer/modifier input. A focus
        // or lock-state check cannot close the race with entering secure input.
        // Ordinary CG keys already carry their complete flags at submission
        // and need no tap rewrite. Submit them once in order without an ACK
        // flight; this is not confirmation of password-field delivery. Modifier
        // and mouse delivery retain their existing tap ordering and timeout.
        if matches!(ty, CGEventType::KeyDown | CGEventType::KeyUp) {
            Ok(None)
        } else {
            self.begin(Kind::Cg(ty))
        }
    }
    pub fn post_cg(&self, event: &CGEvent, tap: CGEventTapLocation) -> Result<(), InputError> {
        self.submit_cg(event, tap)?.wait()
    }
    /// No running tap, or a desktop outside IOGPoint's range: retain CG injection.
    pub fn post_mouse(&self, event: &CGEvent, at: CGPoint) -> Result<Option<Receipt<'_>>, InputError> {
        if super::tap::in_callback() || !self.delivery.lock().unwrap_or_else(|e| e.into_inner()).active {
            return Ok(None);
        }
        let Some((location, data)) = absolute_point(at) else {
            if !self.warned_range.swap(true, Ordering::Relaxed) {
                tracing::warn!("pointer target exceeds HID absolute coordinate range; using CG injection");
            }
            return Ok(None);
        };
        let connection = super::capslock::hid::connection()
            .ok_or_else(|| InputError::Platform("IOHIDSystem mouse injection is unavailable".into()))?;
        let field = |f| CGEvent::integer_value_field(Some(event), f);
        let Some(started) = self.begin(Kind::Hid {
            dx: field(CGEventField::MouseEventDeltaX),
            dy: field(CGEventField::MouseEventDeltaY),
            flags: CGEvent::flags(Some(event)),
        })?
        else {
            return Ok(None);
        };
        // AppKit receives CG moves, but those synthetic posts do not reliably run
        // the system's typing-obscure clearing path (CGEventPost, legacy mouse
        // post and warp tested on macOS 14.7.8 / 26.6.2). Changing CG source/tap
        // does not make them native HID movement. This is observed OS behavior;
        // the private WindowServer branch that makes the distinction is unknown.
        // HID + immediate CG races: CG can arrive first and later companions cannot
        // repair hiding. This HID is the movement itself, not a visibility probe.
        // Absolute IOHID reads NXEventData.mouse.subx/suby (bytes 0/1), in 1/256 pt.
        // Do NOT set CGEvent.location in the tap: even a 0.001 pt correction can
        // reproduce the latch: a rewritten native movement loses the observed
        // clearing behavior. Preserve the OS-produced location instead of a CG
        // precision correction. Round inside right/bottom edges, and only restore
        // raw delta/flags/tag. NSCursor.hide counts remain untouched.
        // Native typing verified on macOS 14.7.8 / 26.6.2; installed remote
        // typing also on 14.7.8.
        // Relative games retain their CG path.
        // SAFETY: live connection, initialized SDK v2 union; NX_MOUSEMOVED and
        // kIOHIDSetCursorPosition only (no global modifier flag changes).
        let result = unsafe { IOHIDPostEvent(connection, 5, location, &data, 2, 0, 2) };
        if result != 0 {
            self.delivery.lock().unwrap_or_else(|e| e.into_inner()).flight.take();
            return Err(InputError::Platform(format!("IOHIDPostEvent failed: {result:#x}")));
        }
        Ok(Some(Receipt {
            motion: self,
            started: Some(started),
            position: Some(CGPoint {
                x: f64::from(location.x) + f64::from(data.0[0]) / 256.,
                y: f64::from(location.y) + f64::from(data.0[1]) / 256.,
            }),
        }))
    }

    /// Select CG fallback only before a native submission. A returned receipt
    /// means OS acceptance, not application delivery; never retry after a timeout.
    pub fn post_button(&self, button: NativeButton, at: CGPoint) -> Result<Option<Receipt<'_>>, InputError> {
        if button.number >= 32
            || super::tap::in_callback()
            || !self.delivery.lock().unwrap_or_else(|e| e.into_inner()).active
        {
            return Ok(None);
        }
        let Some((location, mut data)) = absolute_point(at) else {
            if !self.warned_range.swap(true, Ordering::Relaxed) {
                tracing::warn!("button target exceeds HID absolute coordinate range; using CG injection");
            }
            return Ok(None);
        };
        let connection = super::capslock::hid::connection()
            .ok_or_else(|| InputError::Platform("IOHIDSystem button injection is unavailable".into()))?;
        button.write(&mut data);
        let ty = button.event_type();
        let Some(started) = self.begin(Kind::HidButton {
            ty,
            button: button.number,
            count: button.count,
            flags: button.flags,
            submitted: None,
        })?
        else {
            return Ok(None);
        };
        // On macOS 14.7.8, CG clicks can make an app frontmost while its activation
        // check still reports no keyboard focus. TSM remains inactive and the next
        // short Caps source switch fails. CG session/source/flags variants failed;
        // native buttons succeeded with Off, SessionPass and HidPass diagnostics.
        // One native event is the click itself, not a focus/visibility companion.
        // Preserve its location, type and timestamp; only our matching tap
        // supplies modifiers and restores the submitted count after native recounting.
        // The OS-provided source PID is the native ownership marker; NXEventData
        // has no CG user-data field. Do not rewrite native button user-data,
        // global flags, or add movement/hide/unhide companion events.
        // Physical-held releases remain CG Session; dragging/relative movement
        // keep their existing CG paths. Installed receiver tests passed on
        // macOS 14.7.8 and 26.6.2 (Arc activation, Caps and Chinese composition).
        // SAFETY: live connection, by-value SDK IOGPoint and initialized v2 union;
        // only kIOHIDSetCursorPosition, never kIOHIDSetGlobalEventFlags.
        let result = unsafe { IOHIDPostEvent(connection, ty.0, location, &data, 2, 0, 2) };
        let submitted = super::tap::now_event_ns();
        if result != 0 {
            let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
            if state.flight.as_ref().is_some_and(|f| f.started == started) {
                state.flight.take();
            }
            return Err(InputError::Platform(format!(
                "IOHIDPostEvent button failed: {result:#x}"
            )));
        }
        // The kernel timestamps this event before IOHIDPostEvent returns. The
        // callback may already have consumed it; otherwise close this interval
        // so a future/unrelated own button cannot acquire its flags or ACK.
        if let Some(f) = self.delivery.lock().unwrap_or_else(|e| e.into_inner()).flight.as_mut()
            && f.started == started
            && let Kind::HidButton { submitted: end, .. } = &mut f.kind
        {
            *end = Some(submitted);
        }
        let position = CGPoint {
            x: f64::from(location.x) + f64::from(data.0[0]) / 256.,
            y: f64::from(location.y) + f64::from(data.0[1]) / 256.,
        };
        tracing::debug!(
            ?ty,
            button = button.number,
            click_state = button.count,
            flags = button.flags.0,
            started_ns = started,
            submitted_ns = submitted,
            x = position.x,
            y = position.y,
            "button trace: native button submitted; awaiting session tap"
        );
        Ok(Some(Receipt {
            motion: self,
            started: Some(started),
            position: Some(position),
        }))
    }

    /// True means an own native button: always pass it, including unmatched late
    /// UP. Only a matching current flight may supply modifiers or acknowledge.
    pub fn finish_button(&self, generation: u64, ty: CGEventType, event: &CGEvent) -> bool {
        let field = |field| CGEvent::integer_value_field(Some(event), field);
        if !own_button(
            ty.0,
            field(CGEventField::EventSourceStateID),
            field(CGEventField::EventSourceUnixProcessID),
            std::process::id(),
        ) {
            return false;
        }
        // Native IOHIDPostEvent has no CG user-data argument. Its documented
        // source PID, together with HID source state and mouse type, identifies
        // this backend's native carrier. Do not add a CG user-data rewrite here:
        // activation/TSM must retain the native button path validated on macOS
        // 14.7.8 and 26.6.2. Only a matched flight below restores semantic flags
        // and click count, exactly as in that validated path. Late buttons pass
        // untouched; own identity excludes them from capture before routing.
        let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != generation {
            return true;
        }
        let button = u32::try_from(field(CGEventField::MouseEventButtonNumber)).ok();
        let timestamp = CGEvent::timestamp(Some(event));
        let Some(f) = state
            .flight
            .as_mut()
            .filter(|f| button.is_some_and(|b| f.accepts_button(ty, b, timestamp)))
        else {
            tracing::warn!(
                ?ty,
                ?button,
                timestamp_ns = timestamp,
                "button trace: late or unmatched own native button passed without modifying flags"
            );
            return true;
        };
        let Kind::HidButton { flags, count, .. } = f.kind else {
            return true;
        };
        let raw = CGEvent::flags(Some(event));
        // set_flags(0) preserved activation/Caps/composition in the 14.7.8
        // diagnostic, and nonzero Shift/Control/Option/Command reached the DOM.
        // Native absolute posts are re-counted without our target-distance
        // semantics before Session, even when NXEventData.mouse.click was set.
        // Restore the count for this matched submission, including UP, so it
        // reaches AppKit unchanged. Leave OS location/number/time/source intact.
        // This lock is independent of Injector.state.
        CGEvent::set_flags(Some(event), flags);
        CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventClickState, i64::from(count));
        tracing::debug!(
            ?ty,
            ?button,
            timestamp_ns = timestamp,
            raw_flags = raw.0,
            requested_flags = flags.0,
            result_flags = CGEvent::flags(Some(event)).0,
            "button trace: own native button passed session tap; not application acknowledgement"
        );
        f.complete = true;
        self.delivered.notify_all();
        true
    }
    pub fn acknowledge_cg(&self, generation: u64, ty: CGEventType, event: &CGEvent) {
        let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != generation {
            return;
        }
        if let Some(f) = state.flight.as_mut()
            && f.accepts(false, ty, CGEvent::timestamp(Some(event)))
        {
            f.complete = true;
            self.delivered.notify_all();
        }
    }
    /// None: not ours; false: stale/unsolicited own HID; true: populated and acked.
    pub fn finish_mouse(&self, generation: u64, ty: CGEventType, event: &CGEvent) -> Option<bool> {
        let field = |f| CGEvent::integer_value_field(Some(event), f);
        if !own_mouse(
            ty.0,
            field(CGEventField::EventSourceStateID),
            field(CGEventField::EventSourceUnixProcessID),
            std::process::id(),
        ) {
            return None;
        }
        // Tag even a stale carrier: it is ours regardless of flight matching.
        CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, super::inject::MAGIC);
        let mut state = self.delivery.lock().unwrap_or_else(|e| e.into_inner());
        // A detached old tap must pass a newer tap's carrier untouched.
        if state.generation != generation {
            return Some(true);
        }
        let Some(f) = state
            .flight
            .as_mut()
            .filter(|f| f.accepts(true, ty, CGEvent::timestamp(Some(event))))
        else {
            return Some(false);
        };
        let Kind::Hid { dx, dy, flags } = f.kind else {
            return Some(false);
        };
        CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventDeltaX, dx);
        CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventDeltaY, dy);
        CGEvent::set_flags(Some(event), flags);
        CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, super::inject::MAGIC);
        // Kernel supplies PID. Only one HID is in flight, so it cannot coalesce
        // with the next. A timed-out prior post has an older timestamp and is
        // dropped, never given a newer movement's metadata. eventNum is rewritten
        // by WindowServer, so it cannot be used as a sequence tag.
        f.complete = true;
        self.delivered.notify_all();
        Some(true)
    }
}
fn absolute_point(at: CGPoint) -> Option<(IogPoint, NxEventData)> {
    fn component(v: f64) -> Option<(i16, u8)> {
        let integer = v.floor();
        (v.is_finite() && integer >= f64::from(i16::MIN) && integer <= f64::from(i16::MAX))
            .then(|| (integer as i16, ((v - integer) * 256.).floor() as u8))
    }
    let (x, subx) = component(at.x)?;
    let (y, suby) = component(at.y)?;
    let mut data = NxEventData([0; 48]);
    data.0[0] = subx;
    data.0[1] = suby;
    Some((IogPoint { x, y }, data))
}
fn own_mouse(ty: u32, source: i64, pid: i64, own_pid: u32) -> bool {
    ty == CGEventType::MouseMoved.0
        && source == i64::from(CGEventSourceStateID::HIDSystemState.0)
        && pid == i64::from(own_pid)
}
fn own_button(ty: u32, source: i64, pid: i64, own_pid: u32) -> bool {
    matches!(ty, 1 | 2 | 3 | 4 | 25 | 26)
        && source == i64::from(CGEventSourceStateID::HIDSystemState.0)
        && pid == i64::from(own_pid)
}
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn held_source_requires_observed_delivery_not_just_an_accepted_post() {
        let motion = Motion::default();
        let receipt = Receipt {
            motion: &motion,
            started: None,
            position: None,
        };
        assert!(receipt.wait_observed().is_err());
    }
    #[test]
    fn native_button_source_identity_excludes_echo_without_rewriting_user_data() {
        let motion = Motion::default();
        let generation = motion.new_generation();
        for ty in [
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
        ] {
            let event = CGEvent::new(None).unwrap();
            CGEvent::set_type(Some(&event), ty);
            for (field, value) in [
                (CGEventField::EventSourceStateID, 1),
                (CGEventField::EventSourceUnixProcessID, i64::from(std::process::id())),
            ] {
                CGEvent::set_integer_value_field(Some(&event), field, value);
            }
            let before = CGEvent::new_data(None, Some(&event)).unwrap().to_vec();
            assert!(motion.finish_button(generation, ty, &event));
            assert_eq!(
                CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData),
                0
            );
            assert_eq!(CGEvent::new_data(None, Some(&event)).unwrap().to_vec(), before);
        }
    }

    #[test]
    fn native_normalization_does_not_claim_foreign_or_physical_events() {
        let motion = Motion::default();
        let generation = motion.new_generation();
        for pid in [0, i64::from(std::process::id()) + 1] {
            for source in [-1, 0, 1] {
                let event = CGEvent::new(None).unwrap();
                for (field, value) in [
                    (CGEventField::EventSourceStateID, source),
                    (CGEventField::EventSourceUnixProcessID, pid),
                    (CGEventField::EventSourceUserData, 0x1234),
                ] {
                    CGEvent::set_integer_value_field(Some(&event), field, value);
                }
                assert!(!motion.finish_button(generation, CGEventType::LeftMouseUp, &event));
                assert_eq!(motion.finish_mouse(generation, CGEventType::MouseMoved, &event), None);
                assert_eq!(
                    CGEvent::integer_value_field(Some(&event), CGEventField::EventSourceUserData),
                    0x1234
                );
            }
        }
    }

    #[test]
    fn native_recount_must_not_replace_the_submitted_click_state() {
        let motion = Motion::default();
        let generation = motion.new_generation();
        motion.delivery.lock().unwrap().flight = Some(Flight {
            started: 100,
            kind: Kind::HidButton {
                ty: CGEventType::LeftMouseDown,
                button: 0,
                count: 1,
                flags: CGEventFlags(0),
                submitted: Some(110),
            },
            complete: false,
            cancelled: false,
        });
        // The native packet submitted count=1 at a different target. The OS
        // rebuilt count=2 before Session; AppKit must receive the submitted 1.
        let event = CGEvent::new_mouse_event(
            None,
            CGEventType::LeftMouseDown,
            CGPoint { x: 20., y: 40. },
            objc2_core_graphics::CGMouseButton::Left,
        )
        .unwrap();
        CGEvent::set_timestamp(Some(&event), 105);
        for (field, value) in [
            (CGEventField::EventSourceStateID, 1),
            (CGEventField::EventSourceUnixProcessID, i64::from(std::process::id())),
            (CGEventField::MouseEventClickState, 2),
        ] {
            CGEvent::set_integer_value_field(Some(&event), field, value);
        }
        assert!(motion.finish_button(generation, CGEventType::LeftMouseDown, &event));
        assert_eq!(
            CGEvent::integer_value_field(Some(&event), CGEventField::MouseEventClickState),
            1
        );
    }

    #[test]
    fn native_session_preserves_calculated_counts_and_os_button_identity() {
        for number in [0, 1, 2, 4, 31] {
            for pressed in [true, false] {
                for count in [0, 1, 2, 3, 7] {
                    if pressed && count == 0 {
                        continue;
                    }
                    let button = NativeButton {
                        number,
                        pressed,
                        count,
                        event_number: 123,
                        flags: CGEventFlags::MaskControl,
                    };
                    let motion = Motion::default();
                    let generation = motion.new_generation();
                    motion.delivery.lock().unwrap().flight = Some(Flight {
                        started: 100,
                        kind: Kind::HidButton {
                            ty: button.event_type(),
                            button: number,
                            count,
                            flags: button.flags,
                            submitted: Some(110),
                        },
                        complete: false,
                        cancelled: false,
                    });
                    let event = CGEvent::new_mouse_event(
                        None,
                        button.event_type(),
                        CGPoint { x: 20.25, y: 40.5 },
                        objc2_core_graphics::CGMouseButton::Left,
                    )
                    .unwrap();
                    CGEvent::set_timestamp(Some(&event), 105);
                    for (field, value) in [
                        (CGEventField::EventSourceStateID, 1),
                        (CGEventField::EventSourceUnixProcessID, i64::from(std::process::id())),
                        (CGEventField::MouseEventButtonNumber, i64::from(number)),
                        (CGEventField::MouseEventClickState, 64),
                        // Native also replaces the submitted number. Keep its
                        // matching OS DOWN/UP identity rather than forging 123.
                        (CGEventField::MouseEventNumber, -32459),
                    ] {
                        CGEvent::set_integer_value_field(Some(&event), field, value);
                    }
                    assert!(motion.finish_button(generation, button.event_type(), &event));
                    assert_eq!(
                        CGEvent::integer_value_field(Some(&event), CGEventField::MouseEventClickState),
                        i64::from(count)
                    );
                    assert_eq!(
                        CGEvent::integer_value_field(Some(&event), CGEventField::MouseEventNumber),
                        -32459
                    );
                    assert_eq!(CGEvent::location(Some(&event)), CGPoint { x: 20.25, y: 40.5 });
                    assert_eq!(CGEvent::timestamp(Some(&event)), 105);
                    assert_eq!(CGEvent::flags(Some(&event)), button.flags);
                    // A duplicate/late own event cannot take metadata from an
                    // already acknowledged flight, even of the same button.
                    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, 65);
                    assert!(motion.finish_button(generation, button.event_type(), &event));
                    assert_eq!(
                        CGEvent::integer_value_field(Some(&event), CGEventField::MouseEventClickState),
                        65
                    );
                }
            }
        }
    }
    #[test]
    fn keyboard_without_session_echo_cannot_leave_a_flight_blocking_mouse_input() {
        let motion = Motion::default();
        let generation = motion.new_generation();
        motion.set_active(generation, true);
        // Repeated ordinary keys and their releases can all be
        // posted without any tap callback, as in Secure Event Input.
        for ty in [CGEventType::KeyDown, CGEventType::KeyDown, CGEventType::KeyUp] {
            assert!(motion.begin_cg(ty).unwrap().is_none());
            assert!(motion.delivery.lock().unwrap().flight.is_none());
        }
        // Mouse posts still require their own echo; skipping key ACKs must not
        // turn an unobserved mouse submission into an acknowledged one.
        assert!(motion.begin_cg(CGEventType::LeftMouseDragged).unwrap().is_some());
        assert!(motion.begin_cg(CGEventType::LeftMouseUp).is_err());
        let mut state = motion.delivery.lock().unwrap();
        assert!(!state.flight.take().unwrap().complete);
        drop(state);
        assert!(motion.begin_cg(CGEventType::FlagsChanged).unwrap().is_some());
    }
    #[test]
    fn fraction_preserves_negative_screens_and_inside_edges() {
        for (x, y) in [(1728. - 1e-6, 1117. - 1e-6), (-0.001, -1080.001), (-32768., 32767.999)] {
            let (p, d) = absolute_point(CGPoint { x, y }).unwrap();
            for (v, actual) in [
                (x, f64::from(p.x) + f64::from(d.0[0]) / 256.),
                (y, f64::from(p.y) + f64::from(d.0[1]) / 256.),
            ] {
                assert!(actual <= v && v - actual < 1. / 256.);
            }
        }
        for x in [f64::NAN, f64::INFINITY, -32768.1, 32768.] {
            assert!(absolute_point(CGPoint { x, y: 0. }).is_none());
        }
    }
    #[test]
    fn late_ack_cannot_take_a_new_posts_delta() {
        let mut f = Flight {
            started: 100,
            kind: Kind::Hid {
                dx: 4,
                dy: 5,
                flags: CGEventFlags(0),
            },
            complete: false,
            cancelled: false,
        };
        assert!(!f.accepts(true, CGEventType::MouseMoved, 99));
        assert!(!f.accepts(false, CGEventType::MouseMoved, 101));
        assert!(f.accepts(true, CGEventType::MouseMoved, 101));
        f.complete = true;
        assert!(!f.accepts(true, CGEventType::MouseMoved, 101));
        f.kind = Kind::Cg(CGEventType::KeyUp);
        f.complete = false;
        assert!(!f.accepts(false, CGEventType::KeyUp, 101));
        assert!(!f.accepts(false, CGEventType::KeyDown, 100));
        assert!(f.accepts(false, CGEventType::KeyUp, 100));
    }
    #[test]
    fn stopped_generation_cannot_ack_or_disable_its_replacement() {
        let motion = Motion::default();
        let old = motion.new_generation();
        motion.set_active(old, true);
        let started = motion.begin(Kind::Cg(CGEventType::KeyUp)).unwrap().unwrap();
        motion.set_active(old, false);
        let new = motion.new_generation();
        motion.set_active(new, true);
        motion.set_active(old, false);
        assert!(motion.wait(started).is_err());
        let state = motion.delivery.lock().unwrap();
        assert!(state.active);
        assert_eq!(state.generation, new);
    }

    #[test]
    fn hardware_other_processes_and_service_synth_are_not_carriers() {
        assert!(own_mouse(5, 1, 42, 42));
        assert!(!own_mouse(5, 1, 0, 42));
        assert!(!own_mouse(5, 1, 43, 42));
        assert!(!own_mouse(5, 0, 42, 42));
        assert!(!own_mouse(1, 1, 42, 42));
    }

    #[test]
    fn native_button_packet_preserves_fraction_and_encodes_full_click() {
        for (number, down, ty) in [
            (0, true, 1),
            (0, false, 2),
            (1, true, 3),
            (1, false, 4),
            (4, true, 25),
            (4, false, 26),
        ] {
            let (_, mut data) = absolute_point(CGPoint { x: -0.25, y: 1.5 }).unwrap();
            let button = NativeButton {
                number,
                pressed: down,
                count: 3,
                event_number: 123,
                flags: CGEventFlags(0),
            };
            button.write(&mut data);
            assert_eq!(button.event_type().0, ty);
            assert_eq!(&data.0[..2], &[192, 128]);
            assert_eq!(i16::from_ne_bytes(data.0[2..4].try_into().unwrap()), 123);
            assert_eq!(i32::from_ne_bytes(data.0[4..8].try_into().unwrap()), 3);
            assert_eq!(data.0[8], if down { 255 } else { 0 });
            assert_eq!(data.0[9], number as u8);
            assert!(data.0[10..].iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn native_button_match_requires_kind_number_and_submission_interval() {
        let mut flight = Flight {
            started: 100,
            kind: Kind::HidButton {
                ty: CGEventType::OtherMouseUp,
                button: 4,
                count: 3,
                flags: CGEventFlags(0),
                submitted: None,
            },
            complete: false,
            cancelled: false,
        };
        assert!(flight.accepts_button(CGEventType::OtherMouseUp, 4, 110)); // callback before post returns
        if let Kind::HidButton { submitted, .. } = &mut flight.kind {
            *submitted = Some(120);
        }
        assert!(!flight.accepts_button(CGEventType::OtherMouseUp, 4, 99)); // old timed-out UP
        assert!(!flight.accepts_button(CGEventType::OtherMouseUp, 4, 121));
        assert!(!flight.accepts_button(CGEventType::OtherMouseDown, 4, 110));
        assert!(!flight.accepts_button(CGEventType::OtherMouseUp, 3, 110));
        assert!(!flight.accepts(true, CGEventType::OtherMouseUp, 110));
        for timestamp in [100, 110, 120] {
            assert!(flight.accepts_button(CGEventType::OtherMouseUp, 4, timestamp));
        }
        flight.cancelled = true;
        assert!(!flight.accepts_button(CGEventType::OtherMouseUp, 4, 110));
        flight.cancelled = false;
        flight.complete = true;
        assert!(!flight.accepts_button(CGEventType::OtherMouseUp, 4, 110));
    }

    #[test]
    fn obsolete_waiter_cannot_clear_a_new_flight() {
        let motion = Motion::default();
        motion.delivery.lock().unwrap().flight = Some(Flight {
            started: 200,
            kind: Kind::Cg(CGEventType::KeyUp),
            complete: false,
            cancelled: false,
        });
        assert!(motion.wait(100).is_err());
        assert_eq!(motion.delivery.lock().unwrap().flight.as_ref().unwrap().started, 200);
    }

    #[test]
    fn native_button_provenance_excludes_physical_other_process_and_cg_events() {
        for ty in [1, 2, 3, 4, 25, 26] {
            assert!(own_button(ty, 1, 42, 42));
            assert!(!own_button(ty, 1, 0, 42));
            assert!(!own_button(ty, 1, 43, 42));
            assert!(!own_button(ty, 0, 42, 42));
        }
        for ty in [5, 6, 7, 10, 11, 27] {
            assert!(!own_button(ty, 1, 42, 42));
        }
    }
}
