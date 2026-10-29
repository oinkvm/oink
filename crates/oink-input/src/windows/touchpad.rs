//! Runtime-bound precision touchpad API (Windows 11; tested on build 26200).
//! The SDK shipped with windows-rs predates CreateSyntheticPointerDevice2. The
//! ABI below follows Microsoft's Precision Touchpad Programming Guide. Resolve
//! every export before creating a device, so older Windows keeps wheel delivery.
use std::ffi::c_void;
use std::time::{Duration, Instant};

use oink_foundation::input::ScrollPhase;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Registry::HKEY_CURRENT_USER;
use windows::Win32::UI::Input::Pointer::{POINTER_FLAGS, POINTER_TOUCH_INFO};
use windows::Win32::UI::WindowsAndMessaging::POINTER_INPUT_TYPE;
use windows::core::{s, w};

use crate::touchpad::{Frame, Pan, Sink};
use crate::{ContactInitError, ContactInjectionStatus};

// Only device-less/unknown wheel input uses this ownership period. A matching
// physical mouse raw report always wins. See the real-device origin measurements.
// Build 26200 LISTBOX reached 431.986ms after end under load. Two seconds leaves
// >1.5s margin; a receiver's own physical PTP is ambiguous inside this interval.
const AMBIGUOUS_TAIL: Duration = Duration::from_secs(2);

#[repr(C)]
struct Parameters {
    kind: u32,
    count: u32,
    feedback: u32,
    monitor: *mut c_void,
    width: u32,
    height: u32,
    options: u32,
}

// TOUCH_INFO is the largest member of POINTER_TYPE_INFO's union. Keeping that
// member alone preserves the C union's size/alignment, including on ARM64.
#[repr(C)]
#[derive(Default)]
struct Pointer {
    kind: u32,
    touch: POINTER_TOUCH_INFO,
}

type Create = unsafe extern "system" fn(*const Parameters) -> *mut c_void;
type Inject = unsafe extern "system" fn(*mut c_void, *const Pointer, u32) -> i32;
type Action = unsafe extern "system" fn(*mut c_void, u32) -> i32;
type Destroy = unsafe extern "system" fn(*mut c_void);

struct Device {
    handle: *mut c_void,
    inject: Inject,
    action: Action,
    destroy: Destroy,
    failed: bool,
}

impl Device {
    fn create() -> Result<Self, ContactInitError> {
        Self::create_for(2, 1_000_000, 1_000_000)
    }
    fn create_for(count: u32, width: u32, height: u32) -> Result<Self, ContactInitError> {
        // SAFETY: user32 is loaded by the backend and remains loaded for process
        // lifetime. Each typed function exactly matches its documented C ABI.
        unsafe {
            let user32 = GetModuleHandleW(w!("user32.dll")).map_err(|_| ContactInitError::User32Unavailable)?;
            let create: Create = std::mem::transmute(
                GetProcAddress(user32, s!("CreateSyntheticPointerDevice2"))
                    .ok_or(ContactInitError::CreateApiUnavailable)?,
            );
            let inject: Inject = std::mem::transmute(
                GetProcAddress(user32, s!("InjectSyntheticPointerInput"))
                    .ok_or(ContactInitError::InjectApiUnavailable)?,
            );
            let action: Action = std::mem::transmute(
                GetProcAddress(user32, s!("InjectTouchpadAction")).ok_or(ContactInitError::ActionApiUnavailable)?,
            );
            let destroy: Destroy = std::mem::transmute(
                GetProcAddress(user32, s!("DestroySyntheticPointerDevice"))
                    .ok_or(ContactInitError::DestroyApiUnavailable)?,
            );
            let params = Parameters {
                kind: 5,
                count,
                feedback: 3,
                monitor: std::ptr::null_mut(),
                width,
                height,
                options: 3,
            };
            let handle = create(&params);
            if handle.is_null() {
                tracing::warn!(error = ?windows::core::Error::from_thread(), count, "synthetic touchpad creation rejected");
                super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceCreate);
                return Err(ContactInitError::DeviceCreationRejected);
            }
            super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceCreate);
            Ok(Self {
                handle,
                inject,
                action,
                destroy,
                failed: false,
            })
        }
    }
}

