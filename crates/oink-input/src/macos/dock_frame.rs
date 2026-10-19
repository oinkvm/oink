//! Bounded AX observation of the Dock list, off the event-tap thread.
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{CFString, CFType, CGPoint, CGSize};
use objc2_foundation::NSString;
use oink_foundation::Rect;
use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementSetMessagingTimeout(el: *const c_void, timeout: f32) -> i32;
    fn AXUIElementCopyAttributeValue(el: *const c_void, attr: *const c_void, value: *mut *const c_void) -> i32;
    fn AXUIElementGetTypeID() -> usize;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: *const c_void, ty: u32, out: *mut c_void) -> u8;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetCount(a: *const CFType) -> isize;
    fn CFArrayGetValueAtIndex(a: *const CFType, i: isize) -> *const CFType;
    fn CFRelease(value: *const c_void);
}
struct Owned(*const c_void);
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: Owned is only constructed from a non-null Create/Copy result.
        unsafe { CFRelease(self.0) }
    }
}
fn attr(el: *const c_void, name: &str) -> Option<Owned> {
    let key = CFString::from_str(name);
    let mut out = std::ptr::null();
    // SAFETY: el is a live AX element and key/out remain valid throughout this call.
    let code = unsafe { AXUIElementCopyAttributeValue(el, (&*key as *const CFString).cast(), &mut out) };
    let value = if out.is_null() { None } else { Some(Owned(out)) };
    if code == 0 { value } else { None }
}
pub(super) fn read() -> Option<Rect> {
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str("com.apple.dock"));
    let pid = apps.firstObject()?.processIdentifier();
    // SAFETY: standard AX/CF calls. Arrays, strings and AXValues are type-checked before access;
    // all borrowed children remain owned by the retained array and every Copy/Create is released.
    unsafe {
        let raw = AXUIElementCreateApplication(pid);
        if raw.is_null() {
            return None;
        }
        let app = Owned(raw);
        AXUIElementSetMessagingTimeout(app.0, 0.025);
        let start = Instant::now();
        let children = attr(app.0, "AXChildren")?;
        if CFGetTypeID(children.0) != CFArrayGetTypeID() {
            return None;
        }
        for i in 0..CFArrayGetCount(children.0.cast()).min(16) {
            if start.elapsed() > Duration::from_millis(100) {
                break;
            }
            let el: *const c_void = CFArrayGetValueAtIndex(children.0.cast(), i).cast();
            if el.is_null() || CFGetTypeID(el) != AXUIElementGetTypeID() {
                continue;
            }
            AXUIElementSetMessagingTimeout(el, 0.025);
            let Some(role) = attr(el, "AXRole") else { continue };
            if CFGetTypeID(role.0) != CFStringGetTypeID() || (*role.0.cast::<CFString>()).to_string() != "AXList" {
                continue;
            }
            let pos = attr(el, "AXPosition")?;
            let size = attr(el, "AXSize")?;
            if CFGetTypeID(pos.0) != AXValueGetTypeID() || CFGetTypeID(size.0) != AXValueGetTypeID() {
                return None;
            }
            let mut p = CGPoint::default();
            let mut s = CGSize::default();
            if AXValueGetValue(pos.0, 1, (&mut p as *mut CGPoint).cast()) == 0
                || AXValueGetValue(size.0, 2, (&mut s as *mut CGSize).cast()) == 0
            {
                return None;
            }
            if [p.x, p.y, s.width, s.height].iter().all(|v| v.is_finite()) && s.width > 0.0 && s.height > 0.0 {
                return Some(Rect::new(p.x, p.y, s.width, s.height));
            }
        }
    }
    None
}
