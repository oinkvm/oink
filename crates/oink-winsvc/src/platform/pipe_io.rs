//! One outstanding operation on a message pipe. PIPE_WAIT plus overlapped I/O is
//! intentional: PIPE_NOWAIT is polling mode, not asynchronous completion notification.
use super::*;
use windows_sys::Win32::{
    Storage::FileSystem::*,
    System::{IO::*, Pipes::*},
};

pub(super) struct Core {
    pipe: HANDLE,
    event: Handle,
    overlapped: Box<OVERLAPPED>,
    pending: bool,
}
impl Core {
    fn new(pipe: HANDLE) -> io::Result<Self> {
        // SAFETY: unnamed manual-reset event, exclusively owned by this operation.
        let event = Handle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
        let overlapped = Box::new(OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        });
        Ok(Self {
            pipe,
            event,
            overlapped,
            pending: false,
        })
    }
    pub(super) fn event(&self) -> HANDLE {
        self.event.0
    }
    pub(super) fn reset_idle_event(&mut self) {
        assert!(!self.pending);
        // SAFETY: no operation is outstanding; reset only the owned event.
        unsafe {
            ResetEvent(self.event.0);
        }
    }
    fn prepare(&mut self) {
        assert!(!self.pending);
        // SAFETY: owned event and no outstanding operation.
        unsafe {
            ResetEvent(self.event.0);
        }
        *self.overlapped = OVERLAPPED {
            hEvent: self.event.0,
            ..Default::default()
        };
    }
    fn started(&mut self, ok: i32) -> io::Result<()> {
        if ok != 0 {
            // Synchronous completion does not require GetOverlappedResult. Signal the
            // same event so the helper has one wait path for both completion modes.
            // SAFETY: owned event.
            unsafe {
                SetEvent(self.event.0);
            }
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            self.pending = true;
            Ok(())
        } else {
            Err(error)
        }
    }
    pub(super) fn connect(&mut self) -> io::Result<()> {
        self.prepare();
        // SAFETY: pinned OVERLAPPED lives until completion or cancellation.
        let ok = unsafe { ConnectNamedPipe(self.pipe, &mut *self.overlapped) };
        if ok == 0 && io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
            // A client may connect between CreateNamedPipe and ConnectNamedPipe.
            // SAFETY: this operation's owned event.
            unsafe {
                SetEvent(self.event.0);
            }
            Ok(())
        } else {
            self.started(ok)
        }
    }
    /// Buffer must remain live and untouched until finish/cancel (also on error paths).
    pub(super) unsafe fn read(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        self.prepare();
        // SAFETY: caller keeps the buffer live; OVERLAPPED is pinned in its Box.
        let ok = unsafe {
            ReadFile(
                self.pipe,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                ptr::null_mut(),
                &mut *self.overlapped,
            )
        };
        self.started(ok)
    }
    /// Buffer must remain live and untouched until finish/cancel (also on error paths).
    pub(super) unsafe fn write(&mut self, buffer: &[u8]) -> io::Result<()> {
        self.prepare();
        // SAFETY: caller keeps the buffer live; OVERLAPPED is pinned in its Box.
        let ok = unsafe {
            WriteFile(
                self.pipe,
                buffer.as_ptr(),
                buffer.len() as u32,
                ptr::null_mut(),
                &mut *self.overlapped,
            )
        };
        self.started(ok)
    }
    pub(super) fn finish(&mut self) -> io::Result<u32> {
        let mut count = 0;
        // SAFETY: the operation's completion event has signalled.
        let ok = unsafe { GetOverlappedResult(self.pipe, &*self.overlapped, &mut count, 0) };
        self.pending = false;
        check(ok)?;
        Ok(count)
    }
    pub(super) fn wait(&mut self, timeout: u32) -> io::Result<u32> {
        // SAFETY: event remains valid for the wait.
        match unsafe { WaitForSingleObject(self.event.0, timeout) } {
            WAIT_OBJECT_0 => self.finish(),
            WAIT_TIMEOUT => {
                self.cancel();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "input pipe operation timeout; request not replayed",
                ))
            }
            _ => {
                self.cancel();
                Err(io::Error::last_os_error())
            }
        }
    }
    pub(super) fn cancel(&mut self) {
        if self.pending {
            // CancelIoEx does not wait. Drain its completion before releasing either
            // OVERLAPPED or its buffer; otherwise the kernel can write freed memory.
            // SAFETY: live handle and pinned outstanding operation.
            unsafe {
                CancelIoEx(self.pipe, &*self.overlapped);
                let mut count = 0;
                GetOverlappedResult(self.pipe, &*self.overlapped, &mut count, 1);
            }
            self.pending = false;
        }
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        self.cancel();
    }
}

