//! Windows native drag capture, handoff and destination publication.
//!
//! - 一个专门的 STA 线程（`OleInitialize`、按显示器 DPI 感知 v2，坐标与截获一致为物理像素）+ 消息循环，
//!   两个 `WS_POPUP` 窗口：`WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`，alpha = 1
//!   （alpha 0 的分层窗口不参与命中测试），`WM_MOUSEACTIVATE` 返回不激活。
//!   - **接收窗口**：`RegisterDragDrop`。`DragEnter` 读 `CF_HDROP`（文件夹在另一个线程展开）、文本、图片，报告
//!     `Detected`；`Drop` 只在 `release` 期间接受（交出源拖拽），其他时候拒绝。
//!   - **发起窗口**：`begin` 把它放到光标下、注入左键按下；`WM_LBUTTONDOWN` 里 `DoDragDrop`（模态，内部继续
//!     派发消息与 COM 调用）。数据对象与剪贴板的虚拟文件是同一个（`vfile::build`），`FileContents` 流在 MTA 上
//!     封送，资源管理器在后台线程读取。
//! - `IDropSource::QueryContinueDrag`：取消标志 → `DRAGDROP_S_CANCEL`；左键抬起 → `DRAGDROP_S_DROP`。
//!   `cancel` 置取消标志后注入左键抬起：拖拽循环看到抬起、按取消结束，按钮状态也回到抬起。

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use oink_foundation::Point;
use oink_foundation::clipboard::ClipData;
use oink_foundation::drag_trace::{Context as ReadContext, Trace as ReadTrace, stage as read_stage};
use oink_foundation::input::{InjectEvent, MouseButton};
use windows::Win32::Foundation::{
    COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, ERROR_CLASS_ALREADY_EXISTS,
    GetLastError, HWND, LPARAM, LRESULT, POINT, POINTL, S_OK, WPARAM,
};
use windows::Win32::System::Com::{
    CoTaskMemFree, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, IDropSourceNotify, IDropSourceNotify_Impl, IDropTarget,
    IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GUITHREADINFO, GetCursorPos, GetGUIThreadInfo, GetMessageW,
    HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE, MSG, PostMessageW, RegisterClassExW, SW_HIDE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SetCursor, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WM_APP, WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_SETCURSOR, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WindowFromPoint,
};
use windows::core::{BOOL, HRESULT, Ref, implement, w};

use super::{files, formats, prepare_items, vfile};
use crate::dnd::{
    DndBackend, DndSink, DragOperation, DragOps, DragSpec, Injector, PlatformDndEvent, SourcePress, SourceStart,
    SourceStartTrace,
};
use crate::{ClipboardError, FileContents};

mod parking;

/// 窗口边长（物理像素）。
const SIZE: i32 = 48;
/// 等拖放线程、等系统拖拽的上限。
const WAIT: Duration = Duration::from_secs(1);
const WM_APP_JOB: u32 = WM_APP + 1;

pub(crate) fn backend() -> Result<Box<dyn DndBackend>, ClipboardError> {
    Ok(Box::new(WinDnd {
        shared: Arc::new(Shared {
            native_jobs: OnceLock::new(),
            sink: OnceLock::new(),
            inject: OnceLock::new(),
            source_press: OnceLock::new(),
            source_start: OnceLock::new(),
            st: Mutex::new(St::default()),
            cv: Condvar::new(),
        }),
        thread: OnceLock::new(),
    }))
}

#[derive(Default)]
struct St {
    content_root: Option<std::path::PathBuf>,
    cloud: Option<super::cloud::Owner>,
    source_data: Option<(DragOperation, Arc<crate::content::Deferred>)>,
    source_virtual: Option<Arc<super::content::virtual_files::Virtual>>,
    source_present: bool,
    source_operation: Option<DragOperation>,
    /// Retained through native end for keyed cleanup; revoked by a new probe.
    probe_operation: Option<DragOperation>,
    held_source: Option<DragOperation>,
    held_summary: Option<HoldSummary>,
    releasing: bool,
    released: bool,
    probing: bool,
    /// 这次拖入已经报告过（`DragEnter` 在拖拽进出窗口时会多次调用）。
    reported: bool,
    began: bool,
    native_begin: Option<(DragOperation, std::time::Instant)>,
    press_received: bool,
    active: bool,
    ended: bool,
    cancel: bool,
    source_image: Option<oink_foundation::dnd::DragImage>,
    target_image: Option<oink_foundation::dnd::DragImage>,
    target_preview: Option<(DragOperation, Vec<ClipData>)>,
}

struct HoldEnd {
    at: POINT,
    hit: bool,
    park_window: u64,
}

struct HoldSummary {
    since: Instant,
    surface: usize,
    from: POINT,
    park: POINT,
}

impl HoldSummary {
    fn sample(&self) -> HoldEnd {
        let mut at = POINT::default();
        // SAFETY: read-only position/hit test on the native source/catcher path.
        let hit = unsafe { GetCursorPos(&mut at).is_ok() && WindowFromPoint(at).0 as usize == self.surface };
        HoldEnd {
            at,
            hit,
            park_window: parking::window_kind_at(self.park, self.surface),
        }
    }
    fn finish(self, reason: u64, observed: Option<HoldEnd>) {
        let HoldEnd { at, hit, park_window } = observed.unwrap_or_else(|| self.sample());
        // At most one summary per hold, on resume/drop/loss. Never payload,
        // file names, format names, window titles, or per-DragOver diagnostics.
        tracing::info!(
            dnd_hold_end = reason,
            elapsed_us = self.since.elapsed().as_micros() as u64,
            from_x = self.from.x,
            from_y = self.from.y,
            park_x = self.park.x,
            park_y = self.park.y,
            end_x = at.x,
            end_y = at.y,
            catcher_hit = hit,
            dnd_park_window = park_window,
            "Windows held source summary"
        );
    }
}

struct Shared {
    native_jobs: OnceLock<(mpsc::Sender<Job>, isize)>,
    sink: OnceLock<DndSink>,
    inject: OnceLock<Injector>,
    source_press: OnceLock<SourcePress>,
    source_start: OnceLock<SourceStart>,
    st: Mutex<St>,
    cv: Condvar,
}

