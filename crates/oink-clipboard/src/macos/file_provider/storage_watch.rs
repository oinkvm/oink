//! Supplemental, best-effort invalidation for external writes.
//! Owned store/provider mutations invalidate at commit/close independently.
//! On macOS 26.6.2 (25G83), a just-notified file had no modify callback for
//! at least 40s under filesystem load with dispatch or RunLoop scheduling. Separate-
//! process load also reproduced missed callbacks (10s observation). FlushSync,
//! fsync and latency changes did not establish delivery.
//! Observed in late-callback, RunLoop and separate-process traces.
//! Therefore long-held external writes before close and same-user tampering with
//! private catalog/staging/store paths have only this best-effort signal. No
//! polling or per-operation FSEvents deadline is part of the accounting contract.
//!
//! Native cache invalidation. SDK contract: CoreServices.framework/Frameworks/
//! FSEvents.framework/Headers/FSEvents.h (FSEventStreamContext, create flags,
//! FSEventStreamSetDispatchQueue, FSEventStreamInvalidate).
use std::{ffi::c_void, path::Path, path::PathBuf, ptr::NonNull, sync::Arc};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFArray, CFString};
use objc2_core_services::*;

const UNAVAILABLE: &str = "cache storage watch unavailable";

pub(super) struct StorageWatch {
    stream: FSEventStreamRef,
    queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: stream operations require exclusive ownership; the parent keeps this
// handle behind a Mutex. Callbacks run on the serial queue and only borrow an
// immutable PathBuf retained independently by the stream's context callbacks.
unsafe impl Send for StorageWatch {}

impl StorageWatch {
    pub(super) fn new(root: &Path) -> Result<Self, String> {
        let path = root.canonicalize().map_err(|_| UNAVAILABLE.to_owned())?;
        let path = path.to_str().ok_or_else(|| UNAVAILABLE.to_owned())?;
        let paths = CFArray::from_retained_objects(&[CFString::from_str(path)]);
        let root = Arc::new(root.to_owned());
        let mut context = FSEventStreamContext {
            version: 0,
            info: Arc::as_ptr(&root).cast_mut().cast(),
            retain: Some(retain_root),
            release: Some(release_root),
            copyDescription: None,
        };
        let queue = DispatchQueue::new("com.hachiwii.oink.cache-storage", None);
        // FSEvents watches the whole hierarchy. FileEvents is necessary to keep
        // data/metadata edits visible, rather than just directory changes. Keep
        // the previous backend's 0s latency + NoDefer for prompt invalidation;
        // the service can still coalesce events, so one change per matching
        // callback batch suffices: cache consumers rescan the entire root.
        // SAFETY: paths contains CFStrings; context callbacks retain the Arc for
        // the stream's lifetime, including callbacks queued during teardown.
        let stream = unsafe {
            FSEventStreamCreate(
                None,
                Some(storage_events),
                &mut context,
                paths.as_opaque(),
                u64::MAX, // kFSEventStreamEventIdSinceNow in FSEvents.h
                0.0,
                kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer,
            )
        };
        if stream.is_null() {
            return Err(UNAVAILABLE.to_owned());
        }
        // SAFETY: the owned stream is live and the newly created serial queue
        // is active. FSEvents.h requires scheduling before Start/Invalidate.
        unsafe {
            FSEventStreamSetDispatchQueue(stream, Some(&queue));
            if !FSEventStreamStart(stream) {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Err(UNAVAILABLE.to_owned());
            }
        }
        Ok(Self { stream, queue })
    }
}

impl Drop for StorageWatch {
    fn drop(&mut self) {
        // SAFETY: we exclusively own this started stream. Invalidate unschedules
        // it; drain the serial queue before releasing our final stream handle.
        // This never runs on the callback queue (callbacks only publish changes).
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
        }
        self.queue.exec_sync(|| {});
        // SAFETY: the stream is stopped, invalidated, and owned by this handle.
        unsafe { FSEventStreamRelease(self.stream) };
    }
}

