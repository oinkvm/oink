//! Each name has exactly one reusable instance. A user server cannot create a
//! second same-name instance without granting FILE_CREATE_PIPE_INSTANCE to the
//! user's clients too. Precreate distinct names instead; never relax that DACL.
use super::*;
use std::{
    ffi::c_void,
    os::windows::io::{AsRawHandle, FromRawHandle},
    pin::Pin,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc, oneshot},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{IO::*, Pipes::*, Threading::*},
};
#[path = "windows/overlapped.rs"]
mod overlapped;
use overlapped::Operation;
const SLOTS: usize = 32;
const CLIENT_RIGHTS: u32 = 0x12019b;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
struct Allocation(*mut c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: descriptor/SID string was allocated by a LocalAlloc-based API.
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn sid() -> io::Result<String> {
    // SAFETY: owned token and aligned buffer live through all SID conversions.
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = File::from_raw_handle(token);
        let mut size = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW((*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _allocation = Allocation(text.cast());
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(text, len)))
    }
}
fn create(name: &str) -> io::Result<Arc<File>> {
    create_for_sid(name, &sid()?)
}
fn create_for_sid(name: &str, sid: &str) -> io::Result<Arc<File>> {
    // Exact read/write rights exclude FILE_CREATE_PIPE_INSTANCE (0x4).
    // Medium minimum allows UIAccess backend, normal UI and elevated UI for
    // the SAME SID; NWNR excludes Low. No SYSTEM/Administrators/Everyone ACE.
    // https://learn.microsoft.com/windows/win32/ipc/named-pipe-security-and-access-rights
    let sddl = wide(&format!("O:{sid}D:P(A;;0x{CLIENT_RIGHTS:x};;;{sid})S:(ML;;NWNR;;;ME)"));
    // SAFETY: parser-owned descriptor and stack attributes outlive creation.
    unsafe {
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, ptr::null_mut()) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let _allocation = Allocation(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = CreateNamedPipeW(
            wide(name).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            &attributes,
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(File::from_raw_handle(handle)))
    }
}
#[derive(Debug)]
pub(crate) struct Listener {
    owner: OwnerLock,
    path: String,
    incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<AsyncStream>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Listener {
    pub(crate) fn bind(data_dir: &Path, role: &str) -> io::Result<Self> {
        let owner = OwnerLock::acquire(data_dir, role)?;
        let path = format!(r"\\.\pipe\Oink.IPC.{}.{}", owner.owner.pid, owner.owner.nonce);
        // Create every first instance before publishing; partial failure drops
        // all handles and cannot leave a partly usable endpoint.
        let pipes = (0..SLOTS)
            .map(|slot| create(&format!("{path}.{slot}")))
            .collect::<io::Result<Vec<_>>>()?;
        let (send, receive) = mpsc::unbounded_channel();
        let tasks = pipes
            .into_iter()
            .map(|mut pipe| {
                let send = send.clone();
                tokio::spawn(async move {
                    loop {
                        let result = async {
                            let mut operation = Operation::connect(pipe.clone())?;
                            std::future::poll_fn(|cx| operation.poll(cx)).await
                        }.await;
                        if let Err(error) = result {
                            if matches!(error.raw_os_error(), Some(code) if code == ERROR_NO_DATA as i32 || code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32) {
                                // ConnectNamedPipe documents ERROR_NO_DATA when
                                // a client closed before the server disconnected.
                                // Discard that connection, retaining this instance.
                                // https://learn.microsoft.com/windows/win32/api/namedpipeapi/nf-namedpipeapi-connectnamedpipe
                                // SAFETY: the completed/cancelled connect operation
                                // was dropped above; no other I/O owns this pipe.
                                unsafe { DisconnectNamedPipe(pipe.as_raw_handle()); }
                                continue;
                            }
                            tracing::error!(%error, os_error = ?error.raw_os_error(), "native IPC pipe listener failed");
                            break;
                        }
                        let (recycle, returned) = oneshot::channel();
                        if send
                            .send(AsyncStream {
                                pipe: Some(pipe),
                                recycle: Some(recycle),
                                read: None, write: None, buffered: Vec::new(), offset: 0,
                            })
                            .is_err()
                        {
                            break;
                        }
                        let Ok(next) = returned.await else {
                            break;
                        };
                        pipe = next;
                    }
                })
            })
            .collect();
        Ok(Self {
            owner,
            path,
            incoming: tokio::sync::Mutex::new(receive),
            tasks,
        })
    }
    pub(crate) fn release(&self) {
        self.owner.release();
    }
    pub(crate) fn address(&self) -> Address {
        Address::Native {
            path: self.path.clone(),
            owner: self.owner.owner.clone(),
            lock: self.owner.path.clone(),
        }
    }
    pub(crate) async fn accept(&self) -> io::Result<(AsyncStream, ())> {
        self.incoming
            .lock()
            .await
            .recv()
            .await
            .map(|s| (s, ()))
            .ok_or_else(|| io::ErrorKind::BrokenPipe.into())
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
#[derive(Debug)]
pub(crate) struct AsyncStream {
    pipe: Option<Arc<File>>,
    recycle: Option<oneshot::Sender<Arc<File>>>,
    read: Option<Operation>,
    write: Option<Operation>,
    buffered: Vec<u8>,
    offset: usize,
}
impl Drop for AsyncStream {
    fn drop(&mut self) {
        // No I/O or callback may survive into the next connection. Retaining a
        // Mio named-pipe stream across DisconnectNamedPipe carries cached EOF;
        // native operations instead own/drain completion state for every lease.
        drop(self.read.take());
        drop(self.write.take());
        if let Some(pipe) = self.pipe.take() {
            // SAFETY: all I/O above was drained; this instance remains owned.
            unsafe {
                DisconnectNamedPipe(pipe.as_raw_handle());
            }
            if let Some(send) = self.recycle.take() {
                let _ = send.send(pipe);
            }
        }
    }
}
impl AsyncRead for AsyncStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.offset == self.buffered.len() {
            if self.read.is_none() {
                self.read = Some(Operation::read(
                    self.pipe.as_ref().unwrap().clone(),
                    out.remaining().min(65536),
                )?);
            }
            let count = match self.read.as_mut().unwrap().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => result?,
            };
            let mut operation = self.read.take().unwrap();
            operation.buffer.truncate(count);
            self.buffered = std::mem::take(&mut operation.buffer);
            self.offset = 0;
        }
        let count = out.remaining().min(self.buffered.len() - self.offset);
        out.put_slice(&self.buffered[self.offset..self.offset + count]);
        self.offset += count;
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for AsyncStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if self.write.is_none() {
            self.write = Some(Operation::write(
                self.pipe.as_ref().unwrap().clone(),
                &bytes[..bytes.len().min(65536)],
            )?);
        }
        let result = self.write.as_mut().unwrap().poll(cx);
        if result.is_ready() {
            self.write = None;
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(operation) = self.write.as_mut() {
            match operation.poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    result?;
                    self.write = None;
                }
            }
        }
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
#[derive(Debug)]
pub struct Stream {
    file: File,
    timeouts: Mutex<(Option<Duration>, Option<Duration>)>,
    gate: Mutex<()>,
    closed: AtomicBool,
}
impl Stream {
    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.timeouts.lock().unwrap().0 = t;
        Ok(())
    }
    pub fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.timeouts.lock().unwrap().1 = t;
        Ok(())
    }
    pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
        let _gate = self.gate.lock().unwrap();
        self.closed.store(true, Ordering::Release);
        // SAFETY: shared handle remains alive. Each waiter drains its own
        // cancelled OVERLAPPED before releasing the event and caller's buffer.
        unsafe {
            CancelIoEx(self.file.as_raw_handle(), ptr::null());
        }
        Ok(())
    }
    fn transfer(&self, bytes: *mut u8, len: usize, read: bool) -> io::Result<usize> {
        if len == 0 {
            return Ok(0);
        }
        let timeout = {
            let t = self.timeouts.lock().unwrap();
            if read { t.0 } else { t.1 }
        };
        // SAFETY: event is owned locally; the stack OVERLAPPED and caller buffer
        // remain pinned in this synchronous function until completion is drained.
        unsafe {
            let event = CreateEventW(ptr::null(), 1, 0, ptr::null());
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = File::from_raw_handle(event);
            let mut operation = OVERLAPPED {
                hEvent: event.as_raw_handle(),
                ..Default::default()
            };
            let gate = self.gate.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let handle = self.file.as_raw_handle();
            let size = len.min(u32::MAX as usize) as u32;
            let ok = if read {
                ReadFile(handle, bytes, size, ptr::null_mut(), &mut operation)
            } else {
                WriteFile(handle, bytes.cast_const(), size, ptr::null_mut(), &mut operation)
            };
            let error = io::Error::last_os_error();
            drop(gate);
            if ok == 0 && error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                if read && error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    return Ok(0);
                }
                return Err(error);
            }
            if ok == 0 {
                let ms = timeout
                    .map(|t| t.as_millis().min(u128::from(u32::MAX - 1)) as u32)
                    .unwrap_or(INFINITE);
                let wait = WaitForSingleObject(event.as_raw_handle(), ms);
                if wait != WAIT_OBJECT_0 {
                    CancelIoEx(handle, &operation);
                    let mut count = 0;
                    GetOverlappedResult(handle, &operation, &mut count, 1);
                    return Err(if wait == WAIT_TIMEOUT {
                        io::Error::new(io::ErrorKind::TimedOut, "local IPC operation timed out")
                    } else {
                        io::Error::last_os_error()
                    });
                }
            }
            let mut count = 0;
            if GetOverlappedResult(handle, &operation, &mut count, 0) == 0 {
                let e = io::Error::last_os_error();
                if read && e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                    return Ok(0);
                }
                return Err(e);
            }
            Ok(count as usize)
        }
    }
}
impl Read for &Stream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.transfer(out.as_mut_ptr(), out.len(), true)
    }
}
impl Write for &Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.transfer(bytes.as_ptr().cast_mut(), bytes.len(), false)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn open(path: &str) -> io::Result<File> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let start = NEXT.fetch_add(1, Ordering::Relaxed);
    for offset in 0..SLOTS {
        let name = wide(&format!("{path}.{}", (start + offset) % SLOTS));
        // SAFETY: terminated local name, non-inherited existing pipe handle;
        // exact rights cannot create instances. Identification prevents a rogue
        // server from impersonating an elevated/UIAccess caller.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                CLIENT_RIGHTS,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: successful CreateFile transferred this unique handle.
            return Ok(unsafe { File::from_raw_handle(handle) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "all local IPC connections are occupied",
    ))
}
pub(crate) fn connect(path: &str, pid: u32, timeout: Duration) -> io::Result<Stream> {
    let file = open(path)?;
    let mut server = 0;
    // SAFETY: connected pipe and valid process-id output. The endpoint's locked
    // owner nonce is checked before/after connect; PID reuse cannot authenticate
    // a stale owner. No secret or command is sent before checking this handle.
    if unsafe { GetNamedPipeServerProcessId(file.as_raw_handle(), &mut server) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if server != pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC server does not match instance-lock owner",
        ));
    }
    Ok(Stream {
        file,
        timeouts: Mutex::new((Some(timeout), Some(timeout))),
        gate: Mutex::new(()),
        closed: AtomicBool::new(false),
    })
}
pub(crate) fn probe(path: &str) -> io::Result<()> {
    match open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(e),
    }
}