impl Shared {
    fn source_ended(&self, accepted: bool, reason: u64) {
        // Sample at callback entry, before dropping the COM source reference or
        // notifying the router can change native hit testing. One end record only.
        let observed = self.lock().held_summary.as_ref().map(HoldSummary::sample);
        LOCAL.with(|local| {
            if let Some(local) = local.borrow_mut().as_mut() {
                local.native_source = None;
            }
        });
        let (operation, summary) = {
            let mut st = self.lock();
            if !accepted
                && st.held_source.is_some()
                && let Some((_, data)) = &st.source_data
            {
                data.fail(crate::model::Unavailable::Cancelled);
            }
            st.source_present = false;
            st.source_operation = None;
            st.releasing = false;
            (st.held_source.take(), st.held_summary.take())
        };
        if let Some(summary) = summary {
            summary.finish(reason, observed);
        }
        self.cv.notify_all();
        if let Some(operation) = operation {
            self.emit(if reason == 3 {
                PlatformDndEvent::SourceCatcherLost { operation }
            } else {
                PlatformDndEvent::SourceEnded { operation, accepted }
            });
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait(&self, timeout: Duration, done: impl Fn(&St) -> bool) -> bool {
        let until = Instant::now() + timeout;
        let mut st = self.lock();
        while !done(&st) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    fn inject(&self, ev: InjectEvent) -> bool {
        let trace = if let InjectEvent::Button { button, pressed } = &ev {
            let button = match button {
                MouseButton::Left => 1,
                MouseButton::Right => 2,
                MouseButton::Middle => 3,
                _ => 0,
            };
            Some(ReadTrace::new(
                read_stage::INJECT_BUTTON,
                ReadContext {
                    aux: button | (u32::from(*pressed) << 8),
                    ..ReadContext::current()
                },
            ))
        } else {
            None
        };
        let result = match self.inject.get().map(|f| f(&ev)) {
            Some(Ok(())) => true,
            Some(Err(e)) => {
                tracing::warn!(%e, ?ev, "drag and drop: inject failed");
                false
            }
            None => false,
        };
        if let Some(trace) = trace {
            trace.result(if result { 0 } else { -1 });
        }
        result
    }

    fn emit(&self, ev: PlatformDndEvent) {
        if let Some(s) = self.sink.get() {
            s(ev);
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

struct Thread {
    catcher: isize,
    source: isize,
    jobs: mpsc::Sender<Job>,
}

struct LiveDrag {
    operation: DragOperation,
    data: IDataObject,
    helper: Option<windows::Win32::UI::Shell::IDragSourceHelper>,
    refresh: Option<windows::Win32::UI::Shell::IDropTargetHelper>,
    feedback_entered: bool,
    thumbnail_applied: bool,
    image_window: Option<HWND>,
}
impl LiveDrag {
    fn new(
        operation: DragOperation,
        data: IDataObject,
        helper: Option<windows::Win32::UI::Shell::IDragSourceHelper>,
    ) -> Self {
        use windows::core::Interface;
        let refresh = helper.as_ref().and_then(|h| h.cast().ok());
        Self {
            operation,
            data,
            helper,
            refresh,
            feedback_entered: false,
            thumbnail_applied: false,
            image_window: None,
        }
    }
}
struct Local {
    jobs: mpsc::Receiver<Job>,
    shared: Arc<Shared>,
    source: isize,
    pending: Option<DragSpec>,
    native_source: Option<(DragOperation, IDataObject)>,
    live_drag: Option<LiveDrag>,
}

thread_local! {
    static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
}

struct WinDnd {
    shared: Arc<Shared>,
    thread: OnceLock<Thread>,
}

impl WinDnd {
    fn thread(&self) -> Option<&Thread> {
        self.thread.get()
    }

    /// 在拖放线程上执行 `f` 并等结果。
    fn run<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> Option<R> {
        let t = self.thread()?;
        let (tx, rx) = mpsc::sync_channel(1);
        let pending = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let queued = pending.clone();
        t.jobs
            .send(Box::new(move || {
                if queued.swap(false, Ordering::AcqRel) {
                    let _ = tx.send(f());
                }
            }))
            .ok()?;
        // SAFETY: 窗口属于拖放线程；PostMessage 可跨线程调用。
        unsafe { PostMessageW(Some(HWND(t.catcher as _)), WM_APP_JOB, WPARAM(0), LPARAM(0)) }.ok()?;
        let result = rx.recv_timeout(WAIT).ok();
        pending.store(false, Ordering::Release);
        result
    }

    fn hide(&self, hwnd: isize) {
        let _ = self.run(move || {
            // SAFETY: 本线程的窗口。
            unsafe {
                let _ = ShowWindow(HWND(hwnd as _), SW_HIDE);
            }
        });
    }
}

/// Read hit testing on the DPI-aware OLE thread. A successful injected button
/// does not establish that its message reached this window (local motion and
/// another window's mouse capture can change the recipient).
fn trace_source_hit(source: isize, stage: &str) {
    let mut point = POINT::default();
    let mut gui = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: Both APIs only fill these initialized output structures.
    let (cursor, hit, capture) = unsafe {
        let cursor = GetCursorPos(&mut point).ok().map(|()| (point.x, point.y));
        let hit = cursor.map(|_| WindowFromPoint(point).0 as isize);
        let capture = GetGUIThreadInfo(0, &mut gui).ok().map(|()| gui.hwndCapture.0 as isize);
        (cursor, hit, capture)
    };
    tracing::debug!(stage, source, ?cursor, ?hit, ?capture, "Windows drag source hit test");
}

/// 把窗口放到以 `at` 为中心的位置、置顶显示，不激活。
fn place(hwnd: HWND, at: Point) -> bool {
    let (x, y) = (at.x.round() as i32 - SIZE / 2, at.y.round() as i32 - SIZE / 2);
    // SAFETY: 本线程的窗口。
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            SIZE,
            SIZE,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    }
    .is_ok()
}

impl DndBackend for WinDnd {
    fn observe_native_begin(
        &self,
        operation: DragOperation,
        _wait: bool,
        callback: Box<dyn FnOnce(Option<std::time::Instant>) + Send>,
    ) {
        let at = self
            .shared
            .lock()
            .native_begin
            .filter(|(op, _)| *op == operation)
            .map(|(_, at)| at);
        callback(at);
    }
    fn observe_native_timing(&self, operation: DragOperation, _wait: bool, timing: crate::dnd::NativeBeginTiming) {
        let at = self
            .shared
            .lock()
            .native_begin
            .filter(|(op, _)| *op == operation)
            .map(|(_, at)| at);
        timing.complete(at);
    }
    fn set_source_press(&self, press: SourcePress) {
        let _ = self.shared.source_press.set(press);
    }
    fn set_source_start(&self, start: SourceStart) {
        let _ = self.shared.source_start.set(start);
    }
    fn start(&self, sink: DndSink, inject: Injector) -> Result<(), ClipboardError> {
        let _ = self.shared.sink.set(sink);
        let _ = self.shared.inject.set(inject);
        if self.thread.get().is_some() {
            return Ok(());
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(isize, isize), String>>();
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("oink-dnd".into())
            .spawn(move || run_thread(job_rx, shared, ready_tx))
            .map_err(|e| ClipboardError::Platform(format!("创建拖放线程失败：{e}")))?;
        let (catcher, source) = ready_rx
            .recv()
            .map_err(|_| ClipboardError::Platform("拖放线程意外退出".into()))?
            .map_err(ClipboardError::Platform)?;
        let _ = self.shared.native_jobs.set((job_tx.clone(), catcher));
        let _ = self.thread.set(Thread {
            catcher,
            source,
            jobs: job_tx,
        });
        Ok(())
    }

    fn probe(&self, at: Point) {
        {
            let mut st = self.shared.lock();
            if st.held_source.is_some() {
                return; // A queued follow must not shrink the live parking surface.
            }
            if !st.probing {
                st.reported = false;
                st.probe_operation = None;
            }
            st.probing = true;
        }
        if let Some(t) = self.thread() {
            let hwnd = t.catcher;
            let shared = self.shared.clone();
            let placed = self.run(move || {
                let st = shared.lock();
                if !st.probing || st.held_source.is_some() {
                    return true;
                }
                drop(st);
                place(HWND(hwnd as _), at)
            });
            if placed != Some(true) {
                tracing::warn!("placing the edge catcher failed");
            }
        }
    }

    fn unprobe(&self) {
        {
            let mut st = self.shared.lock();
            st.probing = false;
            st.probe_operation = None;
            st.reported = false;
        }
        if let Some(t) = self.thread() {
            self.hide(t.catcher);
        }
    }

    fn unprobe_source(&self, operation: DragOperation) {
        let Some(t) = self.thread() else { return };
        let hwnd = t.catcher;
        let shared = self.shared.clone();
        let _ = self.run(move || {
            let mut st = shared.lock();
            if st.probe_operation != Some(operation) {
                return;
            }
            st.probing = false;
            st.probe_operation = None;
            st.reported = false;
            drop(st);
            // The identity check and hide share the OLE thread, so a newer
            // DragEnter cannot replace the capture between them.
            // SAFETY: the catcher belongs to this thread.
            unsafe {
                let _ = ShowWindow(HWND(hwnd as _), SW_HIDE);
            }
        });
    }

    fn set_content_store(&self, root: std::path::PathBuf) {
        let cloud = super::cloud::initialize(root.clone());
        let mut state = self.shared.lock();
        state.content_root = Some(root);
        state.cloud = cloud;
    }
    fn source_content(&self, operation: DragOperation) -> Option<Arc<dyn crate::content::Provider>> {
        self.shared
            .lock()
            .source_data
            .as_ref()
            .filter(|(op, _)| *op == operation)
            .map(|(_, source)| source.clone() as Arc<dyn crate::content::Provider>)
    }
    fn forget_content(&self, operation: DragOperation) {
        let mut st = self.shared.lock();
        if st.source_data.as_ref().is_some_and(|(op, _)| *op == operation) {
            st.source_data = None;
            st.source_virtual = None;
        }
    }
    fn hold_source(&self, operation: DragOperation) -> Result<oink_foundation::dnd::DragAnchor, ClipboardError> {
        let t = self.thread().ok_or(ClipboardError::Unsupported)?;
        let surface = t.catcher as usize;
        let shared = self.shared.clone();
        self.run(move || {
            {
                let mut st = shared.lock();
                if !st.probing
                    || !st.source_present
                    || st.held_source.is_some()
                    || st.source_operation != Some(operation)
                {
                    return Err(ClipboardError::Platform(format!(
                        "catcher state: probing={} present={} already_held={} operation_matches={}",
                        st.probing,
                        st.source_present,
                        st.held_source.is_some(),
                        st.source_operation == Some(operation)
                    )));
                }
                st.held_source = Some(operation);
            }
            let mut p = POINT::default();
            // A target DragLeave does not prove the source's DoDragDrop ended.
            // Keep this SAME no-helper HWND beneath departure, parking, and
            // every possible return point. Moving a 48px catcher first would
            // briefly expose the browser/taskbar and revoke the held source.
            // GiveFeedback owns the cursor: SetCursor(NULL) cannot hide it
            // reliably. Park the top-left hotspot on a free right/bottom outer edge;
            // other hotspots may still leave a few pixels visible.
            let (bounds, park) = parking::desktop(surface).ok_or_else(|| {
                shared.lock().held_source = None;
                ClipboardError::Platform("held catcher has no available exterior parking point".into())
            })?;
            // SAFETY: the STA owns this live HWND; the alpha=1 layered window
            // remains hit-testable, topmost, and nonactivating across the warp.
            unsafe {
                let position_ok = GetCursorPos(&mut p).is_ok();
                let hwnd = HWND(surface as _);
                let hit = position_ok && WindowFromPoint(p) == hwnd;
                if !hit {
                    shared.lock().held_source = None;
                    return Err(ClipboardError::Platform(format!(
                        "catcher hit test: position_ok={position_ok} hit={hit}"
                    )));
                }
                if SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    bounds.left,
                    bounds.top,
                    bounds.right - bounds.left,
                    bounds.bottom - bounds.top,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
                .is_err()
                    || WindowFromPoint(p) != hwnd
                    || WindowFromPoint(park) != hwnd
                {
                    shared.lock().held_source = None;
                    let _ = place(hwnd, Point::new(p.x.into(), p.y.into()));
                    return Err(ClipboardError::Platform(
                        "held catcher does not own parking surface".into(),
                    ));
                }
                SetCursor(None);
            }
            shared.lock().held_summary = Some(HoldSummary {
                since: Instant::now(),
                surface,
                from: p,
                park,
            });
            Ok(oink_foundation::dnd::DragAnchor {
                generation: operation.sequence,
                at: Point::new(park.x.into(), park.y.into()),
                surface,
            })
        })
        .ok_or_else(|| ClipboardError::Platform("catcher STA did not acknowledge hold".into()))?
    }

    fn resume_source(&self, operation: DragOperation) -> bool {
        let summary = {
            let mut st = self.shared.lock();
            if st.held_source != Some(operation) || !st.source_present {
                return false;
            }
            st.held_source = None;
            st.held_summary.take()
        };
        if let Some(summary) = summary {
            // Read geometry on the same DPI-aware STA that captured the hold.
            let _ = self.run(move || summary.finish(1, None));
        }
        self.unprobe();
        true
    }

    fn source_completion_is_async(&self) -> bool {
        true
    }
    fn finish_source(&self, operation: DragOperation, accept: bool, release: &dyn Fn() -> Result<(), String>) -> bool {
        {
            let mut st = self.shared.lock();
            if st.held_source != Some(operation) {
                return false;
            }
            st.releasing = accept;
            st.released = false;
        }
        if !accept {
            // Third-party drag sources do not observe Oink's own cancel flag.
            // Tagged Escape reaches their native modal loop on both platforms.
            for pressed in [true, false] {
                if !self.shared.inject(InjectEvent::Key {
                    code: oink_foundation::input::KeyCode::ESCAPE,
                    pressed,
                }) {
                    return false;
                }
            }
        }
        // Drop can synchronously snapshot a large provider. Do not block the
        // input action queue or invent completion from a short acknowledgement.
        release().is_ok()
    }

    fn release(&self) -> bool {
        {
            let mut st = self.shared.lock();
            st.releasing = true;
            st.released = false;
        }
        let injected = self.shared.inject(InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        });
        let ok = injected && self.shared.wait(WAIT, |st| st.released);
        {
            let mut st = self.shared.lock();
            st.releasing = false;
            st.probing = false;
            st.reported = false;
        }
        if let Some(t) = self.thread() {
            self.hide(t.catcher);
        }
        if !ok {
            tracing::warn!(injected, "source drag did not end on the catcher");
        }
        ok
    }

    fn source_image(&self) -> Option<oink_foundation::dnd::DragImage> {
        self.shared.lock().source_image.clone()
    }
    fn set_target_image(&self, image: Option<oink_foundation::dnd::DragImage>) {
        let mut st = self.shared.lock();
        st.target_image = image.filter(|i| i.valid());
        st.target_preview = None;
    }
    fn update_target_image(&self, operation: DragOperation, image: oink_foundation::dnd::DragImage) {
        if !image.valid() {
            return;
        }
        // Decode on the ordinary artwork worker, before entering OLE's STA.
        let Ok(pixels) = super::image::decode_png(&image.png) else {
            return;
        };
        let Some(thread) = self.thread() else { return };
        let _ = thread.jobs.send(Box::new(move || {
            let mut point = POINT::default();
            // SAFETY: read-only pointer query on the OLE STA.
            if unsafe { GetCursorPos(&mut point) }.is_ok() {
                replace_live_image(operation, image, pixels, point);
            }
        }));
        // SAFETY: asynchronous wake of our own STA window, never awaited.
        unsafe {
            let _ = PostMessageW(Some(HWND(thread.catcher as _)), WM_APP_JOB, WPARAM(0), LPARAM(0));
        }
    }
    fn prepare_preview(&self, spec: &DragSpec) -> bool {
        let Some(native) = spec
            .native
            .as_ref()
            .filter(|native| native.inventory.roots.is_empty() && native.files.is_empty())
        else {
            return false;
        };
        if self.shared.lock().target_image.is_some() {
            return false;
        }
        let preview = crate::dnd::preview::content(
            native,
            oink_foundation::clipboard::ClipPlatform::Windows,
            &super::content::WindowsTranslator,
        );
        self.shared.lock().target_preview = Some((spec.operation, preview));
        true
    }
    fn begin(&self, spec: DragSpec) -> Result<(), ClipboardError> {
        let mut trace = SourceStartTrace::default();
        tracing::debug!(?spec, "starting a drag");
        let t = self.thread().ok_or(ClipboardError::Unsupported)?;
        {
            let mut st = self.shared.lock();
            st.began = false;
            st.native_begin = None;
            st.press_received = false;
            st.ended = false;
            st.cancel = false;
        }
        let operation = spec.operation;
        let source = t.source;
        let prepared = self.run(move || {
            LOCAL.with(|l| {
                if let Some(l) = l.borrow_mut().as_mut() {
                    l.pending = Some(spec);
                }
            });
            true
        });
        if prepared != Some(true) {
            return Err(ClipboardError::Platform("drag source window unavailable".into()));
        }
        let mut down = false;
        let mut critical_at = None;
        let mut native_at = None;
        let Some(current) = self
            .run(move || {
                let mut p = POINT::default();
                // SAFETY: p is valid output storage on the DPI-aware window thread.
                unsafe { GetCursorPos(&mut p) }.ok()?;
                let at = Point::new(f64::from(p.x), f64::from(p.y));
                place(HWND(source as _), at).then_some(at)
            })
            .flatten()
        else {
            return Err(ClipboardError::Platform("drag window placement failed".into()));
        };
        let at = current;
        let mut critical = || {
            critical_at = Some(Instant::now());
            native_at = Some(Instant::now());
            // A positioned Down cannot be displaced by the local physical mouse.
            // The injection owner additionally excludes routed MoveTo until the
            // positioned Down is submitted, without gating window preparation or OLE.
            down = self.shared.source_press.get().map_or_else(
                || {
                    self.shared.inject(InjectEvent::Button {
                        button: MouseButton::Left,
                        pressed: true,
                    })
                },
                |press| press(operation, at).is_ok(),
            );
        };
        let result = if let Some(start) = self.shared.source_start.get() {
            start(&mut critical)
        } else {
            critical();
            Ok(0)
        };
        trace.critical_us = critical_at.map_or(0, |at| at.elapsed().as_micros() as u64);
        let received = down && self.shared.wait(WAIT, |st| st.press_received);
        trace.offset = self
            .shared
            .source_press
            .get()
            .filter(|_| down)
            .map(|_| Point::default());
        trace.received = received;
        trace.dropped = result.as_ref().copied().unwrap_or(0);
        let remaining = native_at.map_or(WAIT, |at| WAIT.saturating_sub(at.elapsed()));
        if result.is_ok() && received && self.shared.wait(remaining, |st| st.began) {
            return Ok(());
        }
        tracing::warn!(down, ?at, source, "the drag did not start: releasing the button");
        if down {
            self.shared.inject(InjectEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            });
        }
        let _ = self.run(move || {
            trace_source_hit(source, "source press timed out");
            LOCAL.with(|l| {
                if let Some(l) = l.borrow_mut().as_mut() {
                    l.pending = None;
                }
            });
            // SAFETY: 本线程的窗口。
            unsafe {
                let _ = ShowWindow(HWND(source as _), SW_HIDE);
            }
        });
        Err(ClipboardError::Platform("drag did not start".into()))
    }

    fn cancel(&self) -> bool {
        {
            let mut st = self.shared.lock();
            if !st.active {
                return true;
            }
            st.cancel = true;
            st.ended = false;
        }
        // 拖拽循环看到抬起，按取消标志结束（不放下）；按钮状态回到抬起。
        let up = self.shared.inject(InjectEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        });
        let ok = up && self.shared.wait(WAIT, |st| st.ended);
        if !ok {
            tracing::warn!(up, "the drag did not end after cancelling");
        }
        ok
    }
}

// ───────────── 线程 ─────────────

fn run_thread(jobs: mpsc::Receiver<Job>, shared: Arc<Shared>, ready: mpsc::Sender<Result<(isize, isize), String>>) {
    // SAFETY: 标准的窗口类注册、窗口创建与消息循环；窗口过程只访问本线程的 thread_local。
    unsafe {
        // 与截获一致用物理像素（进程可能没有声明 DPI 感知）。
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if let Err(e) = OleInitialize(None) {
            let _ = ready.send(Err(format!("OleInitialize: {e}")));
            return;
        }
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("GetModuleHandleW: {e}")));
                return;
            }
        };
        let class = w!("OinkDndWindow");
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
        let ex = WINDOW_EX_STYLE(WS_EX_LAYERED.0 | WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0);
        let make = |title| {
            let h = CreateWindowExW(
                ex,
                class,
                title,
                WS_POPUP,
                0,
                0,
                SIZE,
                SIZE,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;
            // alpha 0 的分层窗口不参与命中测试：留 1/255。
            SetLayeredWindowAttributes(h, COLORREF(0), 1, LWA_ALPHA)?;
            Ok::<HWND, windows::core::Error>(h)
        };
        let (catcher, source) = match (make(w!("Oink drop catcher")), make(w!("Oink drag source"))) {
            (Ok(c), Ok(s)) => (c, s),
            (Err(e), _) | (_, Err(e)) => {
                let _ = ready.send(Err(format!("CreateWindowExW: {e}")));
                return;
            }
        };
        let target: IDropTarget = Catcher { shared: shared.clone() }.into();
        if let Err(e) = RegisterDragDrop(catcher, &target) {
            let _ = ready.send(Err(format!("RegisterDragDrop: {e}")));
            return;
        }
        LOCAL.with(|l| {
            *l.borrow_mut() = Some(Local {
                jobs,
                shared,
                source: source.0 as isize,
                pending: None,
                native_source: None,
                live_drag: None,
            })
        });
        let _ = ready.send(Ok((catcher.0 as isize, source.0 as isize)));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR
            if LOCAL.with(|l| {
                l.borrow()
                    .as_ref()
                    .is_some_and(|l| l.source != hwnd.0 as isize && l.shared.lock().held_source.is_some())
            }) =>
        {
            // The catcher is also the cover, so its real cursor callback owns
            // the null shape. Never change global system cursor settings.
            // SAFETY: WM_SETCURSOR runs on this window's owning STA.
            unsafe {
                SetCursor(None);
            }
            LRESULT(1)
        }
        WM_APP_JOB => {
            let pending: Vec<Job> = LOCAL.with(|l| {
                l.borrow()
                    .as_ref()
                    .map(|l| l.jobs.try_iter().collect())
                    .unwrap_or_default()
            });
            for job in pending {
                job();
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let is_source = LOCAL.with(|l| l.borrow().as_ref().is_some_and(|l| l.source == hwnd.0 as isize));
            if is_source {
                LOCAL.with(|l| {
                    if let Some(l) = l.borrow().as_ref() {
                        l.shared.lock().press_received = true;
                        l.shared.cv.notify_all();
                    }
                });
                trace_source_hit(hwnd.0 as isize, "source received button down");
                start_drag(hwnd);
            }
            LRESULT(0)
        }
        // SAFETY: 其余消息交给默认处理。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 发起窗口收到（注入的）左键按下：`DoDragDrop`，结束后报告。
mod representation_trace;

fn start_drag(hwnd: HWND) {
    let (spec, shared) = LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        let l = l.as_mut().expect("dnd thread state");
        (l.pending.take(), l.shared.clone())
    });
    let Some(spec) = spec else {
        tracing::debug!("button down on the drag source window without a pending drag");
        return;
    };
    let (data, effects) = match data_object(&spec) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e, "cannot build the drag data");
            return;
        }
    };
    // Begin always uses default artwork. Late thumbnails update this same
    // IDataObject through the helper while OLE keeps the native drag alive.
    let (target_image, preview) = {
        let mut st = shared.lock();
        let preview = st
            .target_preview
            .take()
            .filter(|(op, _)| *op == spec.operation)
            .map(|(_, preview)| preview)
            .unwrap_or_default();
        (st.target_image.take(), preview)
    };
    // Artwork inputs stay out of data_object: native formats/order remain intact.
    let mut artwork = spec.items.clone();
    artwork.extend(preview);
    let image_helper = match super::drag_image::initialize(&data, &artwork, target_image.as_ref()) {
        Ok(helper) => helper,
        Err(e) => {
            tracing::warn!(%e, "cannot initialize drag preview");
            None
        }
    };
    let source: IDropSource = DropSource { shared: shared.clone() }.into();
    // Keep the identity from the submitted spec across OLE's modal loop.
    // ended/notify below can unblock cancel and a replacement session before
    // emit runs; reading the current peer at that point would misattribute the
    // old completion. Native Windows acceptance is tested separately.
    let operation = spec.operation;
    representation_trace::presented(&data, operation);
    {
        let mut st = shared.lock();
        st.began = true;
        st.native_begin = crate::dnd::native_timing_enabled().then(|| (operation, std::time::Instant::now()));
        st.active = true;
        shared.cv.notify_all();
    }
    tracing::info!(effects = effects.0, "drag started on this PC");
    let mut effect = DROPEFFECT_NONE;
    // SAFETY: 在已 OleInitialize 的 STA 线程上调用；对象在调用期间有效。
    LOCAL.with(|l| {
        if let Some(l) = l.borrow_mut().as_mut() {
            l.live_drag = Some(LiveDrag::new(operation, data.clone(), image_helper));
        }
    });
    // SAFETY: data and source are retained throughout the modal OLE call on this STA.
    let ole = ReadTrace::new(
        read_stage::OLE_SESSION,
        ReadContext {
            object: operation.sequence,
            aux: effects.0,
            ..ReadContext::current()
        },
    );
    let hr = ole.scope(|| {
        // SAFETY: retained data/source objects and output effect belong to this initialized STA.
        unsafe { DoDragDrop(&data, &source, effects, &mut effect) }
    });
    ole.result(hr.0);
    drop(ole);
    let completed = ReadTrace::new(
        read_stage::OLE_SESSION,
        ReadContext {
            object: operation.sequence,
            aux: effect.0,
            ..ReadContext::current()
        },
    );
    completed.result(hr.0);
    drop(completed);
    let live = LOCAL.with(|l| l.borrow_mut().as_mut().and_then(|l| l.live_drag.take()));
    if let Some(refresh) = live.filter(|live| live.feedback_entered).and_then(|live| live.refresh) {
        // SAFETY: balances the artwork manager's DragEnter on this STA.
        unsafe {
            let _ = refresh.DragLeave();
        }
    }
    let dropped = hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE;
    // SAFETY: 本线程的窗口。
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    {
        let mut st = shared.lock();
        st.active = false;
        st.ended = true;
        st.cancel = false;
        shared.cv.notify_all();
    }
    tracing::info!(hr = hr.0, effect = effect.0, dropped, "drag started by Oink ended");
    shared.emit(PlatformDndEvent::Ended { operation, dropped });
}

