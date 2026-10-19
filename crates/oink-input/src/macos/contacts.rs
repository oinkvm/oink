//! The accepted private MultitouchSupport ABI, isolated from routing. The foreign
//! callback validates/copies into a preallocated queue and signals the capture loop.
//! No locks, allocation, formatting, or router calls on the framework thread.
use crate::{CapturedEvent, InputEvent, SourceKind};
use crossbeam_queue::ArrayQueue;
use oink_foundation::contacts::{Contact, Sample, Surface};
use std::{
    ffi::{CStr, c_void},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: f32,
    y: f32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Vector {
    position: Point,
    velocity: Point,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeContact {
    frame: i32,
    timestamp: f64,
    identifier: i32,
    state: i32,
    unknown1: i32,
    unknown2: i32,
    normalized: Vector,
    size: f32,
    unknown3: i32,
    angle: f32,
    major: f32,
    minor: f32,
    absolute: Vector,
    unknown4: i32,
    unknown5: i32,
    density: f32,
}
const _: () = assert!(std::mem::size_of::<NativeContact>() == 96);
const _: () = assert!(std::mem::offset_of!(NativeContact, normalized) == 32);
type Device = *const c_void;
type Callback = unsafe extern "C" fn(Device, *const NativeContact, usize, f64, usize);
type Register = unsafe extern "C" fn(Device, Callback);
type Stop = unsafe extern "C" fn(Device) -> i32;

struct Shared {
    queue: ArrayQueue<Sample>,
    failed: AtomicU8,
    remote: &'static AtomicBool,
    touching: AtomicBool,
    observed: AtomicBool,
    surface: Surface,
    wake: crate::capture_loop::Wake,
}
impl Shared {
    fn fail(&self, reason: u8) {
        if reason != 5 {
            // A malformed report cannot establish a full lift, even if its
            // partially decoded prefix happened to contain no touching contacts.
            self.touching.store(true, Ordering::Release);
        }
        let _ = self
            .failed
            .compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire);
    }
}
// A bad report or queue overflow cancels that touch, not the subscription. macOS
// continues reporting a full lift afterwards. Preserve a queued final lift while
// discarding stale geometry, so recovery does not require another touch/restart.
fn take_fault(shared: &Shared, pending: &mut Option<Sample>, now_us: u64) -> Option<Sample> {
    let reason = shared.failed.swap(0, Ordering::AcqRel);
    if reason == 0 {
        return None;
    }
    *pending = None;
    while shared.queue.pop().is_some() {}
    if !shared.touching.load(Ordering::Acquire) {
        // The validated latest hardware state was a full lift, possibly the
        // report lost to overflow. Retain that boundary, never stale geometry.
        *pending = Some(Sample {
            device: 1,
            time_us: now_us,
            surface: shared.surface,
            ..Default::default()
        });
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
        device: 1,
        time_us: now_us,
        surface: shared.surface,
        len: 255,
        ..Default::default()
    })
}
static CURRENT: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn observe(_: Device, contacts: *const NativeContact, count: usize, timestamp: f64, _: usize) {
    // SeqCst entry precedes loading CURRENT. Drop clears CURRENT before waiting
    // for readers, so a late callback cannot dereference freed callback storage.
    CALLBACKS.fetch_add(1, Ordering::SeqCst);
    let pointer = CURRENT.load(Ordering::SeqCst);
    if !pointer.is_null() {
        // SAFETY: the reader count retains Shared through the complete callback.
        let shared = unsafe { &*pointer };
        let mut sample = Sample {
            device: 1,
            time_us: (timestamp * 1_000_000.) as u64,
            surface: shared.surface,
            ..Default::default()
        };
        let valid = count <= 16 && (count == 0 || !contacts.is_null()) && timestamp.is_finite() && timestamp >= 0.;
        if !valid {
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
            let mut valid_frame = true;
            for p in native {
                if !(0..=7).contains(&p.state) {
                    shared.fail(2);
                    valid_frame = false;
                    break;
                }
                // Native calibration on macOS 14 and 26: array length also includes hover/lift residue.
                if !matches!(p.state, 3 | 4) {
                    continue;
                }
                let Point { x, y } = p.normalized.position;
                if sample.len == 5
                    || p.identifier <= 0
                    || !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
                {
                    shared.fail(3);
                    valid_frame = false;
                    break;
                }
                let c = Contact {
                    id: p.identifier as u32,
                    x: (f64::from(x) * f64::from(shared.surface.width)).round() as i32,
                    y: ((1. - f64::from(y)) * f64::from(shared.surface.height)).round() as i32,
                };
                if sample.points().iter().any(|p| p.id == c.id) {
                    shared.fail(4);
                    valid_frame = false;
                    break;
                }
                sample.contacts[usize::from(sample.len)] = c;
                sample.len += 1;
            }
            if !valid_frame {
                (shared.wake)();
                CALLBACKS.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            let was_touching = shared.touching.swap(sample.len != 0, Ordering::AcqRel);
            let first = !shared.observed.swap(true, Ordering::Relaxed);
            // Local geometry is not forwarded or queued. The empty lift boundary
            // alone lets routing retire a cancelled remote gesture after returning home.
            if (shared.remote.load(Ordering::Acquire) || sample.len == 0 && (was_touching || first))
                && shared.queue.push(sample).is_err()
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
    pending: Option<Sample>,
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
            for i in 0..CFArrayGetCount(list.cast()) {
                let candidate = CFArrayGetValueAtIndex(list.cast(), i).cast();
                if built_in(candidate) {
                    ambiguous |= !device.is_null();
                    device = candidate;
                } else {
                    ambiguous = true;
                }
            }
            // CG reports a device class, not an MT device ID. Until multi-board
            // correlation is validated, keep the existing fallback if more than
            // the one P0-verified built-in touch surface is present.
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
                    touching: AtomicBool::new(false),
                    observed: AtomicBool::new(false),
                    surface,
                    wake,
                }),
                device,
                list,
                unregister,
                stop,
                pending: None,
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
        if let Some(fault) = take_fault(&self.shared, &mut self.pending, now_us) {
            return Some(event(fault));
        }
        let sample = self.pending.take().or_else(|| self.shared.queue.pop())?;
        if sample.time_us.saturating_add(8_000) > now_us {
            self.pending = Some(sample);
            return None;
        }
        Some(event(sample))
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|s| {
            Instant::now()
                + Duration::from_micros(
                    s.time_us
                        .saturating_add(8_000)
                        .saturating_sub(super::tap::now_event_ns() / 1_000),
                )
        })
    }
}
fn event(sample: Sample) -> CapturedEvent {
    CapturedEvent {
        physical: false,
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
    fn capture_fault_preserves_full_lift_and_reports_subsequent_faults() {
        let shared = Shared {
            queue: ArrayQueue::new(2),
            failed: AtomicU8::new(0),
            remote: &REMOTE,
            touching: AtomicBool::new(false),
            observed: AtomicBool::new(false),
            surface: Surface {
                width: 15000,
                height: 9000,
            },
            wake: std::sync::Arc::new(|| {}),
        };
        let mut pending = None;
        for time_us in [10000, 20000] {
            shared
                .queue
                .push(Sample {
                    device: 1,
                    time_us,
                    surface: shared.surface,
                    ..Default::default()
                })
                .unwrap();
            shared.fail(5);
            assert_eq!(take_fault(&shared, &mut pending, time_us + 1000).unwrap().len, 255);
            assert_eq!(pending.take().unwrap().time_us, time_us + 1000);
            assert!(take_fault(&shared, &mut pending, time_us + 2000).is_none());
        }
        shared.fail(3);
        assert_eq!(take_fault(&shared, &mut pending, 40000).unwrap().len, 255);
        assert!(pending.is_none(), "a malformed empty prefix is not a full lift");
    }
}
