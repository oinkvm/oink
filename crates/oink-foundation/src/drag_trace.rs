//! Bounded, numeric-only diagnostics for native drag reads. No wire changes.
use std::{
    cell::{Cell, RefCell},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Recorder accepts only these numeric fields from the exact tracing target.
pub const FIELDS: &[&str] = &[
    "dr_schema",
    "dr_stage",
    "dr_edge",
    "dr_id",
    "dr_parent",
    "dr_request0",
    "dr_request1",
    "dr_request2",
    "dr_request3",
    "dr_object",
    "dr_offer0",
    "dr_offer1",
    "dr_rep",
    "dr_format",
    "dr_aspect",
    "dr_tymed",
    "dr_index",
    "dr_thread",
    "dr_apartment",
    "dr_main",
    "dr_result",
    "dr_at_us",
    "dr_elapsed_us",
    "dr_aux",
];
pub mod stage {
    pub const GET_DATA: u16 = 1;
    pub const QUERY_DATA: u16 = 2;
    pub const ENUM_FORMATS: u16 = 3;
    pub const GET_DATA_HERE: u16 = 4;
    pub const OTHER_COM: u16 = 5;
    pub const CLIPBOARD_QUERY: u16 = 6;
    pub const NATIVE_OPEN: u16 = 7;
    pub const NATIVE_READ: u16 = 8;
    pub const REMOTE_OPEN: u16 = 10;
    pub const SLOT_WAIT: u16 = 11;
    pub const FETCH: u16 = 12;
    pub const LOW_OPEN: u16 = 13;
    pub const REQUEST_SEND: u16 = 14;
    pub const RESPONSE_WAIT: u16 = 15;
    pub const PARALLEL_WAIT: u16 = 18;
    pub const CACHE_WRITE: u16 = 19;
    pub const SERVE: u16 = 20;
    pub const AUTHORIZE: u16 = 21;
    pub const SOURCE_OPEN: u16 = 22;
    pub const SOURCE_READ: u16 = 23;
    pub const REPLY_SEND: u16 = 24;
    pub const MAIN_QUEUE: u16 = 25;
    pub const PASTEBOARD_READ: u16 = 26;
    pub const DEFERRED_WAIT: u16 = 27;
    pub const INPUT_QUEUE: u16 = 28;
    pub const LOW_SEND: u16 = 30;
    pub const LOW_RECV: u16 = 31;
    pub const LOW_DISPATCH: u16 = 32;
    pub const OLE_SESSION: u16 = 40;
    pub const CONTINUE_DRAG: u16 = 41;
    pub const FEEDBACK: u16 = 42;
    pub const TARGET_ENTER: u16 = 43;
    pub const TARGET_LEAVE: u16 = 44;
    pub const CATCHER_ENTER: u16 = 45;
    pub const CATCHER_OVER: u16 = 46;
    pub const CATCHER_DROP: u16 = 47;
    pub const INJECT_BUTTON: u16 = 48;
}

/// Local call IDs join synchronous callbacks; request words join both peers.
/// The four words preserve every bit through the recorder's f64 scalar storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub call: u64,
    pub request: [u32; 4],
    pub object: u64,
    pub offer: u64,
    pub representation: u32,
    pub format: u16,
    pub aspect: u32,
    pub tymed: u32,
    pub index: i32,
    pub aux: u32,
}
thread_local! { static CURRENT: RefCell<Context> = RefCell::new(Context::default()); }
impl Context {
    pub fn current() -> Self {
        CURRENT.with(|c| *c.borrow())
    }
    pub fn with_request(mut self, bytes: [u8; 16]) -> Self {
        self.request = std::array::from_fn(|i| u32::from_be_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()));
        self
    }
    /// Only synchronous work: never carry a thread-local scope across await.
    pub fn scope<T>(self, work: impl FnOnce() -> T) -> T {
        struct Restore(Context);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|c| *c.borrow_mut() = self.0);
            }
        }
        let previous = CURRENT.with(|c| c.replace(self));
        let _restore = Restore(previous);
        work()
    }
}