/// 没有文件内容（本机文件、只有文本）时数据对象不会用到的 `FileContents`。
struct NoContents;
impl FileContents for NoContents {
    fn open(&self, _index: usize) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        Err(std::io::Error::other("no file contents"))
    }
}

fn to_effect(ops: DragOps) -> DROPEFFECT {
    let mut e = DROPEFFECT_NONE.0;
    if ops.contains(DragOps::COPY) {
        e |= DROPEFFECT_COPY.0;
    }
    if ops.contains(DragOps::MOVE) {
        e |= DROPEFFECT_MOVE.0;
    }
    if ops.contains(DragOps::LINK) {
        e |= DROPEFFECT_LINK.0;
    }
    DROPEFFECT(e)
}

fn from_effect(e: DROPEFFECT) -> DragOps {
    let mut o = DragOps::default();
    if e.0 & DROPEFFECT_COPY.0 != 0 {
        o = o.union(DragOps::COPY);
    }
    if e.0 & DROPEFFECT_MOVE.0 != 0 {
        o = o.union(DragOps::MOVE);
    }
    if e.0 & DROPEFFECT_LINK.0 != 0 {
        o = o.union(DragOps::LINK);
    }
    o
}

/// 拖拽的数据对象：远端文件是虚拟文件（与剪贴板同一个实现），本机文件是 `CF_HDROP`，另有文本、图片。
fn data_object(spec: &DragSpec) -> Result<(IDataObject, DROPEFFECT), String> {
    let f = formats();
    let mut fixed = Vec::new();
    let mut effects = DROPEFFECT_COPY;
    let mut virtual_files = None;
    let mut cloud = None;
    for item in &spec.items {
        match item {
            ClipData::Files(entries) => match &spec.files {
                Some(contents) => {
                    cloud = super::cloud::Offer::new(
                        entries.clone(),
                        contents.clone(),
                        spec.native.as_ref().map(|n| &n.inventory),
                    );
                    let d = files::descriptors(entries);
                    if d.skipped_long > 0 && cloud.as_ref().is_none_or(|o| o.virtual_files) {
                        return Err(format!(
                            "{} paths are too long for FileGroupDescriptorW",
                            d.skipped_long
                        ));
                    }
                    fixed.push((f.preferred_drop_effect, DROPEFFECT_COPY.0.to_le_bytes().to_vec()));
                    virtual_files = Some((d, contents.clone()));
                }
                None => {
                    let paths = files::top_local_paths(entries);
                    if paths.is_empty() {
                        return Err("no local paths".into());
                    }
                    fixed.push((u32::from(CF_HDROP.0), files::build_hdrop(&paths)));
                    effects = to_effect(spec.ops);
                }
            },
            other => fixed.extend(prepare_items(std::slice::from_ref(other))),
        }
    }
    if fixed.is_empty() && virtual_files.is_none() && spec.native.is_none() {
        return Err("nothing to drag".into());
    }
    let (descriptors, contents, with_files) = match virtual_files {
        Some((d, c)) => (d, c, true),
        None => (
            files::descriptors(&[]),
            Arc::new(NoContents) as Arc<dyn FileContents>,
            false,
        ),
    };
    let native = spec
        .native
        .clone()
        .map(super::content::target::build_drag)
        .transpose()
        .map_err(|_| "native drag inventory unavailable".to_string())?;
    let (data, shared) = vfile::build_inner(
        vfile::Spec {
            cloud,
            descriptors,
            fixed,
            contents,
            cf_descriptor: f.file_descriptor,
            cf_contents: f.file_contents,
        },
        with_files,
        native,
    );
    shared.host_drag_images();
    Ok((data, effects))
}

