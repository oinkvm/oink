//! 剪贴板专用线程：一个消息窗口（`HWND_MESSAGE`）+ 消息循环。
//!
//! - 读写都在这个线程上执行（`Worker::run`）：剪贴板的所有者窗口固定，`OpenClipboard(hwnd)` 之后
//!   `EmptyClipboard` 才能把所有权交给我们的窗口（传 NULL 时文档规定 `SetClipboardData` 会失败）。
//! - 线程启动时 `OleInitialize`（STA）：虚拟文件（`OleSetClipboard` + 自定义 `IDataObject`，见 `vfile`）
//!   的数据对象放在这个线程上，其他进程的调用与 `WM_RENDERFORMAT` 由这里的消息循环派发。
//! - `AddClipboardFormatListener` 把 `WM_CLIPBOARDUPDATE` 发到这个窗口；回调在另一个「通知线程」上调用，
//!   这样回调里再调用 `read()` 不会死锁，慢回调也不会阻塞消息循环。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardOwner, GetClipboardSequenceNumber, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE,
    WM_CLOSE, WM_DESTROY, WNDCLASSEXW,
};
use windows::core::w;

use crate::{ChangeHandler, ClipboardError};

const WM_APP_JOB: u32 = WM_APP + 1;
/// 等待剪贴板线程完成一次读写的上限。
const JOB_TIMEOUT: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce(HWND) + Send>;

struct ThreadState {
    jobs: mpsc::Receiver<Job>,
    notify: mpsc::Sender<()>,
    shared: Arc<Shared>,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct Shared {
    handlers: Mutex<Vec<ChangeHandler>>,
    listening: AtomicBool,
    /// 剪贴板线程已 `OleInitialize`。
    ole: AtomicBool,
    /// Only this STA publishes through its internal OLE owner window. Updated
    /// after each successful publication and cleared before ordinary writes.
    publication: Mutex<Option<(isize, u64)>>,
}

pub(super) fn publication_changed(count: Option<u64>) {
    let publication = count.and_then(|count| {
        // SAFETY: window-station metadata; no COM object or data rendering is queried.
        unsafe { GetClipboardOwner() }
            .ok()
            .map(|owner| (owner.0 as isize, count))
    });
    STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            *state.shared.publication.lock().unwrap() = publication;
        }
    });
}

pub(crate) struct Worker {
    hwnd: isize,
    thread: ThreadId,
    jobs: mpsc::Sender<Job>,
    notify: mpsc::Sender<()>,
    shared: Arc<Shared>,
    closed: Mutex<mpsc::Receiver<()>>,
    wait_on_drop: bool,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("hwnd", &self.hwnd)
            .finish_non_exhaustive()
    }
}

impl Worker {
    /// Observe metadata without queueing behind a synchronous GetData callback.
    /// In particular the clipboard actor must be able to serve/fail remote reads
    /// while this STA is waiting for a different remote representation.
    pub(crate) fn change_count(&self) -> u64 {
        let publication = self.shared.publication.lock().unwrap();
        // OleSetClipboard assigns an internal OLE owner HWND. All publications
        // on this STA pass through vfile::publish, so the HWND plus our latest
        // successful publication distinguishes own delayed rendering from a new
        // copy. Another owner is visible immediately, even if WM_CLIPBOARDUPDATE
        // is still queued behind GetData. No apartment-bound IDataObject escapes.
        // Native gated-STA/owner replacement tests cover Windows 11 build 26200.
        // https://learn.microsoft.com/windows/win32/api/ole2/nf-ole2-olesetclipboard
        // SAFETY: both calls only inspect the calling process's window station.
        unsafe {
            if let Some((owner, count)) = *publication
                && GetClipboardOwner()
                    .ok()
                    .is_some_and(|current| current.0 as isize == owner)
            {
                return count;
            }
            u64::from(GetClipboardSequenceNumber())
        }
    }

