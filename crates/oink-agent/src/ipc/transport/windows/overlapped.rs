//! One owned operation per native pipe I/O. No cached EOF crosses a connection
//! lease, unlike reusing a Mio named-pipe stream after DisconnectNamedPipe.
//!
//! Completions arrive on a completion port owned by the listener and are taken
//! by one thread that never leaves it. Do not move this back onto thread-pool
//! waits (`CreateThreadpoolWait` on a per-operation event), which is what
//! 0.3.3–0.3.5 did:
//! - The process default pool drops all its workers after 67 s idle and relies
//!   on the kernel to create one when a wait fires. A wait that is signaled by
//!   an operation completing at once and then torn down microseconds later,
//!   repeated while the pool has no worker, leaves the pool with no worker and
//!   no thread creation for later signaled waits until something posts work to
//!   it from user mode. Windows 11 25H2 (26200.9711), 2026-11-10: the backend's
//!   IPC stopped accepting and reading for hours; reproduced standalone in a
//!   few hundred cycles. No public description of this behaviour was found.
//! - A dedicated thread blocked on our own port does not depend on thread
//!   creation, idle timeouts, or what other code does to a shared pool.
//! - Operations that complete at once queue nothing (skip-on-success), so
//!   there is nothing to arm and cancel on the common path.
use super::*;
use std::{cell::UnsafeCell, fmt, task::Waker};

// FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE
// https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-setfilecompletionnotificationmodes
const NOTIFICATION_MODES: u8 = 1 | 2;