// ───────────── COM 对象 ─────────────

#[implement(IDropSource, IDropSourceNotify)]
struct DropSource {
    shared: Arc<Shared>,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        let trace = ReadTrace::new(
            read_stage::CONTINUE_DRAG,
            ReadContext {
                aux: grfkeystate.0 | (u32::from(fescapepressed.as_bool()) << 31),
                ..ReadContext::current()
            },
        );
        let result = trace.scope(|| {
            if self.shared.lock().cancel || fescapepressed.as_bool() {
                return DRAGDROP_S_CANCEL;
            }
            if grfkeystate.0 & MK_LBUTTON.0 == 0 {
                return DRAGDROP_S_DROP;
            }
            S_OK
        });
        trace.result(result.0);
        result
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        let trace = ReadTrace::new(
            read_stage::FEEDBACK,
            ReadContext {
                aux: effect.0,
                ..ReadContext::current()
            },
        );
        let result = trace.scope(|| drag_feedback(effect));
        trace.result(result.0);
        result
    }
}

// Read-only OLE target notifications. No target HWND, title or caller identity is logged.
impl IDropSourceNotify_Impl for DropSource_Impl {
    fn DragEnterTarget(&self, _hwndtarget: HWND) -> windows::core::Result<()> {
        let trace = ReadTrace::new(read_stage::TARGET_ENTER, ReadContext::current());
        trace.result(0);
        Ok(())
    }
    fn DragLeaveTarget(&self) -> windows::core::Result<()> {
        let trace = ReadTrace::new(read_stage::TARGET_LEAVE, ReadContext::current());
        trace.result(0);
        Ok(())
    }
}