// LockFileEx locks are mandatory: locking the metadata itself would prevent a
// medium UI from reading its owner's PID/nonce through another handle. Reserve
// one byte beyond EOF instead; fs2's whole-file ownership probe overlaps it.
// This uses the documented beyond-EOF range behavior, not an OS workaround:
// https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-lockfileex
fn owner_range(file: &File, lock: bool) -> io::Result<()> {
    let mut operation = OVERLAPPED {
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: 0,
                OffsetHigh: 1,
            },
        },
        ..Default::default()
    };
    // SAFETY: live file and stack OVERLAPPED, FAIL_IMMEDIATELY makes acquisition
    // synchronous. Unlock uses exactly the same 4-GiB offset and one-byte range.
    let ok = unsafe {
        if lock {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut operation,
            )
        } else {
            UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut operation)
        }
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
pub(crate) fn lock_owner(file: &File) -> io::Result<()> {
    owner_range(file, true)
}
pub(crate) fn unlock_owner(file: &File) -> io::Result<()> {
    owner_range(file, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ServerOptions;
    #[tokio::test]
    async fn reused_pipe_does_not_inherit_closed_probes_or_previous_eof() {
        use tokio::io::AsyncReadExt;
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "reuse").unwrap();
        let name = wide(&format!("{}.0", listener.path));
        let open = || {
            // SAFETY: local first-instance test pipe, existing handle only.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    CLIENT_RIGHTS,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            assert_ne!(handle, INVALID_HANDLE_VALUE);
            // SAFETY: successful CreateFile transferred a unique handle.
            let file = unsafe { File::from_raw_handle(handle) };
            Stream {
                file,
                timeouts: Mutex::new((Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))),
                gate: Mutex::new(()),
                closed: AtomicBool::new(false),
            }
        };
        // Deliberately close before this current-thread runtime can poll accept.
        drop(open());
        tokio::task::yield_now().await;
        for expected in [b"first", b"again"] {
            let client = open();
            (&client).write_all(expected).unwrap();
            let (mut stream, ()) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = [0; 5];
            tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&bytes, expected);
            drop(client);
            drop(stream);
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn pipe_acl_allows_only_user_and_forbids_competing_instances() {
        let name = format!(r"\\.\pipe\Oink.Unit.IPC.{}.{}", std::process::id(), sid().unwrap());
        let pipe = create(&name).unwrap();
        assert!(create(&name).is_err(), "FIRST_PIPE_INSTANCE must reject a squatter");
        assert!(
            ServerOptions::new().create(&name).is_err(),
            "DACL must reject a competing instance without FIRST_PIPE_INSTANCE too"
        );
        let (mut owner, mut acl, mut sd) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        // SAFETY: valid owned pipe; descriptor allocated by Windows and inspected
        // only while its allocation lives. GetAce validates each index.
        unsafe {
            assert_eq!(
                GetSecurityInfo(
                    pipe.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    ptr::null_mut(),
                    &mut acl,
                    ptr::null_mut(),
                    &mut sd
                ),
                0
            );
            let _allocation = Allocation(sd);
            assert!(!acl.is_null());
            assert_eq!((*acl).AceCount, 1);
            let mut raw = ptr::null_mut();
            assert_ne!(GetAce(acl, 0, &mut raw), 0);
            let ace = &*raw.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Mask, CLIENT_RIGHTS);
            assert_eq!(
                ace.Mask & (FILE_CREATE_PIPE_INSTANCE | GENERIC_WRITE | GENERIC_ALL | WRITE_DAC | WRITE_OWNER),
                0
            );
            let mut expected = ptr::null_mut();
            assert_ne!(ConvertStringSidToSidW(wide(&sid().unwrap()).as_ptr(), &mut expected), 0);
            let _sid = Allocation(expected);
            assert_ne!(EqualSid(ptr::addr_of!(ace.SidStart).cast_mut().cast(), expected), 0);
        }
    }
    #[tokio::test]
    async fn foreign_sid_pipe_is_not_openable_and_server_pid_is_checked() {
        let name = format!(r"\\.\pipe\Oink.Unit.Foreign.{}", std::process::id());
        let pipe = create_for_sid(&name, "S-1-5-21-1-2-3-1001").unwrap();
        let name = wide(&name);
        // SAFETY: terminated local test name and existing object only.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                CLIENT_RIGHTS,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        assert_eq!(handle, INVALID_HANDLE_VALUE);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32)
        );
        drop(pipe);
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "pid-check").unwrap();
        assert_eq!(
            connect(&listener.path, std::process::id() + 1, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        // Every slot was created before publication: a subscription and a UI
        // request can connect concurrently, without same-name instance rights.
        let first = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
        let second = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
        drop((first, second));
    }
}
