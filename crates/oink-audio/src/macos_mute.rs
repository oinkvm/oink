//! HAL endpoint observer plus the same durable mute journal used on Windows.
use crate::{
    endpoint_mute::Lease,
    recovery::{EndpointBackend, EndpointState, Journal, RecoveryOutcome, RecoveryReport},
};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::Path,
};

unsafe extern "C" {
    fn oink_audio_endpoint_exists(uid: *const c_char, error: *mut c_char, capacity: usize) -> i32;
    fn oink_audio_endpoint_open(uid: *const c_char, error: *mut c_char, capacity: usize) -> *mut c_void;
    fn oink_audio_endpoint_close(observer: *mut c_void);
    fn oink_audio_endpoint_on_change(observer: *mut c_void, wake: extern "C" fn(*mut c_void), context: *mut c_void);
    fn oink_audio_endpoint_read(
        observer: *mut c_void,
        volume: *mut f32,
        muted: *mut bool,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn oink_audio_endpoint_flags(observer: *mut c_void) -> u32;
    fn oink_audio_endpoint_set_mute(
        observer: *mut c_void,
        muted: bool,
        release_only: bool,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn oink_audio_mute_lock() -> i32;
    fn oink_audio_mute_unlock(fd: i32);
}
struct SystemLock(i32);
impl SystemLock {
    fn acquire() -> Result<Self, String> {
        // SAFETY: no borrowed pointers; nonblocking OS lock, no audio changes.
        let fd = unsafe { oink_audio_mute_lock() };
        if fd < 0 {
            return Err(
                "source mute is owned by another process or its lock is unavailable; no endpoint changed".into(),
            );
        }
        Ok(Self(fd))
    }
}
impl Drop for SystemLock {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the live descriptor.
        unsafe { oink_audio_mute_unlock(self.0) };
    }
}

struct Observer {
    raw: *mut c_void,
    uid: String,
    _owner: Box<std::thread::Thread>,
}
impl Observer {
    fn new(uid: &str) -> Result<Self, String> {
        let c_uid = CString::new(uid).map_err(|_| "source output UID contains NUL")?;
        let mut error = [0 as c_char; 512];
        // SAFETY: terminated UID and bounded writable error buffer. The handle
        // owns its callback context until close/drain on this ordinary worker.
        let raw = unsafe { oink_audio_endpoint_open(c_uid.as_ptr(), error.as_mut_ptr(), error.len()) };
        if raw.is_null() {
            return Err(message(&error));
        }
        let mut owner = Box::new(std::thread::current());
        extern "C" fn wake(context: *mut c_void) {
            // SAFETY: Observer retains the boxed thread until native close drains callbacks.
            unsafe { &*context.cast::<std::thread::Thread>() }.unpark();
        }
        // SAFETY: stable boxed context outlives the native listener registration.
        unsafe { oink_audio_endpoint_on_change(raw, wake, (&mut *owner as *mut std::thread::Thread).cast()) };
        Ok(Self {
            raw,
            uid: uid.into(),
            _owner: owner,
        })
    }
    fn flags(&self) -> u32 {
        // SAFETY: live handle; only process-local atomics are read.
        unsafe { oink_audio_endpoint_flags(self.raw) }
    }
    fn write(&self, uid: &str, mute: bool, release_only: bool) -> Result<(), String> {
        if uid != self.uid {
            return Err("source mute endpoint mismatch".into());
        }
        let mut error = [0 as c_char; 512];
        // SAFETY: live observer and bounded error buffer. Native code rechecks
        // exact UID and user ownership before writing only the mute property.
        let status =
            unsafe { oink_audio_endpoint_set_mute(self.raw, mute, release_only, error.as_mut_ptr(), error.len()) };
        if status == 0 { Ok(()) } else { Err(message(&error)) }
    }
}
fn message(error: &[c_char]) -> String {
    // SAFETY: every caller uses a zeroed buffer and bounded native snprintf.
    unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned()
}
impl EndpointBackend for Observer {
    fn read(&self, uid: &str) -> Result<Option<EndpointState>, String> {
        if uid != self.uid {
            return Err("source mute endpoint mismatch".into());
        }
        let (mut volume, mut muted) = (0.0, false);
        let mut error = [0 as c_char; 512];
        // SAFETY: live observer, writable scalar outputs and error buffer.
        match unsafe { oink_audio_endpoint_read(self.raw, &mut volume, &mut muted, error.as_mut_ptr(), error.len()) } {
            1 => Ok(Some(EndpointState {
                endpoint: uid.into(),
                volume,
                muted,
            })),
            0 => Ok(None),
            _ => Err(message(&error)),
        }
    }
    fn set_mute(&self, uid: &str, mute: bool) -> Result<(), String> {
        self.write(uid, mute, false)
    }
    fn ownership_lost(&self) -> bool {
        self.flags() & 1 != 0
    }
    fn mute_ownership_lost(&self) -> bool {
        self.flags() & 2 != 0
    }
    fn release_owned_mute(&self, uid: &str) -> Result<(), String> {
        self.write(uid, false, true)
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: exclusively owned handle; close removes listeners and drains
        // their serial queue before releasing the native callback context.
        unsafe { oink_audio_endpoint_close(self.raw) };
    }
}
struct Unavailable;
impl EndpointBackend for Unavailable {
    fn read(&self, _: &str) -> Result<Option<EndpointState>, String> {
        Ok(None)
    }
    fn set_mute(&self, _: &str, _: bool) -> Result<(), String> {
        Err("source output unavailable".into())
    }
    fn ownership_lost(&self) -> bool {
        false
    }
}
fn endpoint_exists(uid: &str) -> Result<bool, String> {
    let uid = CString::new(uid).map_err(|_| "source output UID contains NUL")?;
    let mut error = [0 as c_char; 512];
    // SAFETY: terminated UID and bounded error buffer; no OS mutations.
    match unsafe { oink_audio_endpoint_exists(uid.as_ptr(), error.as_mut_ptr(), error.len()) } {
        1 => Ok(true),
        0 => Ok(false),
        _ => Err(message(&error)),
    }
}
fn recover_locked(journal: &mut Journal) -> Result<RecoveryReport, String> {
    let mut report = RecoveryReport {
        outcome: RecoveryOutcome::NothingToRecover,
        endpoint: None,
        crash_intent_uncertain: false,
    };
    let mut deferred_report = None;
    for uid in journal.endpoints() {
        journal.select_endpoint(&uid)?;
        report = if !journal.has_pending_restore_for(&uid) {
            journal.recover(&Unavailable, true)?
        } else {
            match Observer::new(&uid) {
                Ok(observer) => journal.recover(&observer, true)?,
                Err(error) => {
                    if endpoint_exists(&uid)? {
                        return Err(error);
                    }
                    // CoreAudio does not distinguish unplugged hardware from an
                    // uninstalled HAL device by UID. Retain its controls for replug,
                    // independently of a lease on the replacement default output.
                    tracing::warn!(%error, "saved source output unavailable; deferring mute recovery");
                    journal.recover(&Unavailable, true)?
                }
            }
        };
        if report.outcome == RecoveryOutcome::EndpointUnavailable {
            journal.defer_unavailable()?;
            deferred_report.get_or_insert(report.clone());
        }
    }
    Ok(deferred_report.unwrap_or(report))
}

pub fn recover(dir: &Path) -> Result<RecoveryReport, String> {
    let _lock = SystemLock::acquire()?;
    recover_locked(&mut Journal::open(dir)?)
}
pub(crate) struct MuteLease {
    // Drop backend callbacks/journal before releasing cross-instance ownership.
    lease: Lease<Observer>,
    _lock: SystemLock,
}
impl MuteLease {
    pub(crate) fn start(dir: &Path, uid: &str) -> Result<Self, String> {
        let lock = SystemLock::acquire()?;
        let mut journal = Journal::open(dir)?;
        let report = recover_locked(&mut journal)?;

        if report.crash_intent_uncertain {
            tracing::warn!(
                ?report,
                "mute recovered using matching state; user actions while process was absent cannot be known"
            );
        }
        let lease = Lease::start(journal, Observer::new(uid)?, uid).map_err(|error| {
            // Failed acquisition already ran its RAII restore. Surface any
            // unresolved durable marker even though no Capture was returned.
            match Journal::open(dir) {
                Ok(journal) if !journal.has_active_pending_restore() => error,
                _ => format!("capture_mute_release_unconfirmed: {error}"),
            }
        })?;
        Ok(Self { lease, _lock: lock })
    }
    pub(crate) fn check_ownership(&mut self) -> Result<bool, String> {
        self.lease.check_ownership()
    }
    pub(crate) fn default_changed(&self) -> bool {
        self.lease.backend.flags() & 4 != 0
    }
    pub(crate) fn finish(&mut self) -> Result<(), String> {
        self.lease.finish()
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::{fs::OpenOptions, os::fd::IntoRawFd};

    #[test]
    fn inherited_descriptor_does_not_extend_native_mute_lock_ownership() {
        // Exercise the production release function on an isolated file, never
        // the machine-wide mute lock or a native audio endpoint.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.lock");
        let open = || {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let file = open();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        let inherited = file.try_clone().unwrap();
        let owner = SystemLock(file.into_raw_fd());
        assert!(fs2::FileExt::try_lock_exclusive(&open()).is_err());
        drop(owner);
        let replacement = open();
        fs2::FileExt::try_lock_exclusive(&replacement)
            .expect("native lock ownership ends before an inherited descriptor closes");
        drop(inherited);
        assert!(fs2::FileExt::try_lock_exclusive(&open()).is_err());
    }
}