fn replace_live_image(
    operation: DragOperation,
    image: oink_foundation::dnd::DragImage,
    pixels: super::image::Rgba,
    point: POINT,
) {
    use windows::core::Interface;
    let live = LOCAL.with(|l| {
        let mut local = l.borrow_mut();
        let live = local
            .as_mut()?
            .live_drag
            .as_mut()
            .filter(|live| live.operation == operation && !live.thumbnail_applied)?;
        // Claim before COM calls, which can reenter this STA and dispatch jobs.
        live.thumbnail_applied = true;
        Some((live.data.clone(), live.helper.clone()))
    });
    let Some((data, old)) = live else { return };
    // The manager's destination interface refreshes the same data object
    // after InitializeFromBitmap. No new DoDragDrop/Down or focus handoff.
    // SAFETY: all interfaces and window queries belong to the drag STA.
    unsafe {
        if let Some(old) = old
            .as_ref()
            .and_then(|h| h.cast::<windows::Win32::UI::Shell::IDropTargetHelper>().ok())
        {
            let _ = old.DragLeave();
        }
        let Ok(Some(helper)) = super::drag_image::initialize_prepared(&data, &[], Some(&image), Some(&pixels), old)
        else {
            return;
        };
        let Ok(refresh) = helper.cast::<windows::Win32::UI::Shell::IDropTargetHelper>() else {
            return;
        };
        // Publishing the new DragWindow retires any previous process's
        // cached image window through this hosted IDataObject's ownership rule.
        let window = windows::Win32::UI::WindowsAndMessaging::GetAncestor(
            WindowFromPoint(point),
            windows::Win32::UI::WindowsAndMessaging::GA_ROOT,
        );
        if refresh.DragEnter(window, &data, &point, DROPEFFECT_COPY).is_err() {
            return;
        }
        let _ = refresh.DragOver(&point, DROPEFFECT_COPY);
        let _ = refresh.Show(true);
        let image_window = super::drag_image::drag_window(&data);
        LOCAL.with(|l| {
            if let Some(live) = l
                .borrow_mut()
                .as_mut()
                .and_then(|l| l.live_drag.as_mut())
                .filter(|live| live.operation == operation)
            {
                live.helper = Some(helper);
                live.refresh = Some(refresh);
                live.feedback_entered = true;
                live.image_window = image_window;
            }
        });
    }
}
fn drag_feedback(effect: DROPEFFECT) -> HRESULT {
    let mut point = POINT::default();
    // SAFETY: OLE callback reads the current physical cursor without moving it.
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        feedback_at(point, effect);
    }
    DRAGDROP_S_USEDEFAULTCURSORS
}
fn feedback_at(point: POINT, effect: DROPEFFECT) {
    let feedback = LOCAL.with(|l| {
        let mut local = l.borrow_mut();
        let live = local.as_mut()?.live_drag.as_mut()?;
        // A new destination owns the published image after it enters. The
        // previous source window has been retired; never refresh that context
        // again or ask it to recreate a second image.
        if let Some(window) = live.image_window {
            // SAFETY: read-only liveness check of our retained Shell image HWND.
            if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(window)) }.as_bool() {
                live.feedback_entered = false;
            }
        }
        live.feedback_entered.then(|| live.refresh.clone()).flatten()
    });
    if let Some(refresh) = feedback {
        // Only the replacement manager needs source feedback. The initial image
        // belongs to the destination. Movement only updates position; it never
        // initializes a bitmap, enters a new context or forces Show again.
        // SAFETY: retained helper and callback point belong to the OLE STA.
        unsafe {
            let _ = refresh.DragOver(&point, effect);
        }
    }
}