impl Sink for Device {
    fn frame(&mut self, frame: Frame) -> bool {
        let pointers = [0, 1].map(|i| {
            let mut p = Pointer {
                kind: 5,
                ..Pointer::default()
            };
            p.touch.pointerInfo.pointerType = POINTER_INPUT_TYPE(5);
            p.touch.pointerInfo.pointerId = i + 1;
            // Touchpad frames describe contact state; touch/pen DOWN/UPDATE flags
            // are not used (Microsoft sample). CANCEL avoids system inertia:
            // UP followed by STOP/END still produced a tail on build 26200.
            p.touch.pointerInfo.pointerFlags = POINTER_FLAGS(if frame.cancel { 0xc000 } else { 0x4006 });
            p.touch.pointerInfo.ptHimetricLocation = POINT {
                x: frame.x + i as i32 * 1000,
                y: frame.y,
            };
            p.touch.pointerInfo.dwTime = frame.tick;
            p
        });
        // SAFETY: device owns this handle; two initialized frames live for call.
        let ok = unsafe { (self.inject)(self.handle, pointers.as_ptr(), 2) != 0 };
        super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceInject);
        if !ok {
            self.failed = true;
        }
        ok
    }
    fn stop_inertia(&mut self) {
        // SAFETY: valid owned synthetic device; TA_INERTIA_STOP/END from new API.
        unsafe {
            let stopped = (self.action)(self.handle, 9) != 0;
            let ended = (self.action)(self.handle, 10) != 0;
            self.failed |= !stopped || !ended;
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: sole owner, destroyed once, on the creating hook thread.
        unsafe { (self.destroy)(self.handle) };
        super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceDestroy);
    }
}

struct ScrollContext {
    device: Option<Device>,
    pan: Pan,
    last_input: Option<Instant>,
    ended: Option<Instant>,
    pan_scale: f64,
    direction: f64,
}

