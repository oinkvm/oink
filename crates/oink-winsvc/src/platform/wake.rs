//! Owned kernel wake sources. Notifications only request a new observation;
//! none supplies authorization or an input target.
use super::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use windows_sys::Win32::{Storage::FileSystem::*, System::Registry::*};

pub(super) struct Hub {
    pub stop: Arc<Event>,
    pub broker: Arc<Event>,
    pub launch: Arc<Event>,
    pub update: Arc<Event>,
    pub engine: Arc<Event>,
    pub sessions: crossbeam_queue::SegQueue<crate::supervision::SessionEvent>,
}
impl Hub {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            stop: Event::new(true)?,
            broker: Event::new(false)?,
            launch: Event::new(false)?,
            update: Event::new(false)?,
            engine: Event::new(false)?,
            sessions: Default::default(),
        })
    }
    pub(super) fn session(&self, event: crate::supervision::SessionEvent) {
        self.sessions.push(event);
        self.broker.signal();
        self.session_ready();
    }
    pub(super) fn session_ready(&self) {
        self.launch.signal();
        self.update.signal();
        self.engine.signal();
    }
    pub(super) fn stop(&self) {
        self.stop.signal();
        self.engine.signal();
        self.session_ready();
        self.broker.signal();
    }
}

pub(super) struct Event(Handle);
// SAFETY: event operations are thread-safe and ownership lasts through all Arc
// references and registered callbacks. The underlying handle is never replaced.
unsafe impl Send for Event {}
// SAFETY: concurrent SetEvent/wait operations do not mutate Rust-owned state.
unsafe impl Sync for Event {}
impl Event {
    pub(super) fn new(manual: bool) -> io::Result<Arc<Self>> {
        // SAFETY: private unnamed event, no inherited handles.
        Ok(Arc::new(Self(Handle::new(unsafe {
            CreateEventW(ptr::null(), i32::from(manual), 0, ptr::null())
        })?)))
    }
    pub(super) fn raw(&self) -> HANDLE {
        self.0.0
    }
    pub(super) fn is_set(&self) -> bool {
        // SAFETY: nonblocking read of a live event; used for manual-reset stop.
        unsafe { WaitForSingleObject(self.raw(), 0) == WAIT_OBJECT_0 }
    }
    pub(super) fn signal(&self) {
        // SAFETY: Arc keeps this event live, including in the short OS callback.
        unsafe {
            SetEvent(self.raw());
        }
    }
}

pub(super) enum Woke {
    Signal(usize),
    Deadline,
}
pub(super) fn wait(handles: &[HANDLE], deadline: Option<Instant>) -> io::Result<Woke> {
    let timeout = deadline.map_or(INFINITE, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(1_000_000)
            .min(u128::from(INFINITE - 1)) as u32
    });
    // SAFETY: callers retain every source through the wait; the set is nonempty
    // and limited to kernel wake sources (processes use thread-pool waits).
    let result = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
    if result == WAIT_TIMEOUT {
        return Ok(Woke::Deadline);
    }
    if (WAIT_OBJECT_0..WAIT_OBJECT_0 + handles.len() as u32).contains(&result) {
        return Ok(Woke::Signal((result - WAIT_OBJECT_0) as usize));
    }
    Err(io::Error::last_os_error())
}

