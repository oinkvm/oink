//! One owned operation per native pipe I/O. No cached EOF crosses a connection
//! lease, unlike reusing a Mio named-pipe stream after DisconnectNamedPipe.
use super::*;
use std::{fmt, task::Waker};

#[derive(Default)]
struct Signal {
    ready: AtomicBool,
    waker: Mutex<Option<Waker>>,
}
struct Wait {
    raw: PTP_WAIT,
    signal: Box<Signal>,
}
impl Wait {
    fn new(event: HANDLE) -> io::Result<Self> {
        let mut signal = Box::<Signal>::default();
        // SAFETY: boxed context stays at this address until callbacks have been
        // disarmed and joined in Drop. The callback only signals a Tokio waker.
        let raw = unsafe { CreateThreadpoolWait(Some(wake), ptr::from_mut(&mut *signal).cast(), ptr::null()) };
        if raw == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: event outlives this registration; null timeout means no polling.
        unsafe {
            SetThreadpoolWait(raw, event, ptr::null());
        }
        Ok(Self { raw, signal })
    }
    fn ready(&self, cx: &Context<'_>) -> bool {
        *self.signal.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        self.signal.ready.load(Ordering::Acquire)
    }
}
unsafe extern "system" fn wake(_: PTP_CALLBACK_INSTANCE, context: *mut c_void, _: PTP_WAIT, _: u32) {
    // SAFETY: Wait::drop joins callbacks before freeing this boxed context.
    let signal = unsafe { &*context.cast::<Signal>() };
    signal.ready.store(true, Ordering::Release);
    let waker = signal.waker.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(waker) = waker {
        waker.wake();
    }
}
impl Drop for Wait {
    fn drop(&mut self) {
        // SAFETY: no mutex is held while joining this tiny callback. Disarm and
        // drain before freeing either context or event; Close alone is not a join.
        // https://learn.microsoft.com/windows/win32/api/threadpoolapiset/nf-threadpoolapiset-waitforthreadpoolwaitcallbacks
        unsafe {
            SetThreadpoolWait(self.raw, ptr::null_mut(), ptr::null());
            WaitForThreadpoolWaitCallbacks(self.raw, 1);
            CloseThreadpoolWait(self.raw);
        }
    }
}

pub(super) struct Operation {
    pipe: Arc<File>,
    overlapped: Box<OVERLAPPED>,
    // The wait must be dropped before its event. Its context is separately boxed.
    wait: Wait,
    _event: File,
    pub(super) buffer: Vec<u8>,
    pending: bool,
    result: Option<io::Result<usize>>,
    read: bool,
}
// SAFETY: OVERLAPPED and buffer allocations stay pinned when this owner moves;
// only the callback's atomics/mutex cross threads. Poll/cancel require &mut self.
unsafe impl Send for Operation {}
impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeOperation")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}
impl Operation {
    fn new(pipe: Arc<File>, buffer: Vec<u8>, read: bool) -> io::Result<Self> {
        // SAFETY: private manual-reset event, no inherited handle.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: unique handle returned by CreateEventW.
        let event = unsafe { File::from_raw_handle(event) };
        let wait = Wait::new(event.as_raw_handle())?;
        let overlapped = Box::new(OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        });
        Ok(Self {
            pipe,
            overlapped,
            wait,
            _event: event,
            buffer,
            pending: false,
            result: None,
            read,
        })
    }
    pub(super) fn connect(pipe: Arc<File>) -> io::Result<Self> {
        let mut op = Self::new(pipe, Vec::new(), false)?;
        // SAFETY: owned handle, pinned OVERLAPPED drained on every drop path.
        let ok = unsafe { ConnectNamedPipe(op.pipe.as_raw_handle(), &mut *op.overlapped) };
        let error = io::Error::last_os_error();
        if ok != 0 || error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
            op.result = Some(Ok(0));
        } else if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            op.pending = true;
        } else {
            op.result = Some(Err(error));
        }
        Ok(op)
    }
    pub(super) fn read(pipe: Arc<File>, size: usize) -> io::Result<Self> {
        Self::transfer(pipe, vec![0; size], true)
    }
    pub(super) fn write(pipe: Arc<File>, bytes: &[u8]) -> io::Result<Self> {
        Self::transfer(pipe, bytes.to_vec(), false)
    }
    fn transfer(pipe: Arc<File>, buffer: Vec<u8>, read: bool) -> io::Result<Self> {
        let mut op = Self::new(pipe, buffer, read)?;
        // SAFETY: this operation owns both buffer and pinned OVERLAPPED until
        // completion/cancellation; sizes are bounded by the AsyncStream adapter.
        let ok = unsafe {
            if read {
                ReadFile(
                    op.pipe.as_raw_handle(),
                    op.buffer.as_mut_ptr(),
                    op.buffer.len() as u32,
                    ptr::null_mut(),
                    &mut *op.overlapped,
                )
            } else {
                WriteFile(
                    op.pipe.as_raw_handle(),
                    op.buffer.as_ptr(),
                    op.buffer.len() as u32,
                    ptr::null_mut(),
                    &mut *op.overlapped,
                )
            }
        };
        if ok != 0 {
            op.result = Some(op.finish());
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                op.pending = true;
            } else {
                op.result = Some(op.error(error));
            }
        }
        Ok(op)
    }
    fn error(&self, error: io::Error) -> io::Result<usize> {
        if self.read && error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            Ok(0)
        } else {
            Err(error)
        }
    }
    fn finish(&self) -> io::Result<usize> {
        let mut count = 0;
        // SAFETY: either the API completed synchronously or the event signaled.
        if unsafe { GetOverlappedResult(self.pipe.as_raw_handle(), &*self.overlapped, &mut count, 0) } == 0 {
            self.error(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        if let Some(result) = self.result.take() {
            return Poll::Ready(result);
        }
        if !self.wait.ready(cx) {
            return Poll::Pending;
        }
        self.pending = false;
        Poll::Ready(self.finish())
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if self.pending {
            // CancelIoEx alone is not completion. Drain before releasing the
            // buffer, OVERLAPPED, pipe or the callback's wait event.
            // SAFETY: live pinned operation and owned buffer/pipe.
            unsafe {
                CancelIoEx(self.pipe.as_raw_handle(), &*self.overlapped);
                let mut count = 0;
                GetOverlappedResult(self.pipe.as_raw_handle(), &*self.overlapped, &mut count, 1);
            }
        }
    }
}
