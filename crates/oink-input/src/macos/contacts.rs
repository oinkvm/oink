//! The accepted private MultitouchSupport ABI, isolated from routing. The foreign
//! callback validates/copies into a preallocated queue and signals the capture loop.
//! No locks, allocation, formatting, or router calls on the framework thread.
use crate::{
    CapturedEvent, InputEvent, SourceKind,
    contact_capture::{Delivery, Gate, TimedSample},
    mt_contacts::{NativeContact, decode},
};
use crossbeam_queue::ArrayQueue;
use oink_foundation::contacts::{Sample, Surface};
use std::{
    ffi::{CStr, c_void},
    hash::BuildHasher,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

type Device = *const c_void;
type Callback = unsafe extern "C" fn(Device, *const NativeContact, usize, f64, usize);
type Register = unsafe extern "C" fn(Device, Callback);
type Stop = unsafe extern "C" fn(Device) -> i32;

struct Shared {
    queue: ArrayQueue<TimedSample>,
    failed: AtomicU8,
    remote: &'static AtomicBool,
    gate: Gate,
    source_us: std::sync::atomic::AtomicU64,
    source_token: std::sync::atomic::AtomicU64,
    device_id: u32,
    device_count: u16,
    built_in_count: u16,
    device_hash: std::collections::hash_map::RandomState,
    trace: ArrayQueue<crate::mt_trace::Tracker>,
    surface: Surface,
    wake: crate::capture_loop::Wake,
}
impl Shared {
    fn trace(&self, observation: crate::mt_trace::Observation) {
        if !crate::native_lifecycle::lifecycle_enabled() {
            return;
        }
        // Framework callback never blocks or allocates; concurrent diagnostic
        // consumers may skip an observation, never alter capture admission.
        if let Some(mut tracker) = self.trace.pop() {
            if let Some(observation) = tracker.observe(observation) {
                crate::native_lifecycle::emit_mt_contacts(observation);
            }
            let _ = self.trace.push(tracker);
        }
    }
    fn fail(&self, reason: u8) {
        if reason != 5 {
            // A malformed report cannot establish a full lift, even if its
            // partially decoded prefix happened to contain no touching contacts.
            self.gate.observe(true);
        }
        let _ = self
            .failed
            .compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire);
    }
}
// A bad report or queue overflow cancels that touch, not the subscription. macOS
// continues reporting a full lift afterwards. Preserve a queued final lift while
// discarding stale geometry, so recovery does not require another touch/restart.
fn take_fault(shared: &Shared, delivery: &mut Delivery, now_us: u64) -> Option<Sample> {
    let reason = shared.failed.swap(0, Ordering::AcqRel);
    if reason == 0 {
        return None;
    }
    delivery.pending = None;
    let source_us = shared.source_us.load(Ordering::Acquire);
    while shared.queue.pop().is_some() {}
    let status = shared.gate.status();
    if status.validated_idle() && shared.source_token.load(Ordering::Acquire) == status.token() {
        // The validated latest hardware state was a full lift, possibly the
        // report lost to overflow. Retain that boundary, never stale geometry.
        delivery.pending = Some(TimedSample::new(
            Sample {
                device: shared.device_id,
                time_us: source_us,
                surface: shared.surface,
                ..Default::default()
            },
            now_us,
        ));
        delivery.pending.as_mut().unwrap().token = shared.gate.status().token();
    }
    crate::native_lifecycle::emit_lifecycle(
        crate::native_lifecycle::Kind::ContactCapture,
        crate::native_lifecycle::Data {
            message: 1,
            reason: u32::from(reason),
            success: Some(false),
            ..Default::default()
        },
    );
    Some(Sample {
        device: shared.device_id,
        time_us: source_us,
        surface: shared.surface,
        len: 255,
        ..Default::default()
    })
}
static CURRENT: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());
static NEXT_DEVICE: AtomicU32 = AtomicU32::new(1);
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

/// Called after publishing capture mode. Retire queued/pending samples from the
/// previous focus, and atomically remember whether entry was already known idle.
pub(super) fn boundary() {
    CALLBACKS.fetch_add(1, Ordering::SeqCst);
    let pointer = CURRENT.load(Ordering::SeqCst);
    if !pointer.is_null() {
        // SAFETY: the reader count also protects this non-framework reader.
        let shared = unsafe { &*pointer };
        shared.gate.boundary();
        (shared.wake)();
    }
    CALLBACKS.fetch_sub(1, Ordering::SeqCst);
}

