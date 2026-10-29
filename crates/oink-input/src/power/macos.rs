use super::{Assertion, State, mac_assertion};
use objc2_core_foundation::CFType;
use std::{
    ffi::{c_char, c_void},
    ptr,
};
type Cf = *const c_void;
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Cf);
    fn CFGetTypeID(value: Cf) -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Cf) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFDictionaryGetCount(value: Cf) -> isize;
    fn CFDictionaryGetKeysAndValues(value: Cf, keys: *mut Cf, values: *mut Cf);
    fn CFDictionaryGetValue(value: Cf, key: Cf) -> Cf;
    fn CFArrayGetCount(value: *const CFType) -> isize;
    fn CFArrayGetValueAtIndex(value: *const CFType, index: isize) -> *const CFType;
    fn CFNumberGetValue(value: Cf, kind: isize, output: *mut c_void) -> bool;
    fn CFStringCreateWithCString(allocator: Cf, text: *const c_char, encoding: u32) -> Cf;
    fn CFStringGetCString(value: Cf, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMCopyAssertionsByProcess(output: *mut Cf) -> i32;
    fn IOPMAssertionCreateWithName(kind: Cf, level: u32, name: Cf, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}
struct Owned(Cf);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this wrapper owns the Create/Copy reference exactly once.
            unsafe { CFRelease(self.0) };
        }
    }
}
fn string(text: &str) -> Owned {
    let c = std::ffi::CString::new(text).expect("constant assertion string");
    // SAFETY: NUL-terminated UTF-8 is live for the synchronous copy.
    Owned(unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), 0x08000100) })
}
fn get(dict: Cf, key: &str) -> Cf {
    let key = string(key);
    // SAFETY: callers type-check the borrowed dictionary; key is an owned string.
    unsafe { CFDictionaryGetValue(dict, key.0) }
}
fn number(value: Cf) -> Option<u32> {
    // SAFETY: non-null values are borrowed from the live snapshot dictionary.
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut n = 0i64;
    // SAFETY: checked CFNumber, kCFNumberSInt64Type, and an i64 output slot.
    if !unsafe { CFNumberGetValue(value, 4, (&mut n as *mut i64).cast()) } {
        return None;
    }
    n.try_into().ok()
}
fn suspended(value: Cf) -> bool {
    // SAFETY: checked boolean borrowed from the still-owned assertion snapshot.
    !value.is_null() && unsafe { CFGetTypeID(value) == CFBooleanGetTypeID() && CFBooleanGetValue(value) != 0 }
}
fn text(value: Cf) -> String {
    // SAFETY: non-null values are borrowed from the live snapshot dictionary.
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return String::new();
    }
    let mut out = [0u8; 4096];
    // SAFETY: checked CFString and a writable buffer with its exact byte size.
    if !unsafe { CFStringGetCString(value, out.as_mut_ptr().cast(), out.len() as isize, 0x08000100) } {
        return String::new();
    }
    String::from_utf8_lossy(&out[..out.iter().position(|c| *c == 0).unwrap_or(out.len())]).into_owned()
}
fn image(pid: u32) -> Option<String> {
    let mut path = [0u8; 4096];
    // SAFETY: read-only process lookup into the sized output buffer.
    let n = unsafe { libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32) };
    (n > 0)
        .then(|| String::from_utf8_lossy(&path[..path.iter().position(|b| *b == 0).unwrap_or(path.len())]).into_owned())
}
pub fn observe() -> Result<State, String> {
    let mut raw = ptr::null();
    // SAFETY: writable output reference; Owned releases the returned Copy result.
    let result = unsafe { IOPMCopyAssertionsByProcess(&mut raw) };
    let owned = Owned(raw);
    if result != 0 || owned.0.is_null() {
        return Err(format!("assertion enumeration {result}"));
    }
    // SAFETY: successful non-null Copy result remains owned for this whole walk.
    if unsafe { CFGetTypeID(raw) != CFDictionaryGetTypeID() } {
        return Err("invalid assertion dictionary".into());
    }
    // SAFETY: the owned result has been checked as a dictionary.
    let n = unsafe { CFDictionaryGetCount(raw) };
    if !(0..=16384).contains(&n) {
        return Err("assertion process limit".into());
    }
    let mut keys = vec![ptr::null(); n as usize];
    let mut values = keys.clone();
    // SAFETY: both arrays are sized for the immutable dictionary count.
    unsafe { CFDictionaryGetKeysAndValues(raw, keys.as_mut_ptr(), values.as_mut_ptr()) };
    let mut state = State::default();
    for (key, array) in keys.into_iter().zip(values) {
        let Some(pid) = number(key) else {
            return Err("invalid assertion PID".into());
        };
        // SAFETY: borrowed value from the still-owned snapshot.
        if array.is_null() || unsafe { CFGetTypeID(array) != CFArrayGetTypeID() } {
            return Err("invalid assertion array".into());
        }
        // SAFETY: checked array borrowed from the owned dictionary.
        let count = unsafe { CFArrayGetCount(array.cast()) };
        if !(0..=16384).contains(&count) {
            return Err("assertion count limit".into());
        }
        for i in 0..count {
            // SAFETY: index is within the checked immutable array count.
            let row = unsafe { CFArrayGetValueAtIndex(array.cast(), i).cast::<c_void>() };
            // SAFETY: non-null array element remains borrowed from the snapshot.
            if row.is_null() || unsafe { CFGetTypeID(row) != CFDictionaryGetTypeID() } {
                return Err("invalid assertion row".into());
            }
            state = state.union(mac_assertion(
                Assertion {
                    kind: &text(get(row, "AssertType")),
                    enabled: number(get(row, "AssertLevel")) == Some(255)
                        && !suspended(get(row, "AssertionIsStateSuspended")),
                    pid,
                    on_behalf: number(get(row, "AssertionOnBehalfOfPID")),
                },
                std::process::id(),
                image,
            ));
        }
    }
    Ok(state)
}
#[derive(Debug, Default)]
pub struct Hold {
    display: Option<u32>,
    system: Option<u32>,
    activity: Option<u32>,
}
impl Hold {
    pub fn apply(&mut self, state: State) -> Result<(), String> {
        // Release first, even if creating the other requested assertion fails.
        let mut failure = None;
        for (slot, on) in [
            (&mut self.display, state.display),
            (&mut self.system, state.system),
            (&mut self.activity, state.activity),
        ] {
            if !on && let Some(id) = *slot {
                // SAFETY: this slot owns the assertion ID until release succeeds.
                let code = unsafe { IOPMAssertionRelease(id) };
                if code == 0 {
                    *slot = None;
                } else {
                    failure = Some(format!("release power assertion {code}"));
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        for (slot, on, kind) in [
            (&mut self.display, state.display, "PreventUserIdleDisplaySleep"),
            (&mut self.system, state.system, "PreventUserIdleSystemSleep"),
            (&mut self.activity, state.activity, "UserIsActive"),
        ] {
            if on && slot.is_none() {
                let kind = string(kind);
                let name = string("Oink mirroring group devices");
                let mut id = 0;
                // SAFETY: owned CFStrings and a writable assertion ID slot.
                let code = unsafe { IOPMAssertionCreateWithName(kind.0, 255, name.0, &mut id) };
                if code != 0 {
                    return Err(format!("create power assertion {code}"));
                }
                *slot = Some(id);
            }
        }
        Ok(())
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        let _ = self.apply(State::default());
    }
}