pub(super) struct Process {
    identity: (u32, u64),
    _wait: Object,
}
pub(super) struct Object {
    _object: Handle,
    wait: PTP_WAIT,
    _event: Arc<Event>,
}
// SAFETY: the retained Arc and process outlive the wait; Drop cancels and drains
// callbacks before freeing either. This callback never takes a policy lock.
unsafe extern "system" fn exited(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut core::ffi::c_void,
    _wait: PTP_WAIT,
    _result: u32,
) {
    // SAFETY: Object retains the Arc allocation until this callback drains.
    let event = unsafe { &*context.cast::<Event>() };
    event.signal();
}
impl Process {
    pub(super) fn identity(process: &Handle) -> io::Result<(u32, u64)> {
        // SAFETY: query-only use of a retained creation object, including exit.
        Ok((unsafe { GetProcessId(process.0) }, prelogin::created(process.0)?))
    }
    pub(super) fn new(process: Handle, event: Arc<Event>) -> io::Result<Self> {
        let identity = Self::identity(&process)?;
        Ok(Self {
            identity,
            _wait: Object::new(process, event)?,
        })
    }
    pub(super) fn exited(&self) -> bool {
        // SAFETY: retained creation object, including after exit.
        unsafe { WaitForSingleObject(self._wait._object.0, 0) == WAIT_OBJECT_0 }
    }
    pub(super) fn matches(&self, identity: (u32, u64)) -> bool {
        self.identity == identity
    }
}
impl Object {
    pub(super) fn new(object: Handle, event: Arc<Event>) -> io::Result<Self> {
        // SAFETY: Arc's allocation is stable and retained until callbacks drain.
        let wait = unsafe { CreateThreadpoolWait(Some(exited), Arc::as_ptr(&event).cast_mut().cast(), ptr::null()) };
        if wait == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: owned process + wait, no timeout. A signaled creation object
        // calls back once; it is not rearmed on the permanently signaled object.
        unsafe {
            SetThreadpoolWait(wait, object.0, ptr::null());
        }
        Ok(Self {
            _object: object,
            wait,
            _event: event,
        })
    }
    pub(super) fn thread<T>(thread: &std::thread::JoinHandle<T>, event: Arc<Event>) -> io::Result<Self> {
        use std::os::windows::io::{AsHandle, IntoRawHandle};
        let owned = thread.as_handle().try_clone_to_owned()?;
        Self::new(Handle::new(owned.into_raw_handle())?, event)
    }
}
impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: first cancel, then drain callbacks, then close. The process
        // and event allocation are still alive until after this destructor returns.
        unsafe {
            SetThreadpoolWait(self.wait, ptr::null_mut(), ptr::null());
            WaitForThreadpoolWaitCallbacks(self.wait, 1);
            CloseThreadpoolWait(self.wait);
        }
    }
}

pub(super) struct Registry {
    root: HKEY,
    requested: String,
    key: HKEY,
    event: Arc<Event>,
}
impl Registry {
    pub(super) fn new(root: HKEY, requested: &str) -> io::Result<Self> {
        let event = Event::new(false)?;
        let mut path = requested;
        let key = loop {
            let mut key = ptr::null_mut();
            // SAFETY: read/notify only; never creates or loads a user hive.
            let result = if path.is_empty() {
                // Empty RegOpenKeyEx on a predefined hive returns the same
                // pseudo-handle, so closing it would not cancel notifications.
                // A local connection gives us an owned root handle to close.
                // SAFETY: NULL names this computer; output is an owned root key.
                unsafe { RegConnectRegistryW(ptr::null(), root, &mut key) }
            } else {
                // SAFETY: NUL-terminated path, valid root and output pointer.
                unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, KEY_NOTIFY, &mut key) }
            };
            if result == 0 {
                break key;
            }
            if result != ERROR_FILE_NOT_FOUND && result != ERROR_PATH_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(result as i32));
            }
            path = path.rsplit_once('\\').map_or("", |(parent, _)| parent);
        };
        let watch = Self {
            root,
            requested: requested.into(),
            key,
            event,
        };
        // SAFETY: one asynchronous registration per handle. Missing descendants
        // are covered by name changes in the closest existing ancestor; re-open
        // and re-arm before reading after each notification, never double-arm.
        check_registry(unsafe {
            RegNotifyChangeKeyValue(
                watch.key,
                i32::from(path == requested),
                REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET,
                watch.event.raw(),
                1,
            )
        })?;
        Ok(watch)
    }
    pub(super) fn raw(&self) -> HANDLE {
        self.event.raw()
    }
    pub(super) fn rearm(&mut self) -> io::Result<()> {
        *self = Self::new(self.root, &self.requested)?;
        Ok(())
    }
}
fn check_registry(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}
impl Drop for Registry {
    fn drop(&mut self) {
        // SAFETY: owned key; event remains alive through cancellation on close.
        unsafe {
            RegCloseKey(self.key);
        }
    }
}