const LIMIT: u32 = 32_768;
#[derive(Default)]
struct Budget(Mutex<(u64, u32)>);
impl Budget {
    fn admit(&self, minute: u64) -> (bool, bool) {
        let mut value = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if value.0 != minute {
            *value = (minute, 0);
        }
        value.1 = value.1.saturating_add(1);
        (value.1 <= LIMIT, value.1 == LIMIT + 1)
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Every admitted entry has an exit, including cancellation/unwind (MIN result).
/// A process admits at most 32768 spans/minute; one explicit edge=3 marks loss.
#[derive(Debug)]
pub struct Trace {
    context: Context,
    parent: u64,
    stage: u16,
    began: Instant,
    result: Cell<i32>,
    enabled: bool,
}
impl Trace {
    pub fn new(stage: u16, mut context: Context) -> Self {
        static BUDGET: OnceLock<Budget> = OnceLock::new();
        let (enabled, truncated) = BUDGET.get_or_init(Budget::default).admit(utc_us() / 60_000_000);
        let parent = context.call;
        context.call = NEXT.fetch_add(1, Ordering::Relaxed);
        let trace = Self {
            context,
            parent,
            stage,
            began: Instant::now(),
            result: Cell::new(i32::MIN),
            enabled,
        };
        if enabled {
            trace.emit(0);
        }
        if truncated {
            trace.emit(3);
        }
        trace
    }
    pub fn context(&self) -> Context {
        self.context
    }
    pub fn result(&self, code: i32) {
        self.result.set(code);
    }
    pub fn io<T>(&self, result: &std::io::Result<T>) {
        self.result(result.as_ref().err().map_or(0, |e| {
            e.raw_os_error().unwrap_or_else(|| match e.kind() {
                std::io::ErrorKind::NotConnected => -10,
                std::io::ErrorKind::BrokenPipe => -11,
                std::io::ErrorKind::ConnectionReset => -12,
                std::io::ErrorKind::ConnectionAborted => -13,
                std::io::ErrorKind::UnexpectedEof => -14,
                std::io::ErrorKind::TimedOut => -15,
                std::io::ErrorKind::WouldBlock => -16,
                std::io::ErrorKind::Interrupted => -17,
                std::io::ErrorKind::Unsupported => -18,
                std::io::ErrorKind::NotFound => -19,
                std::io::ErrorKind::PermissionDenied => -20,
                std::io::ErrorKind::InvalidData => -21,
                _ => -1,
            })
        }));
    }
    pub fn scope<T>(&self, work: impl FnOnce() -> T) -> T {
        self.context.scope(work)
    }
    fn emit(&self, edge: u8) {
        let (thread, main, apartment) = thread_info();
        let c = self.context;
        tracing::debug!(target: "oink_dnd_read",
            dr_schema=1_u8, dr_stage=self.stage, dr_edge=edge,
            dr_id=c.call, dr_parent=self.parent,
            dr_request0=c.request[0], dr_request1=c.request[1], dr_request2=c.request[2], dr_request3=c.request[3],
            dr_object=c.object, dr_offer0=c.offer as u32, dr_offer1=(c.offer>>32) as u32,
            dr_rep=c.representation, dr_format=c.format, dr_aspect=c.aspect, dr_tymed=c.tymed, dr_index=c.index,
            dr_thread=thread, dr_apartment=apartment, dr_main=main,
            dr_result=self.result.get(), dr_at_us=utc_us(), dr_elapsed_us=self.began.elapsed().as_micros() as u64,
            dr_aux=c.aux, "native drag read boundary");
    }
}
impl Drop for Trace {
    fn drop(&mut self) {
        if self.enabled {
            self.emit(1);
        }
    }
}
fn utc_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
fn thread_info() -> (u64, u8, i32) {
    #[cfg(windows)]
    {
        let mut apartment = -1;
        let mut qualifier = 0;
        // SAFETY: read-only apartment query; never initialize/change COM here.
        unsafe {
            let status = windows_sys::Win32::System::Com::CoGetApartmentType(&mut apartment, &mut qualifier);
            apartment = if status >= 0 { apartment | (qualifier << 8) } else { -1 };
            (
                windows_sys::Win32::System::Threading::GetCurrentThreadId() as u64,
                0,
                apartment,
            )
        }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn pthread_threadid_np(thread: *mut std::ffi::c_void, id: *mut u64) -> i32;
            fn pthread_main_np() -> i32;
        }
        let mut id = 0;
        // SAFETY: null selects the calling thread; id is a writable scalar.
        unsafe {
            let _ = pthread_threadid_np(std::ptr::null_mut(), &mut id);
            (id, (pthread_main_np() != 0) as u8, -1)
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        (0, 0, -1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_restores_on_nested_callback_and_unwind_and_preserves_request_bits() {
        let c = Context::default().with_request([255; 16]);
        assert_eq!(c.request, [u32::MAX; 4]);
        c.scope(|| {
            assert_eq!(Context::current(), c);
            let _ = std::panic::catch_unwind(|| Context::default().scope(|| panic!("test unwind")));
            assert_eq!(Context::current(), c);
        });
        assert_eq!(Context::current(), Context::default());
    }
    #[test]
    fn ordinary_trace_logs_never_format_io_error_payloads() {
        use std::{io::Write, sync::Arc};
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Writer(Arc::default());
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let trace = Trace::new(stage::LOW_OPEN, Context::default().with_request([255; 16]));
            let result: std::io::Result<()> = Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "PRIVATE-PAYLOAD-PATH-ERROR",
            ));
            trace.io(&result);
            assert_eq!(trace.result.get(), -10);
        });
        let bytes = output.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("dr_request0=4294967295"));
        assert!(text.contains("dr_result=-10"));
        assert!(!text.contains("PRIVATE"));
    }
    #[test]
    fn diagnostics_budget_marks_loss_once_and_resets_without_disabling_work() {
        let b = Budget::default();
        for _ in 0..LIMIT {
            assert_eq!(b.admit(1), (true, false));
        }
        assert_eq!(b.admit(1), (false, true));
        assert_eq!(b.admit(1), (false, false));
        assert_eq!(b.admit(2), (true, false));
    }
}