// A borrowed operation cannot outlive its handle. The owned form pins one
// shared pipe handle for the client owner's persistent asynchronous read.
pub(super) struct Operation<'a> {
    core: Core,
    _pipe: &'a Handle,
}
impl<'a> Operation<'a> {
    pub(super) fn new(pipe: &'a Handle) -> io::Result<Self> {
        Ok(Self {
            core: Core::new(pipe.0)?,
            _pipe: pipe,
        })
    }
}
impl std::ops::Deref for Operation<'_> {
    type Target = Core;
    fn deref(&self) -> &Core {
        &self.core
    }
}
impl std::ops::DerefMut for Operation<'_> {
    fn deref_mut(&mut self) -> &mut Core {
        &mut self.core
    }
}
// Only pipe/event handles enter this shared storage. Desktop attachment and
// OVERLAPPED/buffer state remain thread-owned and are never shared by it.
struct KernelHandle(Handle);
// SAFETY: Windows pipe/event handles have no thread affinity; kernel operations
// synchronize access. This wrapper contains neither an HDESK nor I/O buffers.
unsafe impl Send for KernelHandle {}
// SAFETY: the same immutable kernel-object access; no desktop/buffer state.
unsafe impl Sync for KernelHandle {}
#[derive(Clone)]
pub(super) struct SharedPipe(std::sync::Arc<KernelHandle>);
// SAFETY: a kernel pipe handle is transferable/shareable; all I/O operations
// remain exclusively owned, including their OVERLAPPED and cancellation buffers.
impl SharedPipe {
    pub(super) fn new(handle: Handle) -> Self {
        Self(std::sync::Arc::new(KernelHandle(handle)))
    }
    pub(super) fn handle(&self) -> &Handle {
        &self.0.0
    }
}
pub(super) struct OwnedOperation {
    // Drop/cancel the kernel operation BEFORE releasing the pipe handle.
    core: Core,
    _pipe: SharedPipe,
}
// SAFETY: moved only with pinned Box<OVERLAPPED> and buffers; no concurrent access.
unsafe impl Send for OwnedOperation {}
impl OwnedOperation {
    pub(super) fn new(pipe: SharedPipe) -> io::Result<Self> {
        Ok(Self {
            core: Core::new(pipe.handle().0)?,
            _pipe: pipe,
        })
    }
}
impl std::ops::Deref for OwnedOperation {
    type Target = Core;
    fn deref(&self) -> &Core {
        &self.core
    }
}
impl std::ops::DerefMut for OwnedOperation {
    fn deref_mut(&mut self) -> &mut Core {
        &mut self.core
    }
}
#[derive(Clone)]
pub(super) struct Signal(std::sync::Arc<KernelHandle>);
// SAFETY: an event is a kernel synchronization object; only signal/wait/reset
// use it across threads, never OVERLAPPED buffers or an input/desktop handle.
impl Signal {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: unnamed auto-reset event, no inherited handles.
        Ok(Self(std::sync::Arc::new(KernelHandle(Handle::new(unsafe {
            CreateEventW(ptr::null(), 0, 0, ptr::null())
        })?))))
    }
    pub(super) fn handle(&self) -> HANDLE {
        self.0.0.0
    }
    pub(super) fn notify(&self) {
        // SAFETY: shared event lives through this call; no ordinary-thread mutex.
        unsafe {
            SetEvent(self.0.0.0);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);

    pub(in crate::platform) fn pair() -> (Handle, Handle) {
        // Private test pipe: no desktop, broker, authenticated service, or injection.
        let name = wide(&format!(
            r"\\.\pipe\Oink.Unit.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // SAFETY: local unnamed-security test pipe and valid terminated name.
        let server = Handle::new(unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                ptr::null(),
            )
        })
        .unwrap();
        // SAFETY: matching local test pipe, owned non-inherited handle.
        let client = Handle::new(unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_DATA | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        })
        .unwrap();
        let mut connect = Operation::new(&server).unwrap();
        connect.connect().unwrap();
        // SAFETY: live completion event. Already-connected pipes signal immediately.
        assert_eq!(unsafe { WaitForSingleObject(connect.event(), 1000) }, WAIT_OBJECT_0);
        connect.cancel();
        drop(connect);
        (server, client)
    }

    #[test]
    fn command_arrival_signals_completion_without_polling() {
        let (server, client) = pair();
        let mut received = [0u8; 24];
        let command = [37u8; 24];
        let mut read = Operation::new(&server).unwrap();
        let mut write = Operation::new(&client).unwrap();
        // SAFETY: both buffers outlive the operations, including cancellation.
        unsafe {
            read.read(&mut received).unwrap();
        }
        // SAFETY: an empty pipe leaves the overlapped read pending.
        assert_eq!(unsafe { WaitForSingleObject(read.event(), 0) }, WAIT_TIMEOUT);
        // SAFETY: command remains live through write completion.
        unsafe {
            write.write(&command).unwrap();
        }
        assert_eq!(write.wait(1000).unwrap(), 24);
        // This waits on the kernel completion itself, with no polling/maintenance tick.
        assert_eq!(read.wait(1000).unwrap(), 24);
        assert_eq!(received, command);
    }

    #[test]
    fn timeout_drains_cancellation_before_reusing_buffer() {
        let (server, client) = pair();
        let mut received = [0u8; 24];
        let command = [49u8; 24];
        let mut read = Operation::new(&server).unwrap();
        // SAFETY: received remains live until cancellation is drained by wait.
        unsafe {
            read.read(&mut received).unwrap();
        }
        assert_eq!(read.wait(10).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(received, [0; 24]);
        // SAFETY: cancelled operation is complete; buffer may be reused now.
        unsafe {
            read.read(&mut received).unwrap();
        }
        let mut write = Operation::new(&client).unwrap();
        // SAFETY: command lives through write completion.
        unsafe {
            write.write(&command).unwrap();
        }
        assert_eq!(write.wait(1000).unwrap(), 24);
        assert_eq!(read.wait(1000).unwrap(), 24);
        assert_eq!(received, command);
    }
}