impl ScrollContext {
    #[cfg(all(test, debug_assertions))]
    pub fn unavailable() -> Self {
        Self {
            device: None,
            pan: Pan::default(),
            last_input: None,
            ended: None,
            pan_scale: 1.0,
            direction: 1.0,
        }
    }
    pub fn new() -> Self {
        let device = match Device::create() {
            Ok(d) => {
                tracing::info!("native precision touchpad available");
                Some(d)
            }
            Err(reason) => {
                tracing::info!(%reason, "native precision touchpad unavailable; using wheel");
                None
            }
        };
        Self {
            device,
            pan: Pan::default(),
            last_input: None,
            ended: None,
            pan_scale: 1.0,
            direction: 1.0,
        }
    }
    pub fn cancel(&mut self) {
        self.finish(false);
    }
    fn finish(&mut self, expired: bool) {
        let was_active = self.pan.active();
        if let Some(device) = &mut self.device {
            if expired {
                self.pan.expire(device);
            } else {
                self.pan.cancel(device);
            }
        }
        if was_active {
            self.ended = Some(Instant::now());
        }
        if self.device.as_ref().is_some_and(|device| device.failed) {
            // A failed cancellation cannot leave contacts alive after handoff
            // waiting for another scroll to trigger the regular fallback path.
            self.device = None;
            tracing::warn!("native precision touchpad cancellation failed; using wheel until restart");
        }
        self.last_input = None;
    }
    pub fn owns_ambiguous_wheel(&self, now: Instant) -> bool {
        self.pan.active()
            || self
                .ended
                .is_some_and(|end| now.saturating_duration_since(end) < AMBIGUOUS_TAIL)
    }
    pub fn wheel_tail_age(&self, now: Instant) -> Option<Duration> {
        if self.pan.active() {
            None
        } else {
            self.ended.map(|end| now.saturating_duration_since(end))
        }
    }
    pub fn expire(&mut self) {
        if self.last_input.is_some_and(|t| t.elapsed() >= self.pan.idle_timeout()) {
            self.finish(true);
        }
    }
    pub fn scroll(&mut self, pixels: (f64, f64), dpi: u32, phase: ScrollPhase) -> (f64, f64) {
        self.expire();
        if self.device.is_none() {
            return (1.0, 1.0);
        }
        if !self.pan.active() || phase == ScrollPhase::Began {
            // Windows' 2026 touchpad Scroll speed setting is independent of the
            // wheel lines and pointer speed. Read its persisted value per pan;
            // the public TOUCHPAD_PARAMETERS_V2 has no field for it. Defaults
            // and piecewise scaling were checked in ninput 10.0.26100.9278.
            // No settings are written, and no private SPI structure is used.
            let sensitivity = super::settings::reg_dword(
                HKEY_CURRENT_USER,
                r"Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad",
                "PanSensitivity",
            )
            .unwrap_or(50)
            .min(100);
            self.pan_scale = if sensitivity <= 50 {
                0.5 + f64::from(sensitivity) / 100.0
            } else {
                f64::from(sensitivity) / 50.0
            };
            self.direction = if super::settings::touchpad_natural_scroll().unwrap_or(true) {
                1.0
            } else {
                -1.0
            };
            tracing::debug!(
                sensitivity,
                dpi,
                direction = self.direction,
                "native touchpad pan settings"
            );
        }
        let Some(device) = &mut self.device else {
            return (1.0, 1.0);
        };
        let was_active = self.pan.active();
        let remaining = self.pan.scroll(
            device,
            (pixels.0 * self.direction, pixels.1 * self.direction),
            f64::from(dpi.max(1)) / 2540.0 * self.pan_scale,
            phase,
        );
        let submitted_motion =
            remaining == (0.0, 0.0) && pixels != (0.0, 0.0) && pixels.0.is_finite() && pixels.1.is_finite();
        if (was_active || submitted_motion) && !self.pan.active() {
            self.ended = Some(Instant::now());
        }
        self.last_input = Some(Instant::now());
        if device.failed {
            self.cancel();
            self.device = None;
            tracing::warn!("native precision touchpad injection failed; using wheel until restart");
        }
        remaining
    }
}
impl Drop for ScrollContext {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(super) struct Touchpad {
    contact: ScrollContext,
    raw: Raw,
}
impl Touchpad {
    pub fn new() -> Self {
        Self {
            contact: ScrollContext::new(),
            raw: Raw::new(),
        }
    }
    #[cfg(all(test, debug_assertions))]
    pub fn unavailable() -> Self {
        Self {
            contact: ScrollContext::unavailable(),
            raw: Raw::unavailable(),
        }
    }
    pub fn contacts(&mut self, frame: &oink_foundation::contacts::Frame) -> bool {
        self.raw.apply(frame)
    }
    pub fn cancel_all(&mut self) {
        self.contact.cancel();
        self.raw.cancel();
    }
    pub fn expire(&mut self) {
        self.contact.expire();
    }
    pub fn owns_ambiguous_wheel(&self, now: Instant) -> bool {
        self.raw.owns_wheel(now) || self.contact.owns_ambiguous_wheel(now)
    }
    pub fn wheel_tail_age(&self, now: Instant) -> Option<Duration> {
        if self.raw.receiver.active() || self.contact.pan.active() {
            return None;
        }
        self.raw
            .ended
            .map(|t| now.saturating_duration_since(t))
            .into_iter()
            .chain(self.contact.wheel_tail_age(now))
            .min()
    }
    pub fn scroll(&mut self, pixels: (f64, f64), dpi: u32, phase: ScrollPhase) -> (f64, f64) {
        self.contact.scroll(pixels, dpi, phase)
    }
    pub fn finish_scroll(&mut self) {
        // Timer-arm failure must release promptly, but a late source tail can
        // still use wheel fallback rather than silently losing displacement.
        self.contact.finish(true);
    }
    pub fn awaiting_momentum(&self) -> bool {
        self.contact.pan.awaiting_momentum()
    }
}

// Independent from ScrollContext: no inverse gain, lead-in, 96-ms clock, inertia
// STOP action or idle timeout may be applied to these physical contacts.
use oink_foundation::contacts::{Frame as ContactFrame, Surface};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
static CONTACT_DESKTOP: AtomicBool = AtomicBool::new(false);
pub(super) fn contact_desktop(available: bool) {
    CONTACT_DESKTOP.store(available, Ordering::Release);
}
static CONTACT_CAPACITY: AtomicU8 = AtomicU8::new(0);
// 0 = not probed, 1 = startup creation succeeded; other values are error kinds.
static CONTACT_INIT: AtomicU8 = AtomicU8::new(0);
pub(super) fn contact_status() -> ContactInjectionStatus {
    let init = CONTACT_INIT.load(Ordering::Acquire);
    contact_status_from_probe(init, contact_capacity())
}
fn contact_status_from_probe(init: u8, capacity: Option<u8>) -> ContactInjectionStatus {
    let capacity = capacity.filter(|_| init == 1);
    let init_error_kind = match init {
        0 | 1 => None,
        3 => Some(ContactInitError::User32Unavailable),
        4 => Some(ContactInitError::CreateApiUnavailable),
        5 => Some(ContactInitError::InjectApiUnavailable),
        6 => Some(ContactInitError::ActionApiUnavailable),
        7 => Some(ContactInitError::DestroyApiUnavailable),
        _ => Some(ContactInitError::DeviceCreationRejected),
    };
    ContactInjectionStatus {
        supported: capacity == Some(5),
        initialized: init == 1,
        max_contacts: capacity,
        init_error_kind,
    }
}
pub(super) fn contact_capacity() -> Option<u8> {
    if !CONTACT_DESKTOP.load(Ordering::Acquire) {
        return None;
    }
    match CONTACT_CAPACITY.load(Ordering::Acquire) {
        5 => Some(5),
        _ => None,
    }
}
struct NativeContacts {
    init_error_kind: Option<ContactInitError>,
    device: Option<Device>,
    surface: Surface,
}
impl crate::contacts::Sink for NativeContacts {
    fn create(&mut self, surface: Surface) -> bool {
        if self.device.is_some() && self.surface == surface {
            return true;
        }
        self.device = None;
        self.surface = surface;
        // A reference quantum is exactly one native HIMETRIC unit; keep the
        // integer boundary conversion exact rather than round through pixels.
        self.device = match Device::create_for(5, surface.width, surface.height) {
            Ok(device) => {
                self.init_error_kind = None;
                Some(device)
            }
            Err(error) => {
                self.init_error_kind = Some(error);
                None
            }
        };
        self.device.is_some()
    }
    fn frame(&mut self, contacts: &[crate::contacts::Pointer], tick: u32) -> bool {
        let Some(device) = &mut self.device else {
            return false;
        };
        let mut pointers: [Pointer; 5] = std::array::from_fn(|_| Pointer::default());
        for (p, c) in pointers.iter_mut().zip(contacts) {
            p.kind = 5;
            p.touch.pointerInfo.pointerType = POINTER_INPUT_TYPE(5);
            p.touch.pointerInfo.pointerId = c.id;
            p.touch.pointerInfo.pointerFlags = POINTER_FLAGS(c.flags);
            p.touch.pointerInfo.ptHimetricLocation = POINT { x: c.x, y: c.y };
            p.touch.pointerInfo.dwTime = tick;
        }
        // SAFETY: one owned gesture-only device, at most five initialized pointers.
        let ok = unsafe { (device.inject)(device.handle, pointers.as_ptr(), contacts.len() as u32) != 0 };
        if !ok {
            tracing::warn!(error = ?windows::core::Error::from_thread(), "raw touchpad injection rejected");
        }
        super::mouse_keys::edge(crate::mouse_keys::Reason::DeviceInject);
        ok
    }
    fn close(&mut self) {
        self.device = None;
    }
}
struct Raw<S: crate::contacts::Sink = NativeContacts> {
    receiver: crate::contacts::Receiver,
    sink: S,
    available: bool,
    ended: Option<Instant>,
    frames: u64,
}
impl Raw {
    fn unavailable() -> Self {
        Self {
            receiver: Default::default(),
            sink: NativeContacts {
                init_error_kind: None,
                device: None,
                surface: Surface::default(),
            },
            available: false,
            ended: None,
            frames: 0,
        }
    }
    fn new() -> Self {
        use crate::contacts::Sink;
        let mut raw = Self::unavailable();
        // Creation is the runtime capability probe; no input is injected. Actual
        // source dimensions replace this descriptor on the first raw Begin.
        raw.available = raw.sink.create(Surface {
            width: 10_000,
            height: 6_000,
        });
        CONTACT_CAPACITY.store(if raw.available { 5 } else { 0 }, Ordering::Release);
        CONTACT_INIT.store(
            raw.sink.init_error_kind.map_or(1, |error| error as u8),
            Ordering::Release,
        );
        tracing::info!(
            supported = raw.available,
            initialized = raw.available,
            init_error_kind = ?raw.sink.init_error_kind,
            "gesture-only touchpad contacts capability"
        );
        raw
    }
}
impl<S: crate::contacts::Sink> Raw<S> {
    fn apply(&mut self, frame: &ContactFrame) -> bool {
        if !self.available {
            return false;
        }
        if frame.phase == oink_foundation::contacts::Phase::Begin {
            self.frames = 0;
        }
        let was_active = self.receiver.active();
        let ok = self.receiver.apply(frame, &mut self.sink);
        self.frames += 1;
        if !ok {
            self.cancel();
            self.available = false;
            CONTACT_CAPACITY.store(0, Ordering::Release);
        }
        if frame.phase == oink_foundation::contacts::Phase::Begin || frame.phase.ends() || !ok {
            use crate::native_lifecycle::{Data, Kind, emit_lifecycle};
            let message = match frame.phase {
                oink_foundation::contacts::Phase::Begin => 0,
                oink_foundation::contacts::Phase::Move => 1,
                oink_foundation::contacts::Phase::End => 2,
                oink_foundation::contacts::Phase::Cancel => 3,
            };
            // The capture owner suppresses formatted logging. Record only native
            // lifecycle results, never the per-frame contact geometry.
            emit_lifecycle(
                Kind::ContactInjection,
                Data {
                    message,
                    extra: self.frames,
                    success: Some(ok),
                    ..Default::default()
                },
            );
        }
        // Windows 11 build 26200 promotes native contacts to untagged wheels,
        // including after release. Retain the bounded tail only for a real
        // active-to-idle boundary (or possible partial failure). A foreign or
        // repeated idle Cancel submits no contacts and must not reopen the tail:
        // that would withhold unrelated source wheels without a native gesture.
        if (was_active && !self.receiver.active()) || !ok {
            self.ended = Some(Instant::now());
            tracing::info!(frames = self.frames, success = ok, phase = ?frame.phase, "raw touchpad gesture submitted");
        }
        ok
    }
    fn cancel(&mut self) {
        if self.receiver.active() {
            tracing::info!(
                frames = self.frames,
                "raw touchpad gesture cancelled at lifecycle boundary"
            );
            self.ended = Some(Instant::now());
        }
        if !self.receiver.cancel(&mut self.sink) {
            self.available = false;
            CONTACT_CAPACITY.store(0, Ordering::Release);
        }
    }
    fn owns_wheel(&self, now: Instant) -> bool {
        self.receiver.active()
            || self
                .ended
                .is_some_and(|t| now.saturating_duration_since(t) < AMBIGUOUS_TAIL)
    }
}
impl<S: crate::contacts::Sink> Drop for Raw<S> {
    fn drop(&mut self) {
        self.cancel();
        CONTACT_CAPACITY.store(0, Ordering::Release);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::contacts::{Contact, Frame, Sample};

    #[derive(Default)]
    struct Recording {
        frames: usize,
        fail: bool,
    }
    impl crate::contacts::Sink for Recording {
        fn create(&mut self, _: Surface) -> bool {
            !self.fail
        }
        fn frame(&mut self, _: &[crate::contacts::Pointer], _: u32) -> bool {
            self.frames += 1;
            !self.fail
        }
        fn close(&mut self) {}
    }
    fn raw() -> Raw<Recording> {
        Raw {
            receiver: Default::default(),
            sink: Recording::default(),
            available: true,
            ended: None,
            frames: 0,
        }
    }
    fn begin(raw: &mut Raw<Recording>) -> Sample {
        let mut sample = Sample {
            device: 1,
            surface: Surface {
                width: 15_600,
                height: 9_600,
            },
            time_us: 1_000,
            len: 3,
            ..Default::default()
        };
        for (i, contact) in sample.contacts[..3].iter_mut().enumerate() {
            *contact = Contact {
                id: i as u32 + 1,
                x: 1000 + i as i32 * 500,
                y: 2000,
            };
        }
        assert!(raw.apply(&Frame::between(None, &sample)));
        sample
    }
    #[test]
    fn ignored_contact_cancel_never_opens_or_renews_wheel_ownership() {
        let mut raw = raw();
        assert!(raw.apply(&Frame::cancel(99)));
        assert_eq!(raw.sink.frames, 0);
        assert_eq!(raw.ended, None);
        assert!(!raw.owns_wheel(Instant::now()));

        let sample = begin(&mut raw);
        assert!(raw.apply(&Frame::cancel(sample.device)));
        let expired = Instant::now() - AMBIGUOUS_TAIL - Duration::from_secs(1);
        raw.ended = Some(expired);
        let submitted = raw.sink.frames;
        for device in [sample.device, 99] {
            assert!(raw.apply(&Frame::cancel(device)));
            assert_eq!(raw.sink.frames, submitted);
            assert_eq!(raw.ended, Some(expired));
            assert!(!raw.owns_wheel(Instant::now()));
        }
    }

    fn assert_wheel_capture(raw: &Raw<Recording>, now: Instant, owns: bool) {
        use super::super::{hook::suppress_native_wheel, wheel_origin::Origin};
        use windows::Win32::UI::WindowsAndMessaging::{
            LLMHF_INJECTED, MSLLHOOKSTRUCT, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        };
        assert_eq!(raw.owns_wheel(now), owns);
        for msg in [WM_MOUSEWHEEL, WM_MOUSEHWHEEL, WM_MOUSEMOVE] {
            for origin in [Origin::Unknown, Origin::DeviceLess, Origin::PhysicalMouse] {
                for flags in [0, LLMHF_INJECTED, LLMHF_INJECTED | 2] {
                    // Own tagged records are consumed before this classifier.
                    // Foreign extra-info and zero remain ordinary source input.
                    for extra in [0, 0x1234] {
                        let info = MSLLHOOKSTRUCT {
                            flags,
                            dwExtraInfo: extra,
                            ..Default::default()
                        };
                        assert_eq!(
                            suppress_native_wheel(msg, &info, raw.owns_wheel(now), origin),
                            owns && msg != WM_MOUSEMOVE && flags == 0 && origin != Origin::PhysicalMouse,
                            "msg={msg} origin={origin:?} flags={flags} extra={extra}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn native_contact_end_and_handoff_cancel_bound_only_ambiguous_wheels() {
        use oink_foundation::contacts::Phase;
        for phase in [Some(Phase::End), Some(Phase::Cancel), None] {
            let mut raw = raw();
            assert_wheel_capture(&raw, Instant::now(), false);
            let sample = begin(&mut raw);
            assert_wheel_capture(&raw, Instant::now(), true);
            // A different device's Cancel cannot retire the active owner.
            assert!(raw.apply(&Frame::cancel(99)));
            assert!(raw.receiver.active());
            assert_eq!(raw.ended, None);
            match phase {
                Some(Phase::End) => {
                    let end = Sample {
                        time_us: 9_000,
                        len: 0,
                        ..sample
                    };
                    assert!(raw.apply(&Frame::between(Some(&sample), &end)));
                }
                Some(Phase::Cancel) => assert!(raw.apply(&Frame::cancel(sample.device))),
                // The same path used by focus leave / secure desktop / stop.
                None => raw.cancel(),
                _ => unreachable!(),
            }
            assert!(!raw.receiver.active());
            let ended = raw
                .ended
                .expect("native release must retain asynchronous promotion ownership");
            assert!(raw.sink.frames >= 2);
            assert_wheel_capture(&raw, ended + Duration::from_millis(1999), true);
            assert_wheel_capture(&raw, ended + AMBIGUOUS_TAIL, false);
            raw.cancel();
            assert_eq!(raw.ended, Some(ended), "idle lifecycle cleanup cannot extend the tail");
        }
    }
    #[test]
    fn failed_native_contact_release_keeps_tail_and_withdraws_device() {
        let mut raw = raw();
        let sample = begin(&mut raw);
        raw.sink.fail = true;
        assert!(!raw.apply(&Frame::cancel(sample.device)));
        assert!(!raw.available);
        assert!(!raw.receiver.active());
        let ended = raw
            .ended
            .expect("a failed native release may already have promoted wheels");
        assert_wheel_capture(&raw, ended, true);
        assert_wheel_capture(&raw, ended + AMBIGUOUS_TAIL, false);
    }

    #[test]
    fn contact_status_distinguishes_startup_errors_from_runtime_unavailability() {
        let pending = contact_status_from_probe(0, None);
        assert!(!pending.initialized && !pending.supported);
        assert_eq!(pending.init_error_kind, None);
        assert!(!contact_status_from_probe(0, Some(5)).supported);

        let ready = contact_status_from_probe(1, Some(5));
        assert!(ready.initialized && ready.supported);
        assert_eq!(ready.max_contacts, Some(5));
        assert_eq!(ready.init_error_kind, None);

        let withdrawn = contact_status_from_probe(1, None);
        assert!(withdrawn.initialized && !withdrawn.supported);
        assert_eq!(withdrawn.init_error_kind, None);

        for error in [
            ContactInitError::User32Unavailable,
            ContactInitError::CreateApiUnavailable,
            ContactInitError::InjectApiUnavailable,
            ContactInitError::ActionApiUnavailable,
            ContactInitError::DestroyApiUnavailable,
            ContactInitError::DeviceCreationRejected,
        ] {
            let rejected = contact_status_from_probe(error as u8, None);
            assert!(!rejected.initialized && !rejected.supported);
            assert_eq!(rejected.init_error_kind, Some(error));
        }
    }
    #[test]
    fn pointer_layout_matches_native_sdk() {
        assert_eq!(
            std::mem::offset_of!(Pointer, touch),
            std::mem::align_of::<POINTER_TOUCH_INFO>()
        );
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<Pointer>(), 152);
    }
    #[test]
    fn unavailable_device_retains_the_whole_event_and_never_owns_wheels() {
        let mut touchpad = ScrollContext::unavailable();
        assert_eq!(touchpad.scroll((12.0, -21.0), 192, ScrollPhase::Began), (1.0, 1.0));
        assert_eq!(touchpad.scroll((0.0, -9.0), 192, ScrollPhase::Momentum), (1.0, 1.0));
        assert!(!touchpad.pan.active());
        assert!(!touchpad.owns_ambiguous_wheel(Instant::now()));
    }
    #[test]
    fn only_recent_native_end_owns_ambiguous_wheel() {
        let mut touchpad = ScrollContext::unavailable();
        let end = Instant::now();
        touchpad.ended = Some(end);
        assert!(touchpad.owns_ambiguous_wheel(end + Duration::from_millis(1999)));
        assert!(!touchpad.owns_ambiguous_wheel(end + AMBIGUOUS_TAIL));
        touchpad.cancel();
        assert_eq!(touchpad.ended, Some(end));
    }
}
