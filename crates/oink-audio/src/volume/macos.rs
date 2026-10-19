use super::{Endpoint, ExitVolumeStatus, Fault, KeyAction, Reading};
use std::ffi::{CString, c_char, c_void};
unsafe extern "C" {
    fn oink_exit_volume_open(
        uid: *const c_char,
        result: *mut i32,
        native: *mut i32,
        wake: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> *mut c_void;
    fn oink_exit_volume_key(observer: *mut c_void, action: i32, modifiers: u8, native: *mut i32) -> i32;
    fn oink_exit_volume_close(observer: *mut c_void);
    fn oink_exit_volume_changed(observer: *mut c_void) -> bool;
    fn oink_exit_volume_read(observer: *mut c_void, volume: *mut f32, muted: *mut bool, native: *mut i32) -> i32;
    fn oink_exit_volume_set(observer: *mut c_void, volume: f32, muted: bool, native: *mut i32) -> i32;
}
fn fault(status: i32, native: i32) -> Fault {
    Fault {
        status: match status {
            1 => ExitVolumeStatus::Unsupported,
            2 => ExitVolumeStatus::Unavailable,
            3 => ExitVolumeStatus::PermissionDenied,
            _ => ExitVolumeStatus::BackendFailed,
        },
        native_code: (native != 0).then_some(i64::from(native)),
    }
}
struct Wake(std::sync::Arc<dyn Fn() + Send + Sync>);
extern "C" fn changed(context: *mut c_void) {
    // SAFETY: Native retains this box until native close drains callbacks.
    let wake = unsafe { &*context.cast::<Wake>() };
    (wake.0)();
}
struct Native {
    handle: *mut c_void,
    _wake: Box<Wake>,
}
pub(super) fn open(output: &str, wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> Result<Box<dyn Endpoint>, Fault> {
    let uid = output
        .strip_prefix("coreaudio:")
        .filter(|uid| !uid.is_empty())
        .ok_or_else(Fault::unavailable)?;
    let uid = CString::new(uid).map_err(|_| Fault::unavailable())?;
    let (mut status, mut native) = (0, 0);
    let mut wake = Box::new(Wake(wake));
    let context = (&mut *wake as *mut Wake).cast();
    // SAFETY: terminated UID and writable status outputs; opening only observes.
    let handle = unsafe { oink_exit_volume_open(uid.as_ptr(), &mut status, &mut native, changed, context) };
    if handle.is_null() {
        Err(fault(status, native))
    } else {
        Ok(Box::new(Native { handle, _wake: wake }))
    }
}
impl Endpoint for Native {
    fn changed(&self) -> bool {
        // SAFETY: this owner retains the observer; reads an atomic callback flag.
        unsafe { oink_exit_volume_changed(self.handle) }
    }
    fn read(&mut self) -> Result<Reading, Fault> {
        let (mut volume, mut muted, mut native) = (0.0, false, 0);
        // SAFETY: live observer, writable outputs; native checks its exact UID.
        let status = unsafe { oink_exit_volume_read(self.handle, &mut volume, &mut muted, &mut native) };
        if status != 0 {
            return Err(fault(status, native));
        }
        Ok(Reading {
            percent: (volume * 100.0).round() as u8,
            muted,
        })
    }
    fn key(&mut self, action: KeyAction) -> Result<(), Fault> {
        self.key_with_modifiers(action, 0)
    }
    fn key_with_modifiers(&mut self, action: KeyAction, modifiers: u8) -> Result<(), Fault> {
        let mut native = 0;
        let action = match action {
            KeyAction::Up => 1,
            KeyAction::Down => -1,
            KeyAction::Mute => 0,
        };
        // SAFETY: retained exact-output observer; press context was validated by Owner.
        let status = unsafe { oink_exit_volume_key(self.handle, action, modifiers, &mut native) };
        if status == 0 {
            Ok(())
        } else {
            Err(fault(status, native))
        }
    }
    fn write(&mut self, reading: Reading) -> Result<(), Fault> {
        let mut native = 0;
        // SAFETY: live exact-output observer; the owner already checked the
        // command's claim, Boot, generation, revision and absolute value.
        let status = unsafe {
            oink_exit_volume_set(
                self.handle,
                f32::from(reading.percent) / 100.0,
                reading.muted,
                &mut native,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(fault(status, native))
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: exclusive owner; close detaches and drains queued callbacks.
        unsafe { oink_exit_volume_close(self.handle) };
    }
}