#[derive(Default)]
struct Outstanding {
    closing: bool,
    /// Operations the kernel still owes this port a completion for.
    count: usize,
}
pub(super) struct Port {
    handle: File,
    outstanding: Mutex<Outstanding>,
}
impl fmt::Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeCompletionPort").finish_non_exhaustive()
    }
}
impl Port {
    pub(super) fn new() -> io::Result<Arc<Self>> {
        // SAFETY: creates a new port; one thread ever waits on it.
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let port = Arc::new(Self {
            // SAFETY: unique handle returned by CreateIoCompletionPort.
            handle: unsafe { File::from_raw_handle(handle) },
            outstanding: Mutex::default(),
        });
        let thread = port.clone();
        std::thread::Builder::new()
            .name("oink-ipc-completions".into())
            .spawn(move || thread.run())?;
        Ok(port)
    }
    pub(super) fn attach(&self, pipe: &File) -> io::Result<()> {
        // SAFETY: both handles are live; association lasts for the pipe handle's life.
        unsafe {
            if CreateIoCompletionPort(pipe.as_raw_handle(), self.handle.as_raw_handle(), 0, 0).is_null()
                || SetFileCompletionNotificationModes(pipe.as_raw_handle(), NOTIFICATION_MODES) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
    /// No operation starts after this. The thread leaves once every operation
    /// already issued has handed its completion back.
    pub(super) fn close(&self) {
        self.outstanding.lock().unwrap_or_else(|e| e.into_inner()).closing = true;
        self.nudge();
    }
    pub(super) fn closing(&self) -> bool {
        self.outstanding.lock().unwrap_or_else(|e| e.into_inner()).closing
    }
    fn nudge(&self) {
        // SAFETY: live port; a null OVERLAPPED marks a wake-up, never an operation.
        unsafe {
            PostQueuedCompletionStatus(self.handle.as_raw_handle(), 0, 0, ptr::null());
        }
    }
    fn begin(&self) -> io::Result<()> {
        let mut outstanding = self.outstanding.lock().unwrap_or_else(|e| e.into_inner());
        if outstanding.closing {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "local IPC listener closed"));
        }
        outstanding.count += 1;
        Ok(())
    }
    fn settle(&self) -> bool {
        let mut outstanding = self.outstanding.lock().unwrap_or_else(|e| e.into_inner());
        outstanding.count -= 1;
        outstanding.closing && outstanding.count == 0
    }
    fn run(&self) {
        loop {
            let (mut bytes, mut key, mut overlapped) = (0, 0, ptr::null_mut());
            // SAFETY: live port and valid outputs; blocks until a completion or wake-up.
            let ok = unsafe {
                GetQueuedCompletionStatus(
                    self.handle.as_raw_handle(),
                    &mut bytes,
                    &mut key,
                    &mut overlapped,
                    INFINITE,
                )
            };
            let error = io::Error::last_os_error();
            if overlapped.is_null() {
                if ok == 0 {
                    tracing::error!(%error, os_error = ?error.raw_os_error(), "native IPC completion port failed");
                    return;
                }
                let outstanding = self.outstanding.lock().unwrap_or_else(|e| e.into_inner());
                if outstanding.closing && outstanding.count == 0 {
                    return;
                }
                continue;
            }
            // SAFETY: every OVERLAPPED queued to this port is the first field of
            // a `Shared` whose reference was leaked to the kernel in `submit`.
            let shared = unsafe { Arc::from_raw(overlapped.cast::<Shared>().cast_const()) };
            let result = if ok != 0 {
                Ok(bytes as usize)
            } else {
                shared.error(error)
            };
            shared.complete(result);
            drop(shared);
            if self.settle() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Connect,
    Read,
    Write,
}
#[derive(Default)]
struct Completion {
    result: Option<io::Result<usize>>,
    waker: Option<Waker>,
}
// The port returns the OVERLAPPED address; it must be this allocation's address.
#[repr(C)]
struct Shared {
    overlapped: UnsafeCell<OVERLAPPED>,
    buffer: UnsafeCell<Vec<u8>>,
    // Keeps the handle open until the kernel has handed the completion back.
    pipe: Arc<File>,
    kind: Kind,
    completion: Mutex<Completion>,
}
// SAFETY: the kernel owns `overlapped` and the buffer's bytes from submission
// until the completion is dequeued. Rust reads the buffer only afterwards and
// only through the single `Operation`; everything else is behind the mutex.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}
impl Shared {
    fn error(&self, error: io::Error) -> io::Result<usize> {
        match (self.kind, error.raw_os_error().map(|code| code as u32)) {
            (Kind::Read, Some(ERROR_BROKEN_PIPE)) | (Kind::Connect, Some(ERROR_PIPE_CONNECTED)) => Ok(0),
            _ => Err(error),
        }
    }
    fn finish(&self) -> io::Result<usize> {
        if self.kind == Kind::Connect {
            return Ok(0);
        }
        let mut count = 0;
        // SAFETY: the API reported synchronous completion of this operation.
        if unsafe { GetOverlappedResult(self.pipe.as_raw_handle(), self.overlapped.get(), &mut count, 0) } == 0 {
            self.error(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }
    fn complete(&self, result: io::Result<usize>) {
        let waker = {
            let mut completion = self.completion.lock().unwrap_or_else(|e| e.into_inner());
            completion.result = Some(result);
            completion.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

pub(super) struct Operation {
    shared: Arc<Shared>,
    pending: bool,
    result: Option<io::Result<usize>>,
}
impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeOperation")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}
impl Operation {
    fn submit(port: &Port, pipe: Arc<File>, buffer: Vec<u8>, kind: Kind) -> io::Result<Self> {
        port.begin()?;
        let shared = Arc::new(Shared {
            overlapped: UnsafeCell::new(OVERLAPPED::default()),
            buffer: UnsafeCell::new(buffer),
            pipe,
            kind,
            completion: Mutex::default(),
        });
        // The kernel holds this reference until the port thread takes the
        // completion. It is leaked before the call: the completion can be
        // dequeued before the call returns.
        let kernel = Arc::into_raw(shared.clone());
        let handle = shared.pipe.as_raw_handle();
        let overlapped = shared.overlapped.get();
        // SAFETY: owned handle; OVERLAPPED and buffer stay at these addresses
        // and are untouched by Rust until the completion is handed back.
        let ok = unsafe {
            let buffer = &mut *shared.buffer.get();
            match kind {
                Kind::Connect => ConnectNamedPipe(handle, overlapped),
                Kind::Read => ReadFile(
                    handle,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    ptr::null_mut(),
                    overlapped,
                ),
                Kind::Write => WriteFile(
                    handle,
                    buffer.as_ptr(),
                    buffer.len() as u32,
                    ptr::null_mut(),
                    overlapped,
                ),
            }
        };
        let error = io::Error::last_os_error();
        let result = if ok != 0 {
            Some(shared.finish())
        } else if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            None
        } else {
            Some(shared.error(error))
        };
        if result.is_some() {
            // Finished or refused at once: nothing is queued to the port, so
            // its reference comes straight back.
            // SAFETY: `kernel` came from Arc::into_raw above and no completion
            // will ever reconstruct it.
            drop(unsafe { Arc::from_raw(kernel) });
            if port.settle() {
                port.nudge();
            }
        }
        Ok(Self {
            shared,
            pending: result.is_none(),
            result,
        })
    }
    pub(super) fn connect(port: &Port, pipe: Arc<File>) -> io::Result<Self> {
        Self::submit(port, pipe, Vec::new(), Kind::Connect)
    }
    pub(super) fn read(port: &Port, pipe: Arc<File>, size: usize) -> io::Result<Self> {
        Self::submit(port, pipe, vec![0; size], Kind::Read)
    }
    pub(super) fn write(port: &Port, pipe: Arc<File>, bytes: &[u8]) -> io::Result<Self> {
        Self::submit(port, pipe, bytes.to_vec(), Kind::Write)
    }
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        if let Some(result) = self.result.take() {
            return Poll::Ready(result);
        }
        let mut completion = self.shared.completion.lock().unwrap_or_else(|e| e.into_inner());
        match completion.result.take() {
            Some(result) => {
                self.pending = false;
                Poll::Ready(result)
            }
            None => {
                completion.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
    /// The bytes of a finished read.
    pub(super) fn into_buffer(self) -> Vec<u8> {
        assert!(!self.pending, "pipe operation buffer taken before completion");
        // SAFETY: the operation completed, so the kernel no longer writes here
        // and this is the only reader.
        unsafe { std::mem::take(&mut *self.shared.buffer.get()) }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if self.pending {
            // Request cancellation only. The completion (aborted, or the real
            // one if it won the race) still reaches the port thread, which
            // frees the OVERLAPPED and buffer; nothing blocks a runtime worker.
            // SAFETY: live handle and the OVERLAPPED the kernel still owns.
            unsafe {
                CancelIoEx(self.shared.pipe.as_raw_handle(), self.shared.overlapped.get());
            }
        }
    }
}
