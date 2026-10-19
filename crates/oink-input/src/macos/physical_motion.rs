//! Read the accelerated pointer child attached to this CGEvent. No asynchronous
//! device queue, cursor query, remembered placement or acceleration curve.
//!
//! macOS 26.6.2 hardware probe: ordinary frames carry one flag-0x10000
//! pointer child, same timestamp/sender; left/right/other drags have no tree.
//! CG double, NSEvent and retyping a copy do not recover drag data. Consequently
//! only the eligible ordinary path calls this reader; unsupported data retains
//! the old correction with a numeric reason. Source: Apple IOHIDFamily
//! IOHIDPointerScrollFilter.cpp / IOHIDEventData.h. No changes to event contents or routing
//! of drags, posts, raw companions, scroll or gestures.
use std::ffi::c_void;
use std::sync::OnceLock;

use objc2_core_foundation::CFType;
use objc2_core_graphics::CGEvent;

use super::system::find_symbol;
use crate::ordinary_motion::{HidNode, POINTER_TYPE, Reason, select};

type CopyEvent = unsafe extern "C" fn(*const CGEvent) -> *const c_void;
type GetU32 = unsafe extern "C" fn(*const c_void) -> u32;
type GetU64 = unsafe extern "C" fn(*const c_void) -> u64;
type GetChildren = unsafe extern "C" fn(*const c_void) -> *const c_void;
type GetInteger = unsafe extern "C" fn(*const c_void, u32) -> isize;
type GetFloat = unsafe extern "C" fn(*const c_void, u32) -> f64;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
    fn CFArrayGetCount(array: *const CFType) -> isize;
    fn CFArrayGetValueAtIndex(array: *const CFType, index: isize) -> *const CFType;
}

struct Attached(*const c_void);
impl Drop for Attached {
    fn drop(&mut self) {
        // SAFETY: non-null result of CGEventCopyIOHIDEvent, owned until this drop.
        unsafe { CFRelease(self.0) };
    }
}

struct Functions {
    copy: CopyEvent,
    kind: GetU32,
    flags: GetU32,
    timestamp: GetU64,
    sender: GetU64,
    children: GetChildren,
    float: GetFloat,
    integer: GetInteger,
}

fn functions() -> Option<&'static Functions> {
    static FUNCTIONS: OnceLock<Option<Functions>> = OnceLock::new();
    FUNCTIONS
        .get_or_init(|| {
            macro_rules! symbol {
                ($name:literal, $ty:ty) => {{
                    let ptr = find_symbol($name);
                    if ptr.is_null() {
                        return None;
                    }
                    // SAFETY: IOHIDEvent/CGEvent ABI, resolved only when exported.
                    unsafe { std::mem::transmute::<*mut c_void, $ty>(ptr) }
                }};
            }
            Some(Functions {
                copy: symbol!(c"CGEventCopyIOHIDEvent", CopyEvent),
                kind: symbol!(c"IOHIDEventGetType", GetU32),
                flags: symbol!(c"IOHIDEventGetEventFlags", GetU32),
                timestamp: symbol!(c"IOHIDEventGetTimeStamp", GetU64),
                sender: symbol!(c"IOHIDEventGetSenderID", GetU64),
                children: symbol!(c"IOHIDEventGetChildren", GetChildren),
                float: symbol!(c"IOHIDEventGetFloatValue", GetFloat),
                integer: symbol!(c"IOHIDEventGetIntegerValue", GetInteger),
            })
        })
        .as_ref()
}

impl Functions {
    unsafe fn node(&self, event: *const c_void) -> HidNode {
        // SAFETY: caller retains the attached tree throughout traversal.
        unsafe {
            let kind = (self.kind)(event);
            HidNode {
                kind,
                flags: (self.flags)(event),
                timestamp: (self.timestamp)(event),
                sender: (self.sender)(event),
                // IOHIDEventTypes: PointerX/Y/Z then PointerButtonMask (base+3).
                // This is device-frame evidence, unlike mixed WindowServer state.
                buttons: (kind == POINTER_TYPE).then(|| (self.integer)(event, (POINTER_TYPE << 16) + 3) as u32),
                delta: if kind == POINTER_TYPE {
                    (
                        (self.float)(event, POINTER_TYPE << 16),
                        (self.float)(event, (POINTER_TYPE << 16) + 1),
                    )
                } else {
                    (0., 0.)
                },
            }
        }
    }
}

pub(super) fn initialize() {
    let _ = functions(); // resolve symbols once, before dispatching tap callbacks
}

pub(super) fn read(event: &CGEvent) -> Result<(f64, f64), Reason> {
    let Some(f) = functions() else {
        return Err(Reason::ApiUnavailable);
    };
    // SAFETY: the input CGEvent is alive; Copy's result is released by Attached.
    let ptr = unsafe { (f.copy)(event) };
    if ptr.is_null() {
        return Err(Reason::NoAttachedEvent);
    }
    let attached = Attached(ptr);
    // Bounds protect the event tap. A partial tree is never an acceptable match.
    const MAX_NODES: usize = 32;
    const MAX_DEPTH: u8 = 8;
    let mut nodes = [HidNode::default(); MAX_NODES];
    let mut stack = [(std::ptr::null(), 0u8); MAX_NODES];
    stack[0] = (attached.0, 0);
    let (mut pending, mut count) = (1, 0);
    while pending != 0 {
        pending -= 1;
        let (node, depth) = stack[pending];
        if node.is_null() || depth > MAX_DEPTH || count == MAX_NODES {
            return Err(Reason::TreeLimit);
        }
        // SAFETY: root and all borrowed children remain owned by `attached`.
        unsafe {
            nodes[count] = f.node(node);
            count += 1;
            let children = (f.children)(node);
            if children.is_null() {
                continue;
            }
            let len = CFArrayGetCount(children.cast());
            if len < 0 || len as usize > MAX_NODES - pending {
                return Err(Reason::TreeLimit);
            }
            for index in 0..len {
                stack[pending] = (CFArrayGetValueAtIndex(children.cast(), index).cast(), depth + 1);
                pending += 1;
            }
        }
    }
    select(nodes[0], nodes[1..count].iter().copied())
}

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
static COUNTS: [AtomicU64; Reason::ALL.len()] = [const { AtomicU64::new(0) }; Reason::ALL.len()];

pub(super) fn count(reason: Reason) {
    COUNTS[reason as usize].fetch_add(1, Relaxed);
}
/// Numeric aggregates only, outside the event tap. Every fallback remains
/// visible even under the quiet capture-owner subscriber; no per-frame logging.
pub(super) fn report() {
    for reason in Reason::ALL {
        let count = COUNTS[reason as usize].swap(0, Relaxed);
        if count != 0 {
            crate::native_lifecycle::emit_lifecycle(
                crate::native_lifecycle::Kind::PointerAuthority,
                crate::native_lifecycle::Data {
                    reason: reason as u32,
                    reentries: count,
                    ..Default::default()
                },
            );
            tracing::debug!(reason = reason as u32, count, "ordinary pointer authority interval");
        }
    }
}