#[implement(IDropTarget)]
struct Catcher {
    shared: Arc<Shared>,
}

/// 接收窗口接受时返回的效果：源允许复制就复制，否则取源允许的第一个。
fn accept_effect(allowed: DROPEFFECT) -> DROPEFFECT {
    if allowed.0 & DROPEFFECT_COPY.0 != 0 {
        DROPEFFECT_COPY
    } else {
        DROPEFFECT_NONE
    }
}

impl IDropTarget_Impl for Catcher_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let trace = ReadTrace::new(read_stage::CATCHER_ENTER, ReadContext::current());
        let result = trace.scope(|| {
            // SAFETY: OLE 保证 pdweffect 在调用期间有效。
            let allowed = unsafe { pdweffect.as_ref() }.copied().unwrap_or(DROPEFFECT_NONE);
            let own = self.shared.lock().active;
            let effect = if own { DROPEFFECT_NONE } else { accept_effect(allowed) };
            // SAFETY: 同上。
            if let Some(p) = unsafe { pdweffect.as_mut() } {
                *p = effect;
            }
            if own || effect == DROPEFFECT_NONE {
                return Ok(());
            }
            let operation = {
                let mut st = self.shared.lock();
                st.source_present = true;
                let first = st.probing && !st.reported;
                if first {
                    st.reported = true;
                    let operation = DragOperation::new();
                    st.source_operation = Some(operation);
                    st.probe_operation = Some(operation);
                    Some(operation)
                } else {
                    None
                }
            };
            let Some(operation) = operation else {
                return Ok(());
            };
            let Some(data) = pdataobj.as_ref() else {
                return Ok(());
            };
            let started = Instant::now();
            let (dnd_formats, complete) = offered_formats(data);
            tracing::debug!(
                dnd_formats,
                dnd_formats_complete = complete,
                "native drag offered formats"
            );
            // Capture/resize/encode on this drag STA, outside the shared-state lock:
            // the hook and routing paths must not wait for image processing.
            let source_image = super::drag_image::capture(data);
            self.shared.lock().source_image = source_image;
            let id = oink_foundation::content::OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([0; 32]),
                    seq: 0,
                },
                generation: operation.sequence,
            };
            let drops = read_drag_paths(data);
            let Ok(mut inventory) = super::content::inventory(data, id) else {
                return Ok(());
            };
            for item in &mut inventory.items {
                item.virtual_files = item.file.is_some() && drops.is_empty();
            }
            representation_trace::inventory(&inventory, operation, false);
            let Ok(source) =
                crate::content::Deferred::new(inventory.clone(), Arc::new(super::content::WindowsTranslator))
            else {
                return Ok(());
            };
            LOCAL.with(|local| {
                if let Some(local) = local.borrow_mut().as_mut() {
                    local.native_source = Some((operation, data.clone()));
                }
            });
            let hover_shared = Arc::downgrade(&self.shared);
            let hover_inventory = inventory.clone();
            source.set_hover_reader(Arc::new(move |id| {
                use crate::model::Unavailable;
                let shared = hover_shared
                    .upgrade()
                    .ok_or_else(|| crate::content::unavailable(Unavailable::Cancelled))?;
                let (jobs, catcher) = shared
                    .native_jobs
                    .get()
                    .ok_or_else(|| crate::content::unavailable(Unavailable::Cancelled))?;
                let rep = crate::content::find(&hover_inventory, id)
                    .cloned()
                    .ok_or_else(|| crate::content::unavailable(Unavailable::NoSuchItem))?;
                let (tx, rx) = mpsc::sync_channel(1);
                jobs.send(Box::new(move || {
                    // Clone in this COM apartment, then release the RefCell borrow
                    // before GetData, which can reenter the native message loop.
                    let data = LOCAL.with(|local| {
                        local.borrow().as_ref().and_then(|local| {
                            local
                                .native_source
                                .as_ref()
                                .filter(|(op, _)| *op == operation)
                                .map(|(_, data)| data.clone())
                        })
                    });
                    let result = data
                        .ok_or_else(|| crate::content::unavailable(Unavailable::Superseded))
                        .and_then(|data| super::content::read_representation(&data, &rep))
                        .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as crate::FileReader);
                    let _ = tx.send(result);
                }))
                .map_err(|_| crate::content::unavailable(Unavailable::Cancelled))?;
                // SAFETY: the drag STA owns catcher; only an owned job crosses threads.
                unsafe { PostMessageW(Some(HWND(*catcher as _)), WM_APP_JOB, WPARAM(0), LPARAM(0)) }
                    .map_err(|_| crate::content::unavailable(Unavailable::Cancelled))?;
                rx.recv_timeout(Duration::from_secs(3))
                    .map_err(|_| crate::content::unavailable(Unavailable::NoProgress))?
            }));
            {
                let mut st = self.shared.lock();
                if let Some((_, old)) = st.source_data.replace((operation, source.clone())) {
                    old.fail(crate::model::Unavailable::Superseded);
                }
            }
            let virtual_files = if drops.is_empty() {
                match super::content::virtual_files::Virtual::metadata(data) {
                    Ok(files) => files,
                    Err(_) => {
                        source.fail(crate::model::Unavailable::ReadFailed);
                        return Ok(());
                    }
                }
            } else {
                None
            };
            self.shared.lock().source_virtual = virtual_files.clone();
            let items = crate::content::items(&inventory);
            let report_end = std::time::Instant::now();
            crate::dnd::preparation_timing(crate::dnd::PreparationStage::SourceRead, started, report_end);
            tracing::debug!(
                dnd_phase = 1_u64,
                elapsed_us = report_end.saturating_duration_since(started).as_micros() as u64,
                "native drag formats, artwork and content read"
            );
            let ops = from_effect(allowed);
            let shared = self.shared.clone();
            tracing::debug!(
                files = drops.len(),
                others = items.len(),
                "system drag entered the edge catcher"
            );
            // Stat selected roots off the OLE thread; descendants remain unvisited.
            let spawned = std::thread::Builder::new().name("oink-dnd-read".into()).spawn(move || {
                if let Some(files) = virtual_files {
                    if !source.attach_files(files) {
                        return;
                    }
                } else if !drops.is_empty() {
                    let roots = drops
                        .into_iter()
                        .enumerate()
                        .map(|(index, path)| (crate::model::NodeId(index as u64 + 1), path))
                        .collect();
                    let Ok(files) = crate::content::namespace::Local::new(roots) else {
                        return;
                    };
                    if !source.attach_files(files) {
                        return;
                    }
                }
                use crate::content::Provider;
                let items = crate::content::items(&source.inventory());
                if items.is_empty() {
                    tracing::info!("system drag has no content Oink can carry: not handing it over");
                    return;
                }
                let current = {
                    let st = shared.lock();
                    st.probing && st.source_operation == Some(operation)
                };
                if current {
                    shared.emit(PlatformDndEvent::Detected { operation, items, ops });
                }
            });
            if let Err(e) = spawned {
                tracing::warn!(%e, "drag and drop: cannot spawn the reader thread");
            }
            Ok(())
        });
        trace.result(result.as_ref().err().map_or(0, |e: &windows::core::Error| e.code().0));
        result
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let trace = ReadTrace::new(read_stage::CATCHER_OVER, ReadContext::current());
        let result = trace.scope(|| {
            let own = self.shared.lock().active;
            // SAFETY: OLE 保证 pdweffect 在调用期间有效。
            if let Some(p) = unsafe { pdweffect.as_mut() } {
                *p = if own { DROPEFFECT_NONE } else { accept_effect(*p) };
            }
            Ok(())
        });
        trace.result(result.as_ref().err().map_or(0, |e: &windows::core::Error| e.code().0));
        result
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        let trace = ReadTrace::new(read_stage::TARGET_LEAVE, ReadContext::current());
        let result = trace.scope(|| {
            self.shared.source_ended(false, 3);
            Ok(())
        });
        trace.result(result.as_ref().err().map_or(0, |e: &windows::core::Error| e.code().0));
        result
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let trace = ReadTrace::new(read_stage::CATCHER_DROP, ReadContext::current());
        let result = trace.scope(|| {
            let accept = {
                let mut st = self.shared.lock();
                let accept = st.releasing;
                if accept {
                    st.released = true;
                    self.shared.cv.notify_all();
                }
                accept
            };
            let accept = if accept {
                let (source, root, virtual_files) = {
                    let st = self.shared.lock();
                    (
                        st.source_data.as_ref().map(|(_, p)| p.clone()),
                        st.content_root.clone(),
                        st.source_virtual.clone(),
                    )
                };
                match (source, root, pdataobj.as_ref()) {
                    (Some(source), Some(root), Some(data)) => {
                        let id = crate::content::Provider::inventory(source.as_ref()).id;
                        let captured = virtual_files
                            .map_or(Ok(()), |files| files.snapshot(data, &root))
                            .and_then(|()| super::content::snapshot(data, &root, id));
                        match captured {
                            Ok(snapshot) => source.complete(snapshot),
                            Err(error) => {
                                source.fail(crate::content::reason(&error));
                                false
                            }
                        }
                    }
                    (Some(source), _, _) => {
                        source.fail(crate::model::Unavailable::ReadFailed);
                        false
                    }
                    (None, _, _) => true,
                }
            } else {
                false
            };
            self.shared.lock().released = accept;
            self.shared.source_ended(accept, if accept { 2 } else { 4 });
            // SAFETY: OLE 保证 pdweffect 在调用期间有效。
            if let Some(p) = unsafe { pdweffect.as_mut() } {
                *p = if accept { accept_effect(*p) } else { DROPEFFECT_NONE };
            }
            if accept {
                tracing::debug!("source drag dropped on the catcher");
            } else {
                tracing::info!("drag dropped on the edge catcher while probing: rejected");
            }
            Ok(())
        });
        trace.result(result.as_ref().err().map_or(0, |e: &windows::core::Error| e.code().0));
        result
    }
}