    pub(crate) fn spawn() -> Result<Self, ClipboardError> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (notify_tx, notify_rx) = mpsc::channel::<()>();
        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, String>>();
        let (closed_tx, closed_rx) = mpsc::channel();
        let sh = Arc::clone(&shared);
        let worker_notify = notify_tx.clone();
        let handle = std::thread::Builder::new()
            .name("oink-clipboard".into())
            .spawn(move || {
                run(job_rx, notify_tx, sh, ready_tx);
                let _ = closed_tx.send(());
            })
            .map_err(|e| ClipboardError::Platform(format!("创建剪贴板线程失败：{e}")))?;
        let thread = handle.thread().id();
        let hwnd = ready_rx
            .recv()
            .map_err(|_| ClipboardError::Platform("剪贴板线程意外退出".into()))?
            .map_err(ClipboardError::Platform)?;
        // 通知线程：合并连续的变化信号，逐个调用回调
        let sh = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("oink-clipboard-notify".into())
            .spawn(move || {
                while notify_rx.recv().is_ok() {
                    while notify_rx.try_recv().is_ok() {}
                    let handlers = sh.handlers.lock().map(|h| h.clone()).unwrap_or_default();
                    for h in handlers {
                        h();
                    }
                }
            })
            .map_err(|e| ClipboardError::Platform(format!("创建通知线程失败：{e}")))?;
        Ok(Self {
            hwnd,
            thread,
            jobs: job_tx,
            notify: worker_notify,
            shared,
            closed: Mutex::new(closed_rx),
            wait_on_drop: true,
        })
    }

    /// A source application's COM call cannot be forcibly interrupted. Each live
    /// offer gets its own reader STA so a hung old owner cannot block capturing
    /// or pasting a newer copy. Closing it must not join on the network executor;
    /// WM_CLOSE releases apartment state when the outstanding native call exits.
    pub(crate) fn spawn_reader() -> Result<Self, ClipboardError> {
        let mut worker = Self::spawn()?;
        worker.wait_on_drop = false;
        Ok(worker)
    }

    /// 在剪贴板线程上执行 `f`（参数为剪贴板窗口），等待结果。已在该线程上时直接执行。
    pub(crate) fn run<R, F>(&self, f: F) -> Result<R, ClipboardError>
    where
        R: Send + 'static,
        F: FnOnce(HWND) -> R + Send + 'static,
    {
        if std::thread::current().id() == self.thread {
            return Ok(f(HWND(self.hwnd as _)));
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(move |hwnd| {
            let _ = tx.send(f(hwnd));
        })?;
        rx.recv_timeout(JOB_TIMEOUT)
            .map_err(|_| ClipboardError::Platform("剪贴板线程无响应".into()))
    }

    pub(crate) fn post(&self, f: impl FnOnce(HWND) + Send + 'static) -> Result<(), ClipboardError> {
        self.jobs
            .send(Box::new(f))
            .map_err(|_| ClipboardError::Platform("剪贴板线程已退出".into()))?;
        // SAFETY: a message window owned by this worker, valid until Worker drop.
        unsafe { PostMessageW(Some(HWND(self.hwnd as _)), WM_APP_JOB, WPARAM(0), LPARAM(0)) }
            .map_err(|e| ClipboardError::Platform(format!("PostMessageW: {e}")))
    }

    /// A background read may suppress WM_CLIPBOARDUPDATE until identity validation
    /// finishes. Recheck then, including a genuine copy made during that read.
    pub(crate) fn notify(&self) {
        let _ = self.notify.send(());
    }

    /// 剪贴板线程是否已初始化 OLE（虚拟文件需要）。
    pub(crate) fn ole_ready(&self) -> bool {
        self.shared.ole.load(Ordering::SeqCst)
    }

    pub(crate) fn set_watch_paused(&self, paused: bool) -> Result<(), ClipboardError> {
        let listening = !paused;
        if self.shared.listening.swap(listening, Ordering::SeqCst) != listening {
            self.run(move |hwnd| {
                // SAFETY: hwnd is this worker's live window; registration runs on its owner thread.
                unsafe {
                    if paused {
                        RemoveClipboardFormatListener(hwnd)
                    } else {
                        AddClipboardFormatListener(hwnd)
                    }
                }
            })?
            .map_err(|error| ClipboardError::Platform(error.to_string()))?;
        }
        Ok(())
    }

    pub(crate) fn watch(&self, handler: ChangeHandler) -> Result<(), ClipboardError> {
        self.shared
            .handlers
            .lock()
            .map_err(|_| ClipboardError::Platform("回调列表锁已损坏".into()))?
            .push(handler);
        if !self.shared.listening.swap(true, Ordering::SeqCst) {
            let r = self.run(|hwnd| {
                // SAFETY: hwnd 是本线程创建的消息窗口。
                unsafe { AddClipboardFormatListener(hwnd) }
            })?;
            if let Err(e) = r {
                self.shared.listening.store(false, Ordering::SeqCst);
                return Err(ClipboardError::Platform(format!("AddClipboardFormatListener: {e}")));
            }
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: 同 run()；窗口已销毁时只是失败。
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        // 正常进程退出前完成 OLE 清理，不能依赖调用方在 drop 后 sleep。
        // 同线程释放时不能等自己；异常 COM/系统挂起时有界返回，不让退出永远卡住。
        if self.wait_on_drop
            && std::thread::current().id() != self.thread
            && let Ok(closed) = self.closed.get_mut()
            && let Err(e) = closed.recv_timeout(Duration::from_secs(2))
        {
            tracing::warn!("等待剪贴板线程退出失败，文件格式可能无法清理：{e}");
        }
    }
}

fn run(
    jobs: mpsc::Receiver<Job>,
    notify: mpsc::Sender<()>,
    shared: Arc<Shared>,
    ready: mpsc::Sender<Result<isize, String>>,
) {
    // SAFETY: 标准的窗口类注册、窗口创建与消息循环；窗口过程只访问本线程的 thread_local。
    unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("GetModuleHandleW: {e}")));
                return;
            }
        };
        let class = w!("OinkClipboardWindow");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            let _ = ready.send(Err(format!("RegisterClassExW: {:?}", GetLastError())));
            return;
        }
        match OleInitialize(None) {
            Ok(()) => shared.ole.store(true, Ordering::SeqCst),
            Err(e) => tracing::warn!("OleInitialize 失败，虚拟文件不可用：{e}"),
        }
        let ole = shared.ole.load(Ordering::SeqCst);
        STATE.with(|s| *s.borrow_mut() = Some(ThreadState { jobs, notify, shared }));
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Oink clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("CreateWindowExW: {e}")));
                return;
            }
        };
        let _ = ready.send(Ok(hwnd.0 as isize));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // OLE 的剪贴板窗口与 COM 的派发都依赖 TranslateMessage/DispatchMessage
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        STATE.with(|s| s.borrow_mut().take());
        if ole {
            OleUninitialize();
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_JOB => {
            // 先取出所有任务再执行，避免执行任务时持有 RefCell 借用
            let pending: Vec<Job> = STATE.with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|st| st.jobs.try_iter().collect())
                    .unwrap_or_default()
            });
            for job in pending {
                job(hwnd);
            }
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            super::vfile::release_if_replaced();
            STATE.with(|s| {
                if let Some(st) = s.borrow().as_ref()
                    && st.shared.listening.load(Ordering::SeqCst)
                {
                    let _ = st.notify.send(());
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            super::on_worker_close(hwnd);
            // SAFETY: 本线程自己的窗口。
            unsafe {
                let _ = RemoveClipboardFormatListener(hwnd);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: 结束本线程的消息循环。
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: 其余消息交给默认处理。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