unsafe extern "C-unwind" fn retain_root(info: *const c_void) -> *const c_void {
    // SAFETY: Create receives a live Arc<PathBuf>; each native retain/release
    // pair owns one strong reference independently of the constructor's Arc.
    unsafe { Arc::increment_strong_count(info.cast::<PathBuf>()) };
    info
}

unsafe extern "C-unwind" fn release_root(info: *const c_void) {
    // SAFETY: balances the strong reference acquired by retain_root.
    unsafe { Arc::decrement_strong_count(info.cast::<PathBuf>()) };
}

fn changes_cache(flags: FSEventStreamEventFlags) -> bool {
    // Coalesced/dropped events require the same whole-root reconciliation as
    // concrete file mutations (FSEvents.h MustScanSubDirs contract).
    let changes = kFSEventStreamEventFlagMustScanSubDirs
        | kFSEventStreamEventFlagKernelDropped
        | kFSEventStreamEventFlagUserDropped
        | kFSEventStreamEventFlagRootChanged
        | kFSEventStreamEventFlagMount
        | kFSEventStreamEventFlagUnmount
        | kFSEventStreamEventFlagItemCreated
        | kFSEventStreamEventFlagItemRemoved
        | kFSEventStreamEventFlagItemRenamed
        | kFSEventStreamEventFlagItemInodeMetaMod
        | kFSEventStreamEventFlagItemFinderInfoMod
        | kFSEventStreamEventFlagItemChangeOwner
        | kFSEventStreamEventFlagItemXattrMod
        | kFSEventStreamEventFlagItemModified;
    flags & kFSEventStreamEventFlagHistoryDone == 0 && flags & changes != 0
}

unsafe extern "C-unwind" fn storage_events(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    count: usize,
    _paths: NonNull<c_void>,
    flags: NonNull<FSEventStreamEventFlags>,
    _ids: NonNull<FSEventStreamEventId>,
) {
    // SAFETY: FSEvents supplies count flags and keeps the retained context alive
    // throughout the callback. Paths/IDs are unnecessary for root invalidation.
    let (flags, root) = unsafe {
        (
            std::slice::from_raw_parts(flags.as_ptr(), count),
            &*info.cast::<PathBuf>(),
        )
    };
    if flags.iter().copied().any(changes_cache) {
        crate::cache::changed(root);
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn existing_root_starts_and_stops_watch() {
        let temp = tempfile::tempdir().unwrap();
        let watch = StorageWatch::new(temp.path()).unwrap();
        drop(watch);
    }

    #[test]
    fn missing_root_reports_watch_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            StorageWatch::new(&temp.path().join("missing")).err().as_deref(),
            Some(UNAVAILABLE)
        );
    }

    #[test]
    fn event_flags_preserve_create_modify_remove_filter() {
        for flag in [
            kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagKernelDropped,
            kFSEventStreamEventFlagUserDropped,
            kFSEventStreamEventFlagItemCreated,
            kFSEventStreamEventFlagItemModified,
            kFSEventStreamEventFlagItemRemoved,
            kFSEventStreamEventFlagItemRenamed,
            kFSEventStreamEventFlagItemInodeMetaMod,
            kFSEventStreamEventFlagItemFinderInfoMod,
            kFSEventStreamEventFlagItemChangeOwner,
            kFSEventStreamEventFlagItemXattrMod,
            kFSEventStreamEventFlagRootChanged,
            kFSEventStreamEventFlagMount,
            kFSEventStreamEventFlagUnmount,
        ] {
            assert!(changes_cache(flag | kFSEventStreamEventFlagItemIsFile));
        }
        for flag in [
            kFSEventStreamEventFlagNone,
            kFSEventStreamEventFlagItemIsDir,
            kFSEventStreamEventFlagHistoryDone | kFSEventStreamEventFlagItemCreated,
        ] {
            assert!(!changes_cache(flag));
        }
    }
}