/// Enumerate names by matching fixed registered IDs, without requesting any extra
/// content. Registered custom names, paths and URLs never enter the diagnostic.
fn offered_formats(data: &IDataObject) -> (u64, bool) {
    let f = formats();
    // SAFETY: fixed, NUL-terminated format names; enumeration owns each ptd and
    // releases it with CoTaskMemFree. No returned payload or arbitrary name is read.
    unsafe {
        let known = [
            u32::from(CF_UNICODETEXT.0),
            f.html,
            f.rtf,
            f.png,
            u32::from(CF_DIB.0),
            u32::from(CF_DIBV5.0),
            u32::from(CF_HDROP.0),
            f.file_descriptor,
            f.file_contents,
            RegisterClipboardFormatW(w!("UniformResourceLocatorW")),
            RegisterClipboardFormatW(w!("UniformResourceLocator")),
            RegisterClipboardFormatW(w!("DragImageBits")),
        ];
        let Ok(enumerator) = data.EnumFormatEtc(DATADIR_GET.0 as u32) else {
            return (0, false);
        };
        let mut mask = 0_u64;
        // Bound a malformed source's enumerator. The completion bit distinguishes
        // partial inventories from a genuine absence of the known formats.
        for _ in 0..256 {
            let mut entry = [FORMATETC::default()];
            let mut fetched = 0;
            let result = enumerator.Next(&mut entry, Some(&mut fetched));
            if !entry[0].ptd.is_null() {
                CoTaskMemFree(Some(entry[0].ptd.cast()));
            }
            if result.is_err() {
                return (mask, false);
            }
            if fetched == 0 {
                return (mask, true);
            }
            if let Some(bit) = known.iter().position(|&cf| cf == u32::from(entry[0].cfFormat)) {
                mask |= 1 << bit;
            }
        }
        (mask, false)
    }
}