unsafe extern "C" fn observe(
    callback_device: Device,
    contacts: *const NativeContact,
    count: usize,
    timestamp: f64,
    frame_count: usize,
) {
    // SeqCst entry precedes loading CURRENT. Drop clears CURRENT before waiting
    // for readers, so a late callback cannot dereference freed callback storage.
    CALLBACKS.fetch_add(1, Ordering::SeqCst);
    let pointer = CURRENT.load(Ordering::SeqCst);
    if !pointer.is_null() {
        // SAFETY: the reader count retains Shared through the complete callback.
        let shared = unsafe { &*pointer };
        let token = shared.gate.begin().token();
        let received_us = super::tap::now_event_ns() / 1_000;
        let mut sample = Sample {
            device: shared.device_id,
            time_us: (timestamp * 1_000_000.) as u64,
            surface: shared.surface,
            ..Default::default()
        };
        let valid = count <= 16 && (count == 0 || !contacts.is_null()) && timestamp.is_finite() && timestamp >= 0.;
        let mut trace = crate::mt_trace::Observation {
            kind: 1,
            device: shared.device_id,
            callback_device: shared.device_hash.hash_one(callback_device as usize) as u32,
            devices: shared.device_count,
            built_in: shared.built_in_count,
            frame_count: frame_count as u64,
            source_us: sample.time_us,
            received_us,
            raw_count: count.min(255) as u8,
            ..Default::default()
        };
        if !valid {
            shared.trace(trace);
            shared.gate.finish(true);
            shared.fail(1);
        } else {
            // SAFETY: count was bounded; MultitouchSupport lends this array only
            // for the callback. A zero-length callback may have a null pointer.
            let native = if count == 0 {
                &[][..]
            } else {
                // SAFETY: validated bounded non-null array lent for this callback.
                unsafe { std::slice::from_raw_parts(contacts, count) }
            };
            for (out, p) in trace.contacts.iter_mut().zip(native) {
                *out = crate::mt_trace::Contact {
                    id: p.path_index,
                    finger_identity: p.finger_identity,
                    state: p.state,
                    frame: p.frame,
                    size_bits: p.size.to_bits(),
                    density_bits: p.density.to_bits(),
                };
            }
            let result = decode(native, &mut sample);
            let valid_frame = result.is_ok();
            trace.valid = valid_frame;
            trace.accepted = sample.len;
            shared.trace(trace);
            if let Err(reason) = result {
                shared.fail(reason);
            }
            shared.source_us.store(sample.time_us, Ordering::Release);
            shared.source_token.store(token, Ordering::Release);
            let (old, _) = shared.gate.finish(!valid_frame || sample.len != 0);
            if !valid_frame {
                (shared.wake)();
                CALLBACKS.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let was_touching = old.touching();
            let first = !old.observed();
            let mut queued = TimedSample::new(sample, received_us);
            queued.token = token;
            // Local geometry is not forwarded or queued. The empty lift boundary
            // alone lets routing retire a cancelled remote gesture after returning home.
            if (shared.remote.load(Ordering::Acquire) || sample.len == 0 && (was_touching || first))
                && shared.queue.push(queued).is_err()
            {
                shared.fail(5);
            }
        }
        if !shared.queue.is_empty() || shared.failed.load(Ordering::Acquire) != 0 {
            (shared.wake)();
        }
    }
    CALLBACKS.fetch_sub(1, Ordering::SeqCst);
}

unsafe extern "C" {
    fn CFArrayGetCount(array: *const objc2_core_foundation::CFType) -> isize;
    fn CFArrayGetValueAtIndex(
        array: *const objc2_core_foundation::CFType,
        index: isize,
    ) -> *const objc2_core_foundation::CFType;
    fn CFRelease(object: *const c_void);
}

pub(super) struct Capture {
    shared: Box<Shared>,
    device: Device,
    list: *const c_void,
    unregister: Register,
    stop: Stop,
    delivery: Delivery,
}
impl Capture {
    pub fn start(remote: &'static AtomicBool, wake: crate::capture_loop::Wake) -> Option<Self> {
        if !CURRENT.load(Ordering::SeqCst).is_null() {
            return None;
        }
        // SAFETY: all runtime symbols have the P0-checked C signatures. The library
        // remains loaded for process lifetime; it may retain its own worker threads.
        unsafe {
            let library = libc::dlopen(
                c"/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if library.is_null() {
                return None;
            }
            let symbol = |name: &CStr| {
                let p = libc::dlsym(library, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let list_fn: unsafe extern "C" fn() -> *const c_void = std::mem::transmute(symbol(c"MTDeviceCreateList")?);
            let built_in: unsafe extern "C" fn(Device) -> bool = std::mem::transmute(symbol(c"MTDeviceIsBuiltIn")?);
            let dimensions: unsafe extern "C" fn(Device, *mut i32, *mut i32) -> i32 =
                std::mem::transmute(symbol(c"MTDeviceGetSensorSurfaceDimensions")?);
            let register: Register = std::mem::transmute(symbol(c"MTRegisterContactFrameCallback")?);
            let unregister: Register = std::mem::transmute(symbol(c"MTUnregisterContactFrameCallback")?);
            let start: unsafe extern "C" fn(Device, i32) -> i32 = std::mem::transmute(symbol(c"MTDeviceStart")?);
            let stop: Stop = std::mem::transmute(symbol(c"MTDeviceStop")?);
            let list = list_fn();
            if list.is_null() {
                return None;
            }
            let mut device: Device = std::ptr::null();
            let mut ambiguous = false;
            let device_count = CFArrayGetCount(list.cast()).clamp(0, u16::MAX as isize) as u16;
            let mut built_in_count = 0u16;
            let device_id = NEXT_DEVICE.fetch_add(1, Ordering::Relaxed).max(1);
            for i in 0..device_count as isize {
                let candidate = CFArrayGetValueAtIndex(list.cast(), i).cast();
                if built_in(candidate) {
                    built_in_count += 1;
                    ambiguous |= !device.is_null();
                    device = candidate;
                } else {
                    ambiguous = true;
                }
            }
            // CG reports a device class, not an MT device ID. Until multi-board
            // correlation is validated, keep the existing fallback if more than
            // the one P0-verified built-in touch surface is present.
            crate::native_lifecycle::emit_mt_contacts(crate::mt_trace::Observation {
                kind: 0,
                device: device_id,
                devices: device_count,
                built_in: built_in_count,
                valid: !device.is_null() && !ambiguous,
                ..Default::default()
            });
            if device.is_null() || ambiguous {
                CFRelease(list);
                return None;
            }
            let (mut width, mut height) = (0, 0);
            dimensions(device, &mut width, &mut height);
            // Native sensor units are 1e-5 metres. The fixed reference quantum
            // is exactly that length, so the integer boundary conversion is exact.
            let surface = Surface {
                width: width as u32,
                height: height as u32,
            };
            if !surface.valid() {
                CFRelease(list);
                return None;
            }
            let mut capture = Self {
                shared: Box::new(Shared {
                    queue: ArrayQueue::new(256),
                    failed: AtomicU8::new(0),
                    remote,
                    gate: Gate::default(),
                    source_us: std::sync::atomic::AtomicU64::new(0),
                    source_token: std::sync::atomic::AtomicU64::new(0),
                    device_id,
                    device_count,
                    built_in_count,
                    device_hash: std::collections::hash_map::RandomState::new(),
                    trace: {
                        let q = ArrayQueue::new(1);
                        let _ = q.push(crate::mt_trace::Tracker::default());
                        q
                    },
                    surface,
                    wake,
                }),
                device,
                list,
                unregister,
                stop,
                delivery: Delivery::default(),
            };
            // The product owns one capture thread; startup/stop are serialized.
            CURRENT.store(&mut *capture.shared, Ordering::SeqCst);
            register(device, observe);
            if start(device, 0) != 0 {
                return None;
            }
            crate::native_lifecycle::emit_lifecycle(
                crate::native_lifecycle::Kind::ContactCapture,
                crate::native_lifecycle::Data {
                    success: Some(true),
                    ..Default::default()
                },
            );
            Some(capture)
        }
    }
    /// Eight milliseconds gives the normal CG pan channel a chance to arrive
    /// before a contact-only first movement commits raw. It is not a pan timeout.
    pub fn pop(&mut self, now_us: u64) -> Option<CapturedEvent> {
        if self.delivery.take_boundary(self.shared.gate.status()) {
            crate::native_lifecycle::emit_lifecycle(
                crate::native_lifecycle::Kind::ContactCapture,
                crate::native_lifecycle::Data {
                    message: 3,
                    extra: self.shared.gate.status().token(),
                    ..Default::default()
                },
            );
            return Some(CapturedEvent {
                physical: false,
                source: SourceKind::Touchpad,
                time_us: now_us,
                cursor: None,
                event: InputEvent::TouchpadBoundary,
            });
        }
        if let Some(fault) = take_fault(&self.shared, &mut self.delivery, now_us) {
            return Some(event(fault));
        }
        let sample = self
            .delivery
            .pop(&self.shared.queue, self.shared.gate.status(), now_us)?;
        if self.delivery.attested_lift {
            // Distinguish an entry's known-idle attestation from a new hardware
            // zero in the recorder; raw MT metadata always remains unmodified.
            crate::native_lifecycle::emit_lifecycle(
                crate::native_lifecycle::Kind::ContactCapture,
                crate::native_lifecycle::Data {
                    message: 2,
                    extra: self.shared.gate.status().token(),
                    success: Some(true),
                    ..Default::default()
                },
            );
        }
        Some(event(sample))
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.delivery
            .pending
            .map(|s| Instant::now() + Duration::from_micros(s.remaining_us(super::tap::now_event_ns() / 1_000)))
    }
}
fn event(sample: Sample) -> CapturedEvent {
    CapturedEvent {
        // Nonempty validated snapshots come from the hardware contact callback.
        // Empty lift attestations and corruption/overflow sentinels can also be
        // synthesized by Delivery; those must never renew group activity.
        physical: sample.valid() && sample.len != 0,
        source: SourceKind::Touchpad,
        time_us: sample.time_us,
        cursor: None,
        event: InputEvent::TouchpadSample(sample),
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        CURRENT.store(std::ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: same retained device and registered callback, capture owner only.
        unsafe {
            (self.stop)(self.device);
            (self.unregister)(self.device, observe);
        }
        while CALLBACKS.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
        // SAFETY: callbacks can no longer refer to the device list or Shared.
        unsafe { CFRelease(self.list) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static REMOTE: AtomicBool = AtomicBool::new(true);
    #[test]
    fn hardware_contact_activity_excludes_delivery_lifts_and_faults() {
        let mut sample = Sample {
            device: 1,
            surface: Surface {
                width: 15000,
                height: 9000,
            },
            len: 1,
            ..Default::default()
        };
        sample.contacts[0] = oink_foundation::contacts::Contact {
            id: 1,
            x: 5000,
            y: 4000,
        };
        assert!(event(sample).physical);
        sample.len = 0;
        assert!(!event(sample).physical);
        sample.len = 255;
        assert!(!event(sample).physical);
        sample.len = 1;
        sample.contacts[0].id = 0;
        assert!(!event(sample).physical);
    }

    #[test]
    fn capture_fault_preserves_full_lift_and_reports_subsequent_faults() {
        let shared = Shared {
            queue: ArrayQueue::new(2),
            failed: AtomicU8::new(0),
            remote: &REMOTE,
            gate: Gate::default(),
            source_us: std::sync::atomic::AtomicU64::new(0),
            source_token: std::sync::atomic::AtomicU64::new(0),
            device_id: 1,
            device_count: 1,
            built_in_count: 1,
            device_hash: std::collections::hash_map::RandomState::new(),
            trace: ArrayQueue::new(1),
            surface: Surface {
                width: 15000,
                height: 9000,
            },
            wake: std::sync::Arc::new(|| {}),
        };
        shared.gate.observe(false);
        let mut pending = Delivery::default();
        for time_us in [10000, 20000] {
            shared
                .queue
                .push(TimedSample::new(
                    Sample {
                        device: shared.device_id,
                        time_us,
                        surface: shared.surface,
                        ..Default::default()
                    },
                    time_us,
                ))
                .unwrap();
            shared.source_us.store(time_us, Ordering::Release);
            shared.fail(5);
            assert_eq!(take_fault(&shared, &mut pending, time_us + 1000).unwrap().len, 255);
            assert_eq!(pending.pending.take().unwrap().sample.time_us, time_us);
            assert!(take_fault(&shared, &mut pending, time_us + 2000).is_none());
        }
        // An old callback that completes across entry cannot smuggle its zero
        // through the overflow recovery path as a current-epoch lift.
        shared.gate.observe(true);
        let old = shared.gate.begin();
        shared.gate.boundary();
        shared.gate.finish(false);
        shared.source_token.store(old.token(), Ordering::Release);
        shared.fail(5);
        assert!(take_fault(&shared, &mut pending, 39000).is_some());
        assert!(pending.pending.is_none());
        shared.fail(3);
        assert_eq!(take_fault(&shared, &mut pending, 40000).unwrap().len, 255);
        assert!(pending.pending.is_none(), "a malformed empty prefix is not a full lift");
    }
}
