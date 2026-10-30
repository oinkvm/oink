//! Retirement handles and handle-free hydration edit checks.
use super::*;
use std::{
    fs::File,
    os::windows::{ffi::OsStringExt, io::AsRawHandle},
};

fn identity(handle: HANDLE) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: live owned/referenced handle and fixed-size output.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
}

/// Referencing an exclusive RH handle is used only for retirement, after consumer
/// drain. Data hydration checks use callback identities without a data handle.
pub(super) struct Protected {
    api: Arc<Api>,
    protected: HANDLE,
    pub(super) handle: HANDLE,
    referenced: bool,
}
// SAFETY: cfapi/Win32 handles have no thread affinity. Ownership is unique,
// methods retain the reference, and Drop runs only after all shared borrows end.
unsafe impl Send for Protected {}
// SAFETY: shared access only queries the live handle; deletion consumes the owner.
unsafe impl Sync for Protected {}
impl Protected {
    pub(super) fn open(api: Arc<Api>, path: &Path, flags: i32) -> io::Result<Self> {
        Self::open_traced(api, path, flags, HydrationTrace::default())
    }
    pub(super) fn file_path(file: &File) -> io::Result<PathBuf> {
        let mut path = vec![0u16; 32768];
        // SAFETY: retained file handle and writable path buffer.
        let count = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), path.as_mut_ptr(), path.len() as u32, 0) };
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        if count as usize >= path.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&path[..count as usize])))
    }
    pub(super) fn for_file(api: Arc<Api>, file: &File) -> io::Result<Self> {
        let path = Self::file_path(file)?;
        let protected = Self::open(api, &path, CF_OPEN_FILE_FLAG_NONE)?;
        if identity(file.as_raw_handle())? != identity(protected.handle)? {
            return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
        }
        Ok(protected)
    }
    fn open_traced(api: Arc<Api>, path: &Path, flags: i32, trace: HydrationTrace) -> io::Result<Self> {
        let mut owned = Self::unreferenced(api, path, flags, trace)?;
        owned.reference(trace)?;
        Ok(owned)
    }
    fn unreferenced(api: Arc<Api>, path: &Path, flags: i32, trace: HydrationTrace) -> io::Result<Self> {
        // Windows 11 (26200; cldapi 10.0.26100.9278) leaks an internal
        // OpenProcess(self) handle on failed CfOpenFileWithOplock calls
        // (100 missing-path calls -> +100 handles).
        // It returns no handle the caller can close. Avoid expected failures;
        // this metadata check neither recalls data nor replaces the oplock or
        // identity checks, and a concurrent removal can still fail natively.
        trace.io(51, std::fs::symlink_metadata(path))?;
        let mut protected = std::ptr::null_mut();
        // SAFETY: path/output live for the synchronous cfapi call.
        trace.hr(51, unsafe {
            (api.open_protected)(wide(path).as_ptr(), flags, &mut protected)
        })?;
        Ok(Self {
            api,
            protected,
            handle: std::ptr::null_mut(),
            referenced: false,
        })
    }
    fn reference(&mut self, trace: HydrationTrace) -> io::Result<()> {
        // SAFETY: this owner retains the opaque cfapi handle.
        if !unsafe { (self.api.reference_protected)(self.protected) } {
            // CfReferenceProtectedHandle returns BOOLEAN, not an HRESULT or a
            // documented GetLastError value. Code zero means unavailable here.
            return trace.io(52, Err(io::ErrorKind::WouldBlock.into()));
        }
        self.referenced = true;
        // SAFETY: the successful reference pins the underlying Win32 handle.
        self.handle = unsafe { (self.api.win32_protected)(self.protected) };
        if self.handle.is_null() || self.handle == INVALID_HANDLE_VALUE {
            return trace.io(53, Err(io::ErrorKind::WouldBlock.into()));
        }
        Ok(())
    }
    fn release(&mut self) {
        if std::mem::take(&mut self.referenced) {
            // SAFETY: balance this owner's successful reference exactly once.
            unsafe { (self.api.release_protected)(self.protected) };
        }
    }
    pub(super) fn delete(self) -> io::Result<()> {
        let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: caller opened a referenced DELETE handle; input is fixed-size.
        if unsafe {
            SetFileInformationByHandle(
                self.handle,
                FileDispositionInfo,
                (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Protected {
    fn drop(&mut self) {
        self.release();
        // SAFETY: close this owner after releasing its reference.
        unsafe { (self.api.close_protected)(self.protected) };
    }
}

impl Work {
    // Hydration range checks must not open a data handle or issue a handle-based FSCTL:
    // a minifilter scan can wait behind this very hydration. The hydration
    // range API uses the callback's native file identity and filter message
    // port instead. TRANSFER_DATA atomically preserves modified/unsynced ranges
    // even if an edit races this preflight (CF_CALLBACK_PARAMETERS contract).
    pub(super) fn check_hydration_target(&self) -> io::Result<()> {
        let trace = self.trace();
        let file = self.delivery.file(self.index);
        if self.request.cancelled.load(Ordering::Acquire)
            || file.deleted.load(Ordering::Acquire)
            || file.withdrawn.load(Ordering::Acquire)
            || file.local_final.load(Ordering::Acquire)
        {
            return Err(unavailable());
        }
        let Some(query) = self.service.api.hydration_ranges else {
            // Legacy platforms cannot safely query this callback's ranges.
            // TRANSFER_DATA itself atomically preserves modified/unsynced data;
            // close/recovery still detects edits. No data handle/FSCTL fallback.
            return Ok(());
        };
        let key = (*self.service.connection.lock().unwrap()).ok_or(io::ErrorKind::NotConnected)?;
        let mut range = CF_FILE_RANGE::default();
        let mut written = 0;
        // SAFETY: callback identity, bounded range output; no file is opened.
        let code = unsafe {
            query(
                key,
                self.request.transfer,
                self.request.file_id,
                CF_PLACEHOLDER_RANGE_INFO_MODIFIED,
                0,
                -1,
                (&mut range as *mut CF_FILE_RANGE).cast(),
                size_of::<CF_FILE_RANGE>() as u32,
                &mut written,
            )
        };
        let dirty = if (code & 0xffff) == ERROR_HANDLE_EOF as i32 && written == 0 {
            self.request.total != 0
        } else if (code & 0xffff) == ERROR_MORE_DATA as i32 || (code & 0xffff) == ERROR_CLOUD_FILE_NOT_IN_SYNC as i32 {
            true
        } else {
            trace.hr(55, code)?;
            if written != 0
                && (written != size_of::<CF_FILE_RANGE>() as u32 || range.StartingOffset < 0 || range.Length <= 0)
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            written != 0
        };
        if dirty {
            let mut journal = self.service.received.lock().unwrap();
            let previous = journal
                .deliveries
                .get(&self.delivery.id)
                .and_then(|nodes| nodes.get(self.index))
                .cloned()
                .ok_or(io::ErrorKind::NotFound)?;
            let mut next = previous.clone();
            next.local_final = true;
            next.withdrawn = false;
            // Keep prior completion evidence. Partial local edits are never
            // promoted to a successful source handoff or re-read for proof.
            journal.deliveries.get_mut(&self.delivery.id).unwrap()[self.index] = next;
            if let Err(error) = trace.io(59, journal.save(&self.service.root)) {
                journal.deliveries.get_mut(&self.delivery.id).unwrap()[self.index] = previous;
                return Err(error);
            }
            file.local_final.store(true, Ordering::Release);
            file.withdrawn.store(false, Ordering::Release);
            // NOT_IN_SYNC protects the OS copy immediately. Do not open a pin
            // handle inside FETCH_DATA; restore/withdrawal publish local policy.
            return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::{
        fs::OpenOptionsExt,
        io::{FromRawHandle, OwnedHandle},
    };

    #[test]
    fn native_hidden_and_large_file_sharing_errors_keep_the_original_win32_code() {
        const CHILD: &str = "OINK_TEST_HYDRATION_LOG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Keep the text-log assertion independent of concurrent tests'
            // scoped subscribers and their process-wide callsite interest cache.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "windows::cloud::protection::tests::native_hidden_and_large_file_sharing_errors_keep_the_original_win32_code", "--test-threads=1"])
                .env(CHILD, "1")
                .status().unwrap();
            assert!(status.success());
            return;
        }
        use std::io::Write;
        #[derive(Clone)]
        struct Log(Arc<Mutex<Vec<u8>>>);
        impl Write for Log {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let log = Log(Arc::new(Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let temp = tempfile::tempdir().unwrap();
            for (index, (name, bytes)) in [(".DS_Store", 6148), ("large.bin", 179_200)].into_iter().enumerate() {
                let path = temp.path().join(name);
                std::fs::write(&path, vec![0x5a; bytes]).unwrap();
                if index == 0 {
                    assert_ne!(
                        // SAFETY: live UTF-16 path to this test's private file.
                        unsafe { SetFileAttributesW(wide(&path).as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
                        0
                    );
                }
                let trace = HydrationTrace {
                    request: Some(123),
                    index,
                };
                let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                let error = trace
                    .io(
                        50,
                        std::fs::OpenOptions::new()
                            .read(true)
                            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                            .open(&path),
                    )
                    .err()
                    .unwrap();
                assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                drop(writer);
                let reader = File::open(&path).unwrap();
                let guard = trace
                    .io(
                        50,
                        std::fs::OpenOptions::new()
                            .read(true)
                            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                            .open(&path),
                    )
                    .unwrap();
                assert_eq!(std::fs::read(&path).unwrap().len(), bytes);
                drop((guard, reader));
                // Full failing HRESULT survives conversion to io::Error.
                assert!(trace.hr(51, 0x80070185_u32 as i32).is_err());
            }
        });
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("file_stage=50"), "{output}");
        assert!(output.contains("file_request=123"));
        assert!(output.contains("file_code=32"));
        assert!(output.contains("file_stage=51"));
        assert!(output.contains("file_code=2147942789"));
        assert!(!output.contains(".DS_Store"));
        assert!(!output.contains("large.bin"));
    }

    #[test]
    fn native_delete_guard_refuses_lingering_writable_mapping() {
        use windows::Win32::{
            Foundation::HANDLE as WinHandle,
            System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile},
        };
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("received");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        // SAFETY: private writable file, unnamed mapping, system-selected length.
        let mapping =
            unsafe { CreateFileMappingW(WinHandle(file.as_raw_handle()), None, PAGE_READWRITE, 0, 0, None).unwrap() };
        // SAFETY: take sole ownership of the newly created native mapping handle.
        let mapping = unsafe { OwnedHandle::from_raw_handle(mapping.0) };
        // SAFETY: private writable mapping is retained until after unmap below.
        let view = unsafe { MapViewOfFile(WinHandle(mapping.as_raw_handle()), FILE_MAP_WRITE, 0, 0, 4096) };
        assert!(!view.Value.is_null());
        drop(file); // An ordinary share-none metadata open alone can now succeed.
        let api = Api::load().unwrap();
        let deletion = Protected::open(
            api.clone(),
            &path,
            CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_DELETE_ACCESS | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
        );
        // SAFETY: exact view returned above; no access occurs after unmapping.
        unsafe {
            UnmapViewOfFile(view).unwrap();
        }
        drop(mapping);
        assert!(deletion.is_err(), "a lingering writable mmap must exclude cleanup");
        assert!(
            Protected::open(
                api,
                &path,
                CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_DELETE_ACCESS | CF_OPEN_FILE_FLAG_WRITE_ACCESS
            )
            .is_ok()
        );
    }
}