/// 读拖拽的数据对象：`CF_HDROP` 的顶层路径（由调用方展开），以及文本、PNG。
fn read_drag_paths(data: &IDataObject) -> Vec<std::path::PathBuf> {
    let get = |cf: u32| -> Option<Vec<u8>> {
        let fe = FORMATETC {
            cfFormat: cf as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        // SAFETY: 标准的 GetData / ReleaseStgMedium；只读取 HGLOBAL。
        unsafe {
            let mut medium = data.GetData(&fe).ok()?;
            let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                vfile::hglobal_bytes(medium.u.hGlobal)
            } else {
                None
            };
            ReleaseStgMedium(&mut medium);
            bytes
        }
    };
    get(u32::from(CF_HDROP.0))
        .map(|b| files::parse_hdrop(&b, |a| String::from_utf8_lossy(a).into_owned()))
        .unwrap_or_default()
}

#[cfg(all(test, debug_assertions))]
mod artwork_desktop_tests;

#[cfg(all(test, debug_assertions))]
mod diagnostic_tests {
    use super::*;
    use windows::core::Interface;
    #[test]
    fn continuation_diagnostics_preserve_release_cancel_and_target_notifications() {
        let shared = Arc::new(Shared {
            native_jobs: OnceLock::new(),
            sink: OnceLock::new(),
            inject: OnceLock::new(),
            source_press: OnceLock::new(),
            source_start: OnceLock::new(),
            st: Mutex::new(St::default()),
            cv: Condvar::new(),
        });
        let source: IDropSource = DropSource { shared: shared.clone() }.into();
        // SAFETY: only calls private Rust COM objects; no windows or input are used.
        unsafe {
            assert_eq!(source.QueryContinueDrag(false, MK_LBUTTON), S_OK);
            assert_eq!(source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)), DRAGDROP_S_DROP);
            assert_eq!(source.QueryContinueDrag(true, MK_LBUTTON), DRAGDROP_S_CANCEL);
            shared.lock().cancel = true;
            assert_eq!(source.QueryContinueDrag(false, MK_LBUTTON), DRAGDROP_S_CANCEL);
            let notify: IDropSourceNotify = source.cast().unwrap();
            notify.DragEnterTarget(HWND::default()).unwrap();
            notify.DragLeaveTarget().unwrap();
        }
    }
}
