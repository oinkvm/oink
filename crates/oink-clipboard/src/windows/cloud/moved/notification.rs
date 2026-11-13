//! Positive filesystem completion for phone proxy handoffs.
use super::super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::{
    Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
    },
    System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
};

/// Dropping the journal's guard cancels the wait. The worker owns the directory
/// notifications until it leaves WaitForMultipleObjects, so no waited handle is
/// ever closed by another thread. No polling or elapsed-time completion rule.
pub(in crate::windows::cloud) struct Watch(Arc<OwnedHandle>, Option<mpsc::Sender<()>>);
impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the guard and worker both own this live event.
        unsafe { SetEvent(self.0.as_raw_handle()) };
    }
}
struct Change(usize);
impl Change {
    fn new(directory: &Path) -> io::Result<Self> {
        // A verbatim canonical path supports long target directory names without
        // relying on the caller process's longPathAware manifest.
        let directory = directory.canonicalize()?;
        // SAFETY: NUL-terminated directory, notification registered before ACK.
        let handle = unsafe {
            FindFirstChangeNotificationW(
                wide(&directory).as_ptr(),
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_ATTRIBUTES,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle as usize))
    }
    fn raw(&self) -> HANDLE {
        self.0 as HANDLE
    }
}
impl Drop for Change {
    fn drop(&mut self) {
        // SAFETY: unique owning notification, no outstanding wait after drop.
        unsafe { FindCloseChangeNotification(self.raw()) };
    }
}
impl Watch {
    fn id(&self) -> usize {
        self.0.as_raw_handle() as usize
    }
    pub(in crate::windows::cloud) fn activate(&mut self) {
        if let Some(ready) = self.1.take() {
            let _ = ready.send(());
        }
    }
    pub(in crate::windows::cloud) fn start(
        service: Weak<Service>,
        old: PathBuf,
        target: PathBuf,
        key: (u32, i64),
    ) -> io::Result<Self> {
        // Windows 11 build 26200 / NTFS: 25/25 successful native moves
        // (IFileOperation single/multi/folder, MoveFileEx cold/warm) produced 30
        // NOTIFY_RENAME ACKs and zero RENAME_COMPLETIONs in five-second windows;
        // a separate cold phone batch also had none in 20 seconds. Microsoft
        // reports the same missing callback:
        // https://learn.microsoft.com/en-us/answers/questions/1696802/when-will-cf-callback-type-notify-rename-completio
        // Evidence: local validation records Arm real directory
        // notifications BEFORE ACK to close the rename race. A signal only asks
        // us to check: source absent + exact volume/FRN now outside the root are
        // mandatory. Unrelated changes, delays or DELETE cannot authorize unlink.
        // FindFirst/Next retains intervening changes and needs no USN privilege;
        // specific event names are unnecessary because the FRN is revalidated.
        let source = Change::new(old.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let destination = Change::new(target.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        // SAFETY: unnamed manual-reset event; owned handles keep it alive.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: newly created event transfers ownership to OwnedHandle.
        let stop = Arc::new(unsafe { OwnedHandle::from_raw_handle(event) });
        let worker_stop = stop.clone();
        let (ready, admitted) = mpsc::channel();
        std::thread::Builder::new()
            .name("oink-proxy-move".into())
            .spawn(move || {
                // Arm OS notifications before ACK, but do not inspect journal
                // ownership until our guard is installed. A canceled old watcher
                // must never erase a later retry of the same FRN and pathname.
                if admitted.recv().is_err() {
                    return;
                }
                let watcher_id = worker_stop.as_raw_handle() as usize;
                let handles = [worker_stop.as_raw_handle(), source.raw(), destination.raw()];
                loop {
                    // SAFETY: all three handles remain owned through this wait.
                    let event = unsafe { WaitForMultipleObjects(3, handles.as_ptr(), 0, INFINITE) };
                    if event == WAIT_OBJECT_0 || event > WAIT_OBJECT_0 + 2 {
                        break;
                    }
                    let change = if event == WAIT_OBJECT_0 + 1 {
                        &source
                    } else {
                        &destination
                    };
                    // SAFETY: this handle signaled. Re-arm before opening files so
                    // changes during proof checking are retained for the next wait.
                    let rearmed = unsafe { FindNextChangeNotification(change.raw()) } != 0;
                    let Some(current) = service.upgrade() else { break };
                    if current.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let ours = current
                        .received
                        .lock()
                        .unwrap()
                        .move_watches
                        .get(&key)
                        .is_some_and(|watch| watch.id() == watcher_id);
                    if !ours {
                        break;
                    }
                    match current.track_proxy_move(&old, key.0, key.1) {
                        Ok(true) => {
                            tracing::info!(target: "oink_file_diagnostics", file_stage = 64_u8,
                            file_id = key.1, "phone move proven by directory notification");
                            break;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::debug!(kind=?error.kind(), "directory event did not prove a phone file handoff")
                        }
                    }
                    if !rearmed {
                        break;
                    }
                }
                if let Some(current) = service.upgrade() {
                    let mut journal = current.received.lock().unwrap();
                    if journal
                        .move_watches
                        .get(&key)
                        .is_some_and(|watch| watch.id() == watcher_id)
                    {
                        journal.renames.remove(&key);
                        journal.move_watches.remove(&key);
                    }
                }
            })?;
        Ok(Self(stop, Some(ready)))
    }
}
