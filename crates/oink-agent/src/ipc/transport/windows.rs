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
use overlapped::{Operation, Port};
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
    port: Arc<Port>,
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
        let port = Port::new()?;
        for pipe in &pipes {
            if let Err(error) = port.attach(pipe) {
                port.close();
                return Err(error);
            }
        }
        let (send, receive) = mpsc::unbounded_channel();
        let tasks = pipes
            .into_iter()
            .map(|mut pipe| {
                let send = send.clone();
                let port = port.clone();
                tokio::spawn(async move {
                    loop {
                        let result = async {
                            let mut operation = Operation::connect(&port, pipe.clone())?;
                            std::future::poll_fn(|cx| operation.poll(cx)).await
                        }.await;
                        if let Err(error) = result {
                            if port.closing() {
                                break;
                            }
                            if matches!(error.raw_os_error(), Some(code) if code == ERROR_NO_DATA as i32 || code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32) {
                                // ConnectNamedPipe documents ERROR_NO_DATA when
                                // a client closed before the server disconnected.
                                // Discard that connection, retaining this instance.
                                // https://learn.microsoft.com/windows/win32/api/namedpipeapi/nf-namedpipeapi-connectnamedpipe
                                // SAFETY: the connect operation finished above;
                                // no other I/O is in flight on this pipe.
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
                                port: port.clone(),
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
            port,
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
        self.port.close();
    }
}
#[derive(Debug)]
pub(crate) struct AsyncStream {
    pipe: Option<Arc<File>>,
    recycle: Option<oneshot::Sender<Arc<File>>>,
    port: Arc<Port>,
    read: Option<Operation>,
    write: Option<Operation>,
    buffered: Vec<u8>,
    offset: usize,
}
impl Drop for AsyncStream {
    fn drop(&mut self) {
        // No I/O may survive into the next connection. Retaining a Mio
        // named-pipe stream across DisconnectNamedPipe carries cached EOF;
        // native operations instead own their completion state for one lease.
        // Dropping cancels; DisconnectNamedPipe then fails whatever is still
        // in flight, so nothing is pending when the slot listens again. Late
        // completions only reach their own, already abandoned operation.
        drop(self.read.take());
        drop(self.write.take());
        if let Some(pipe) = self.pipe.take() {
            // SAFETY: this instance remains owned by its slot.
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
                    &self.port,
                    self.pipe.as_ref().unwrap().clone(),
                    out.remaining().min(65536),
                )?);
            }
            let count = match self.read.as_mut().unwrap().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => result?,
            };
            self.buffered = self.read.take().unwrap().into_buffer();
            self.buffered.truncate(count);
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
                &self.port,
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
/// Whether the client of one accepted connection has closed its end.
#[derive(Debug)]
pub(crate) struct Peer(Arc<File>);
pub(crate) fn peer(stream: &AsyncStream) -> io::Result<Peer> {
    Ok(Peer(stream.pipe.as_ref().unwrap().clone()))
}
impl Peer {
    pub(crate) fn closed(&self) -> bool {
        let mut available = 0;
        // Peeking fails once the client end is closed and its data consumed;
        // while the client is connected it succeeds, with or without data.
        // https://learn.microsoft.com/windows/win32/api/namedpipeapi/nf-namedpipeapi-peeknamedpipe
        // SAFETY: live pipe handle and a valid output; reads no data.
        unsafe {
            PeekNamedPipe(
                self.0.as_raw_handle(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                ptr::null_mut(),
            ) == 0
        }
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_clients_recycle_pipe_slots_after_repeated_roundtrips() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "recycle-roundtrip").unwrap();
        let held = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
        let (held_server, _) = listener.accept().await.unwrap();
        for _ in 0..SLOTS * 4 {
            let client = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
            client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let client = tokio::task::spawn_blocking(move || {
                let mut client = &client;
                client.write_all(b"request").unwrap();
                let mut response = [0; 8];
                client.read_exact(&mut response).unwrap();
                assert_eq!(&response, b"response");
            });
            let (mut server, _) = listener.accept().await.unwrap();
            let mut request = [0; 7];
            server.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"request");
            server.write_all(b"response").await.unwrap();
            client.await.unwrap();
            let mut rest = Vec::new();
            server.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
            drop(server);
        }
        drop((held, held_server));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_idle_clients_and_cancelled_reads_recycle_pipe_slots() {
        use tokio::io::AsyncReadExt;
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "recycle-idle").unwrap();
        for round in 0..SLOTS * 4 {
            let client = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
            let (mut server, _) = listener.accept().await.unwrap();
            let read = tokio::spawn(async move {
                let mut byte = [0];
                server.read(&mut byte).await
            });
            tokio::task::yield_now().await;
            if round % 2 == 0 {
                drop(client);
                assert_eq!(read.await.unwrap().unwrap(), 0);
            } else {
                read.abort();
                assert!(read.await.unwrap_err().is_cancelled());
                drop(client);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_closed_clients_recycle_every_pipe_slot() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "recycle-concurrent").unwrap();
        let held = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
        let (held_server, _) = listener.accept().await.unwrap();
        for _ in 0..128 {
            let mut clients = tokio::task::JoinSet::new();
            let mut servers = tokio::task::JoinSet::new();
            for _ in 1..SLOTS {
                let client = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
                client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                clients.spawn_blocking(move || {
                    let mut client = &client;
                    client.write_all(b"x").unwrap();
                    let mut response = [0];
                    client.read_exact(&mut response).unwrap();
                    assert_eq!(response, [b'y']);
                });
                let (mut server, _) = listener.accept().await.unwrap();
                servers.spawn(async move {
                    let mut request = [0];
                    server.read_exact(&mut request).await.unwrap();
                    server.write_all(b"y").await.unwrap();
                    assert_eq!(server.read(&mut request).await.unwrap(), 0);
                });
            }
            while let Some(result) = clients.join_next().await {
                result.unwrap();
            }
            while let Some(result) = servers.join_next().await {
                result.unwrap();
            }
            tokio::task::yield_now().await;
        }
        drop((held, held_server));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_close_wakes_confirmed_pending_reads_and_full_pipe_writes() {
        use std::future::{Future, poll_fn};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "recycle-pending").unwrap();
        for round in 0..128 {
            let client = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
            let (server, _) = listener.accept().await.unwrap();
            let (pending, confirmed) = oneshot::channel();
            let task = tokio::spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(server);
                let mut pending = Some(pending);
                if round % 2 == 0 {
                    let mut byte = [0];
                    let mut read = std::pin::pin!(reader.read(&mut byte));
                    let result = poll_fn(|cx| {
                        let result = read.as_mut().poll(cx);
                        if result.is_pending()
                            && let Some(pending) = pending.take()
                        {
                            pending.send(()).unwrap();
                        }
                        result
                    })
                    .await;
                    assert_eq!(result.unwrap(), 0);
                } else {
                    let bytes = vec![b'x'; 1024 * 1024];
                    let mut write = std::pin::pin!(writer.write_all(&bytes));
                    let result = poll_fn(|cx| {
                        let result = write.as_mut().poll(cx);
                        if result.is_pending()
                            && let Some(pending) = pending.take()
                        {
                            pending.send(()).unwrap();
                        }
                        result
                    })
                    .await;
                    assert!(result.is_err());
                }
            });
            confirmed.await.unwrap();
            drop(client);
            task.await.unwrap();
        }
    }

    #[test]
    fn quiescent_multithread_runtime_wakes_native_pipe_tasks() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(16)
            .enable_all()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let listener = {
            let _entered = runtime.enter();
            Listener::bind(directory.path(), "recycle-quiescent").unwrap()
        };
        // The host binds before Node startup finishes and starts accepting later.
        // Let every runtime worker park without an active block_on driver first.
        std::thread::sleep(Duration::from_millis(200));
        let path = listener.path.clone();
        let server = runtime.spawn(async move {
            for _ in 0..8 {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = tokio::io::split(stream);
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line, "request\n");
                writer.write_all(b"response\n").await.unwrap();
                line.clear();
                assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
            }
        });
        let client = std::thread::spawn(move || {
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(100));
                let client = connect(&path, std::process::id(), Duration::from_secs(1)).unwrap();
                client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                // Connect completion and read completion must independently wake
                // a parked worker; the main block_on only waits for server exit.
                std::thread::sleep(Duration::from_millis(100));
                (&client).write_all(b"request\n").unwrap();
                let mut response = [0; 9];
                (&client).read_exact(&mut response).unwrap();
                assert_eq!(&response, b"response\n");
                std::thread::sleep(Duration::from_millis(100));
                drop(client);
            }
        });
        runtime.block_on(server).unwrap();
        client.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    async fn full_duplex_subscription_does_not_stall_other_pipe_slots() {
        use std::io::BufRead;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "recycle-duplex").unwrap();
        let client = Arc::new(connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap());
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicUsize::new(0));
        let controls = Arc::new(AtomicUsize::new(0));
        let reader = {
            let client = client.clone();
            let stopping = stopping.clone();
            let received = received.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(&*client);
                let mut line = Vec::new();
                while !stopping.load(Ordering::Acquire) {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            received.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) if stopping.load(Ordering::Acquire) => break,
                        Err(error) => panic!("event reader: {error}"),
                    }
                }
            })
        };
        let writer = {
            let client = client.clone();
            let stopping = stopping.clone();
            std::thread::spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    (&*client).write_all(b"{\"live_views\":true}\n").unwrap();
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let subscription = {
            let controls = controls.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(stream);
                let mut lines = tokio::io::BufReader::new(reader).lines();
                let mut events = tokio::time::interval(Duration::from_millis(2));
                let mut payload = vec![b'x'; 8191];
                payload.push(b'\n');
                loop {
                    tokio::select! {
                        _ = events.tick() => {
                            if writer.write_all(&payload).await.is_err() { break; }
                        }
                        line = lines.next_line() => {
                            match line {
                                Ok(Some(_)) => { controls.fetch_add(1, Ordering::Relaxed); }
                                _ => break,
                            }
                        }
                    }
                }
            })
        };
        for _ in 0..8 {
            let mut clients = tokio::task::JoinSet::new();
            let mut servers = tokio::task::JoinSet::new();
            for _ in 1..SLOTS {
                let client = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
                client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                clients.spawn_blocking(move || {
                    (&client).write_all(b"x").unwrap();
                    let mut response = [0];
                    (&client).read_exact(&mut response).unwrap();
                });
                let (mut server, _) = listener.accept().await.unwrap();
                servers.spawn(async move {
                    let mut request = [0];
                    server.read_exact(&mut request).await.unwrap();
                    server.write_all(b"y").await.unwrap();
                    assert_eq!(server.read(&mut request).await.unwrap(), 0);
                });
            }
            while let Some(result) = clients.join_next().await {
                result.unwrap();
            }
            while let Some(result) = servers.join_next().await {
                result.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        stopping.store(true, Ordering::Release);
        writer.join().unwrap();
        client.shutdown(Shutdown::Both).unwrap();
        reader.join().unwrap();
        drop(client);
        subscription.await.unwrap();
        assert!(received.load(Ordering::Relaxed) > 100);
        assert!(controls.load(Ordering::Relaxed) > 100);
    }

    // Thread-pool worker factories of this process, read through ntdll. Only
    // the regression test below needs them.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryObject(handle: HANDLE, class: u32, buffer: *mut c_void, length: u32, returned: *mut u32) -> i32;
        fn NtQueryInformationWorkerFactory(
            handle: HANDLE,
            class: u32,
            buffer: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
        fn NtSetInformationWorkerFactory(handle: HANDLE, class: u32, buffer: *const c_void, length: u32) -> i32;
    }
    /// (handle, idle timeout in 100 ns units, worker threads) per factory.
    fn worker_factories() -> Vec<(usize, i64, u32)> {
        let mut name = [0u64; 512];
        let mut info = [0u8; 128];
        let mut found = Vec::new();
        for raw in (4usize..0x2000).step_by(4) {
            let mut returned = 0;
            // SAFETY: probing handle values of this process with valid buffers;
            // an invalid value only makes the query fail.
            unsafe {
                if NtQueryObject(raw as HANDLE, 2, name.as_mut_ptr().cast(), 4096, &mut returned) != 0 {
                    continue;
                }
                let text = std::slice::from_raw_parts(name[1] as *const u16, (name[0] & 0xffff) as usize / 2);
                if String::from_utf16_lossy(text) != "TpWorkerFactory"
                    || NtQueryInformationWorkerFactory(raw as HANDLE, 7, info.as_mut_ptr().cast(), 120, &mut returned)
                        != 0
                {
                    continue;
                }
            }
            found.push((
                raw,
                i64::from_le_bytes(info[16..24].try_into().unwrap()),
                u32::from_le_bytes(info[52..56].try_into().unwrap()),
            ));
        }
        found
    }

    /// Regression for the 2026-11-10 stall (see overlapped.rs). With thread-pool
    /// waits, every write below that completed at once signaled a wait and
    /// cancelled it; doing that while the default pool had no worker ended, in
    /// a few hundred rounds, with a pool that no longer ran wait callbacks, so
    /// the final connection was never accepted. That implementation spends
    /// minutes here before failing; this one never touches the pool.
    ///
    /// Runs in a process of its own: it shortens the idle timeout of every
    /// thread pool in the process for minutes, which stalled an unrelated
    /// clipboard test sharing the full-CI test process.
    #[test]
    fn writes_completing_at_once_on_an_idle_thread_pool_do_not_stop_later_accepts() {
        const CHILD: &str = "OINK_IDLE_POOL_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .unwrap()
                .block_on(idle_pool_writes_then_accept());
            return;
        }
        let name = format!(
            "{}::writes_completing_at_once_on_an_idle_thread_pool_do_not_stop_later_accepts",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads", "1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    async fn idle_pool_writes_then_accept() {
        use std::io::BufRead;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const DEFAULT_IDLE: i64 = -670_000_000;
        const SHORT_IDLE: i64 = -10_000_000;
        let directory = tempfile::tempdir().unwrap();
        let listener = Listener::bind(directory.path(), "idle-pool").unwrap();
        let client = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
        // Each write waits for idle pools to drop their workers (over a second,
        // longer under load); the subscriber must outlast those gaps.
        client.set_read_timeout(None).unwrap();
        let (mut subscription, ()) = listener.accept().await.unwrap();
        let received = Arc::new(AtomicUsize::new(0));
        let reader = {
            let received = received.clone();
            std::thread::spawn(move || {
                let mut line = String::new();
                let mut reader = std::io::BufReader::new(&client);
                while reader.read_line(&mut line).is_ok_and(|read| read != 0) {
                    received.fetch_add(1, Ordering::Relaxed);
                    line.clear();
                }
            })
        };
        let mut shortened = std::collections::HashSet::new();
        for _ in 0..600 {
            // Let every default-timeout pool lose its workers (1 s instead of 67 s).
            for _ in 0..300 {
                let mut busy = false;
                for (handle, idle, workers) in worker_factories() {
                    if idle == DEFAULT_IDLE {
                        let short = SHORT_IDLE;
                        // SAFETY: our own factory handle and an 8-byte timeout.
                        unsafe {
                            NtSetInformationWorkerFactory(handle as HANDLE, 2, (&raw const short).cast(), 8);
                        }
                        shortened.insert(handle);
                    }
                    busy |= shortened.contains(&handle) && workers != 0;
                }
                if !busy {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            subscription.write_all(b"event\n").await.unwrap();
        }
        let late = connect(&listener.path, std::process::id(), Duration::from_secs(1)).unwrap();
        (&late).write_all(b"request").unwrap();
        let mut request = [0; 7];
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut stream, ()) = listener.accept().await.unwrap();
            stream.read_exact(&mut request).await.unwrap();
        })
        .await
        .expect("a connection made after the idle writes must still be accepted and read");
        assert_eq!(&request, b"request");
        // Disconnecting discards what the client has not read yet.
        tokio::time::timeout(Duration::from_secs(5), async {
            while received.load(Ordering::Relaxed) < 600 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("every event written at once reaches the client");
        drop(subscription);
        reader.join().unwrap();
    }
}