pub(super) struct Directory {
    requested: PathBuf,
    handle: HANDLE,
}
impl Directory {
    pub(super) fn new(requested: &Path) -> io::Result<Self> {
        let mut path = requested;
        while !path.is_dir() {
            path = path.parent().ok_or_else(denied)?;
        }
        // SAFETY: notification only. File notifications never authorize access;
        // the update store still performs its original protected-path checks.
        let handle = unsafe {
            FindFirstChangeNotificationW(
                wide(path.to_str().ok_or_else(denied)?).as_ptr(),
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            requested: requested.into(),
            handle,
        })
    }
    pub(super) fn raw(&self) -> HANDLE {
        self.handle
    }
    pub(super) fn rearm(&mut self) -> io::Result<()> {
        *self = Self::new(&self.requested)?;
        Ok(())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: change notification handles require their matching close API.
        unsafe {
            FindCloseChangeNotification(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::windows::io::IntoRawHandle, time::Duration};
    #[test]
    fn process_exit_wakes_the_owner_without_reopening_a_pid() {
        let event = Event::new(false).unwrap();
        let child = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "exit 7"])
            .spawn()
            .unwrap();
        let process = Handle::new(child.into_raw_handle()).unwrap();
        let watch = Process::new(process, event.clone()).unwrap();
        assert!(matches!(
            wait(&[event.raw()], Some(Instant::now() + Duration::from_secs(2))).unwrap(),
            Woke::Signal(0)
        ));
        drop(watch);
    }
    #[test]
    fn thread_completion_wakes_after_the_thread_object_is_signaled() {
        let event = Event::new(false).unwrap();
        let release = Event::new(true).unwrap();
        let gate = release.clone();
        let thread = std::thread::spawn(move || {
            wait(&[gate.raw()], None).unwrap();
        });
        let watch = Object::thread(&thread, event.clone()).unwrap();
        release.signal();
        assert!(matches!(
            wait(&[event.raw()], Some(Instant::now() + Duration::from_secs(2))).unwrap(),
            Woke::Signal(0)
        ));
        assert!(thread.is_finished());
        thread.join().unwrap();
        drop(watch);
    }
    #[test]
    fn registry_watch_follows_a_late_key_then_value_changes() {
        // SAFETY: read-only current thread identity for a unique private key.
        let tid = unsafe { GetCurrentThreadId() };
        let name = format!(r"Software\Oink-Event-Test-{}-{tid}", std::process::id());
        let requested = format!(r"{name}\Run");
        let mut watch = Registry::new(HKEY_CURRENT_USER, &requested).unwrap();
        let mut key = ptr::null_mut();
        // SAFETY: unique test-only HKCU subtree, never an installed Oink key.
        check_registry(unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(&requested).as_ptr(),
                0,
                ptr::null(),
                0,
                KEY_SET_VALUE,
                ptr::null(),
                &mut key,
                ptr::null_mut(),
            )
        })
        .unwrap();
        assert!(matches!(
            wait(&[watch.raw()], Some(Instant::now() + Duration::from_secs(2))).unwrap(),
            Woke::Signal(0)
        ));
        watch.rearm().unwrap();
        // SAFETY: owned private test key, fixed one-byte value.
        check_registry(unsafe { RegSetValueExW(key, wide("test").as_ptr(), 0, REG_BINARY, b"1".as_ptr(), 1) }).unwrap();
        assert!(matches!(
            wait(&[watch.raw()], Some(Instant::now() + Duration::from_secs(2))).unwrap(),
            Woke::Signal(0)
        ));
        // SAFETY: close the private key before deleting its unique subtree.
        unsafe {
            RegCloseKey(key);
        }
        drop(watch);
        // SAFETY: deletes only the unique test subtree created above.
        check_registry(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&name).as_ptr()) }).unwrap();
    }
    #[test]
    fn directory_change_wakes_after_an_external_worker_writes() {
        let root = tempfile::tempdir().unwrap();
        let mut watch = Directory::new(root.path()).unwrap();
        std::fs::write(root.path().join("worker-ready.json"), b"{}").unwrap();
        assert!(matches!(
            wait(&[watch.raw()], Some(Instant::now() + Duration::from_secs(2))).unwrap(),
            Woke::Signal(0)
        ));
        watch.rearm().unwrap();
        assert!(matches!(
            wait(&[watch.raw()], Some(Instant::now())).unwrap(),
            Woke::Deadline
        ));
    }
}
