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
    // Preserve the old backend's Create/Modify/Remove translation, including
    // renames, metadata, and mount/root changes. HistoryDone and rescan/type-only
    // notifications did not produce those kinds (nor Any) in that backend.
    let changes = kFSEventStreamEventFlagRootChanged
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
    use std::time::Duration;

    #[tokio::test]
    async fn create_modify_remove_nested_files_publish_cache_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        let file = root.join("nested/deeper/file");
        let watch = StorageWatch::new(root).unwrap();
        let mut changes = crate::cache::subscribe();
        for operation in ["create", "modify", "rename", "remove"] {
            // End the previous batch before the next operation, so an earlier
            // coalesced notification cannot satisfy the next assertion.
            // SAFETY: watch owns a started stream on its separate dispatch queue.
            unsafe { FSEventStreamFlushSync(watch.stream) };
            while changes.try_recv().is_ok() {}
            match operation {
                "create" => std::fs::write(&file, b"first").unwrap(),
                "modify" => std::fs::write(&file, b"changed contents").unwrap(),
                "rename" => std::fs::rename(&file, file.with_extension("renamed")).unwrap(),
                "remove" => std::fs::remove_file(file.with_extension("renamed")).unwrap(),
                _ => unreachable!(),
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match changes.recv().await {
                        Ok(changed) if changed == root => break,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(error) => panic!("cache changes closed: {error}"),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("no cache change after {operation}"));
        }
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
            kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagKernelDropped,
            kFSEventStreamEventFlagHistoryDone | kFSEventStreamEventFlagItemCreated,
        ] {
            assert!(!changes_cache(flag));
        }
    }
}
