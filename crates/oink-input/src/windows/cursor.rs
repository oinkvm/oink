//! Windows cursor hiding. The cover window owns its visibility and cursor shape.
//! Departure is an acknowledged operation: request a cover over the source and park
//! points, return from the input callback, then park only after the cover thread has
//! hidden the global cursor. A timeout never authorizes a visible warp. All commands
//! are asynchronous and obsolete acknowledgements are discarded by generation.
//!
//! The optional system-cursor strategy retains its crash-recovery marker. The default
//! window strategy changes no global cursor scheme and dies with the process.

use oink_foundation::snapshot::Snapshot;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
#[cfg(debug_assertions)]
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorInfo,
    GetCursorPos, GetMessageW, HCURSOR, HWND_TOPMOST, IsWindowVisible, LWA_ALPHA, MA_NOACTIVATE, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SPI_SETCURSORS, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetCursor, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    SystemParametersInfoW, WM_APP, WM_CLOSE, WM_DESTROY, WM_MOUSEACTIVATE, WM_SETCURSOR, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WindowFromPoint,
};
#[cfg(debug_assertions)]
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, CreateCursor, DestroyCursor, HICON, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO,
    OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, SYSTEM_CURSOR_ID,
    SetSystemCursor,
};
use windows::core::w;

use super::dpi::ThreadDpiGuard;

/// 遮罩窗口边长（物理像素）。
const COVER: i32 = 128;
const WM_COVER_APPLY: u32 = WM_APP + 1;

#[derive(Clone, Copy, Debug)]
struct CoverRequest {
    generation: u64,
    reply: usize,
    from: POINT,
    park: POINT,
    visible: bool,
}
thread_local! {
    static COVER_REQUEST: RefCell<Option<Arc<Snapshot<Option<CoverRequest>>>>> = const { RefCell::new(None) };
    static COVER_APPLIED: Cell<Option<CoverRequest>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    Window,
    #[cfg(debug_assertions)]
    SystemCursors,
    #[cfg(debug_assertions)]
    None,
}

impl Strategy {
    pub(crate) fn from_env() -> Self {
        #[cfg(not(debug_assertions))]
        {
            Self::Window
        }
        #[cfg(debug_assertions)]
        match oink_foundation::debug_env!("OINK_WIN_CURSOR_HIDE").as_deref() {
            #[cfg(debug_assertions)]
            Ok("system") => Self::SystemCursors,
            #[cfg(debug_assertions)]
            Ok("none") => Self::None,
            _ => Self::Window,
        }
    }
}

/// 只在输入线程上使用。
#[derive(Debug)]
pub(crate) struct Hider {
    strategy: Strategy,
    cover: Option<Cover>,
    hidden: bool,
    /// 遮罩窗口停在哪里（`window` 方案隐藏时的停靠点，遮罩以它为中心）。
    covered: Option<POINT>,
    pending_park: Option<CoverRequest>,
    generation: u64,
    reply: HWND,
}

/// 遮罩窗口与它所属的专用线程。
#[derive(Debug)]
struct Cover {
    hwnd: HWND,
    thread: Option<JoinHandle<()>>,
    request: Arc<Snapshot<Option<CoverRequest>>>,
}

/// `system` 方案当前是否处于隐藏状态（控制台事件处理函数读取）。
static SYSTEM_HIDDEN: AtomicBool = AtomicBool::new(false);

fn marker_path() -> std::path::PathBuf {
    oink_foundation::paths::data_dir().join("win-cursor-hidden")
}

/// backend 创建时调用：上次运行在 `system` 方案隐藏光标后异常退出，则恢复系统光标。
pub(crate) fn recover_if_needed() -> bool {
    let path = marker_path();
    if !path.exists() {
        return false;
    }
    tracing::warn!("发现光标隐藏标记 {}：上次运行未正常恢复光标，现在恢复", path.display());
    restore_system_cursors();
    let _ = std::fs::remove_file(&path);
    true
}

fn restore_system_cursors() {
    // SAFETY: SPI_SETCURSORS 不使用 pvParam；从注册表重新加载用户的光标方案。
    let r = unsafe { SystemParametersInfoW(SPI_SETCURSORS, 0, None, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) };
    if let Err(e) = r {
        tracing::error!("SPI_SETCURSORS 失败，光标可能仍不可见：{e}");
    }
    SYSTEM_HIDDEN.store(false, Ordering::SeqCst);
}

#[cfg(debug_assertions)]
unsafe extern "system" fn console_ctrl(_ctrl: u32) -> windows::core::BOOL {
    if SYSTEM_HIDDEN.load(Ordering::SeqCst) {
        restore_system_cursors();
        let _ = std::fs::remove_file(marker_path());
    }
    windows::core::BOOL(0) // 交给下一个处理函数（默认会结束进程）
}

#[cfg(debug_assertions)]
const SYSTEM_CURSORS: [SYSTEM_CURSOR_ID; 14] = [
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
    OCR_HELP,
];

fn cover_request() -> Option<CoverRequest> {
    COVER_REQUEST.with(|slot| slot.borrow().as_ref().and_then(|slot| *slot.load()))
}

type CoverObservation = Option<(u64, Option<bool>)>;
thread_local! {
    // WM_SETCURSOR can repeat for every motion. Ordinary recording retains only
    // the first observation or a result change for each cover generation.
    static COVER_DIAGNOSTICS: Cell<[CoverObservation; 2]> = const { Cell::new([None; 2]) };
}
fn cover_trace(kind: crate::native_lifecycle::Kind, r: CoverRequest, success: Option<bool>) {
    if crate::native_lifecycle::lifecycle_enabled() {
        {
            let index = match kind {
                crate::native_lifecycle::Kind::CoverCursor => Some(0),
                crate::native_lifecycle::Kind::CoverReady => Some(1),
                _ => None,
            };
            if let Some(index) = index {
                let duplicate = COVER_DIAGNOSTICS.with(|slot| {
                    let mut previous = slot.get();
                    let value = Some((r.generation, success));
                    if previous[index] == value {
                        return true;
                    }
                    previous[index] = value;
                    slot.set(previous);
                    false
                });
                if duplicate {
                    return;
                }
            }
        }
        crate::native_lifecycle::emit(
            kind,
            crate::native_lifecycle::Data {
                extra: r.generation,
                point: Some([r.from.x, r.from.y]),
                park: Some([r.park.x, r.park.y]),
                hidden: Some(r.visible),
                success,
                ..crate::native_lifecycle::Data::default()
            },
        );
    }
}

fn cursor_hidden() -> bool {
    #[cfg(all(test, debug_assertions))]
    if let Some(hidden) = test_native::hidden() {
        return hidden;
    }
    let mut info = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: initialized output storage. Failure is never evidence that hiding worked.
    unsafe { GetCursorInfo(&mut info).is_ok() && info.flags.0 & CURSOR_SHOWING.0 == 0 }
}

pub(crate) fn drag_anchor_ready(anchor: oink_foundation::dnd::DragAnchor) -> bool {
    let hwnd = HWND(anchor.surface as *mut _);
    let point = POINT {
        x: anchor.at.x as i32,
        y: anchor.at.y as i32,
    };
    // OLE's source GiveFeedback owns the cursor, and may replace the catcher's
    // SetCursor(NULL). The catcher authorizes a move to its outer bottom pixel;
    // it must cover both the current cursor and that park point until return.
    // Requiring a hidden cursor here rejects a valid held OLE session. Keep the
    // hide acknowledgement for the ordinary cover/park path below.
    // SAFETY: read-only queries; a stale or covered catcher still fails closed.
    let ready = !hwnd.is_invalid()
        && anchor_surface_ready(hwnd, point)
        && anchor_surface_ready(hwnd, super::hook::cursor_pos());
    tracing::debug!(
        generation = anchor.generation,
        ready,
        cursor_hidden = cursor_hidden(),
        "native drag anchor checked"
    );
    ready
}

fn anchor_surface_ready(hwnd: HWND, point: POINT) -> bool {
    #[cfg(all(test, debug_assertions))]
    if let Some(at) = test_native::position() {
        return hwnd.0 as usize == 1 && point == at;
    }
    // SAFETY: read-only native window queries; tests replace only that boundary.
    unsafe { IsWindowVisible(hwnd).as_bool() && WindowFromPoint(point) == hwnd }
}

fn cover_ready(hwnd: HWND, r: CoverRequest) -> bool {
    #[cfg(all(test, debug_assertions))]
    if test_native::active() {
        return cursor_hidden();
    }
    // SAFETY: read-only hit test and visibility query on the live cover window.
    unsafe { IsWindowVisible(hwnd).as_bool() && WindowFromPoint(r.park) == hwnd && cursor_hidden() }
}

fn acknowledge_cover(hwnd: HWND, r: CoverRequest) -> bool {
    let ready = COVER_APPLIED.with(|applied| applied.get().is_some_and(|applied| applied.generation == r.generation))
        && cover_request().is_some_and(|latest| latest.visible && latest.generation == r.generation)
        && cover_ready(hwnd, r);
    cover_trace(crate::native_lifecycle::Kind::CoverReady, r, Some(ready));
    if ready {
        // SAFETY: fixed notification to the input-owner window; no synchronous call to its thread.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(r.reply as *mut _)),
                super::hook::WM_APP_COVER_READY,
                WPARAM(r.generation as usize),
                LPARAM(0),
            );
        }
    }
    ready
}

unsafe extern "system" fn cover_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COVER_APPLY => {
            if let Some(r) = cover_request() {
                // An old show/hide message can reenter WM_SETCURSOR. Do not acknowledge
                // the latest request until THIS generation's geometry is actually applied.
                COVER_APPLIED.with(|applied| applied.set(None));
                if r.visible {
                    let half = COVER / 2;
                    let (l, t) = (r.from.x.min(r.park.x) - half, r.from.y.min(r.park.y) - half);
                    let (right, bottom) = (r.from.x.max(r.park.x) + half, r.from.y.max(r.park.y) + half);
                    // SAFETY: this thread owns the cover. The input owner never waits for it.
                    let placed = unsafe {
                        SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            l,
                            t,
                            right - l,
                            bottom - t,
                            SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        )
                        .is_ok()
                    };
                    if placed {
                        COVER_APPLIED.with(|applied| applied.set(Some(r)));
                    }
                    cover_trace(crate::native_lifecycle::Kind::CoverApplied, r, Some(placed));
                    let mut p = POINT::default();
                    // SAFETY: only change the shape when this cover owns the point under the cursor.
                    unsafe {
                        if GetCursorPos(&mut p).is_ok() && WindowFromPoint(p) == hwnd {
                            SetCursor(None);
                        }
                    }
                    // The real WM_SETCURSOR can arrive after this message. It also acknowledges
                    // the same generation; an unacknowledged operation never moves to park.
                    if !acknowledge_cover(hwnd, r) {
                        // A stationary pointer need not receive WM_SETCURSOR merely because
                        // the cover was shown. Ask the input owner to refresh its SOURCE
                        // point after its callback returns, never move to park to trigger it.
                        // SAFETY: asynchronous message to the hook owner's live window.
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(r.reply as *mut _)),
                                super::hook::WM_APP_COVER_PLACED,
                                WPARAM(r.generation as usize),
                                LPARAM(0),
                            );
                        }
                    }
                } else {
                    // SAFETY: own window, called only after the input owner authorizes uncover.
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                    cover_trace(crate::native_lifecycle::Kind::UncoverCommitted, r, Some(true));
                    // SAFETY: asynchronous completion; receiver validates the generation.
                    unsafe {
                        let _ = PostMessageW(
                            Some(HWND(r.reply as *mut _)),
                            super::hook::WM_APP_COVER_HIDDEN,
                            WPARAM(r.generation as usize),
                            LPARAM(0),
                        );
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(r) = cover_request().filter(|r| r.visible) {
                // SAFETY: this is the cover's cursor query; its class cursor is null too.
                unsafe {
                    SetCursor(None);
                }
                cover_trace(crate::native_lifecycle::Kind::CoverCursor, r, Some(cursor_hidden()));
                acknowledge_cover(hwnd, r);
                LRESULT(1)
            } else {
                // SAFETY: a retired cover request must not hide a newly local cursor.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_CLOSE => {
            // SAFETY: this thread owns the window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: end this cover thread's message loop.
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: unhandled window messages use the standard procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

impl Cover {
    fn request(&self, request: CoverRequest) {
        self.request.publish(Some(request));
        cover_trace(
            if request.visible {
                crate::native_lifecycle::Kind::CoverRequest
            } else {
                crate::native_lifecycle::Kind::UncoverRequest
            },
            request,
            None,
        );
        #[cfg(all(test, debug_assertions))]
        if test_native::active() {
            return;
        }
        // SAFETY: only a wake message crosses threads; the latest complete request is in the snapshot.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_COVER_APPLY, WPARAM(0), LPARAM(0));
        }
    }
}

impl Hider {
    /// 在输入线程上创建（`window` 方案会启动遮罩窗口的专用线程，窗口先隐藏）。
    pub(crate) fn new(strategy: Strategy) -> Self {
        let mut h = Self {
            strategy,
            cover: None,
            hidden: false,
            covered: None,
            pending_park: None,
            generation: 0,
            reply: HWND::default(),
        };
        if strategy == Strategy::Window {
            h.cover = spawn_cover();
            if h.cover.is_none() {
                tracing::warn!("创建光标遮罩窗口失败，Remote 模式下光标将可见");
            }
        }
        #[cfg(debug_assertions)]
        if strategy == Strategy::SystemCursors {
            // SAFETY: 注册一个进程级的控制台事件处理函数（重复注册无害）。
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(console_ctrl), true);
            }
        }
        h
    }

    pub(crate) fn strategy(&self) -> Strategy {
        self.strategy
    }

    pub(crate) fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Start hiding without waiting inside the low-level hook. True means parking
    /// completed immediately (the explicit non-window strategies only).
    pub(crate) fn hide(&mut self, from: POINT, park: POINT, reply: HWND) -> bool {
        super::trace::action(crate::native_lifecycle::Kind::Hide, Some(park), 0, None);
        self.generation += 1;
        self.reply = reply;
        self.pending_park = None;
        match self.strategy {
            Strategy::Window => {
                let request = CoverRequest {
                    generation: self.generation,
                    from,
                    park,
                    reply: reply.0 as usize,
                    visible: true,
                };
                self.pending_park = Some(request);
                self.hidden = true;
                if let Some(cover) = &self.cover {
                    cover.request(request);
                }
                // Missing cover or missing acknowledgement leaves the cursor at the source.
                // Never turn failure to hide into permission to warp a visible cursor.
                false
            }
            #[cfg(debug_assertions)]
            Strategy::SystemCursors => {
                if !self.hidden {
                    let path = marker_path();
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if let Err(e) = std::fs::write(&path, format!("pid {}\n", std::process::id())) {
                        tracing::warn!("写光标恢复标记失败（{e}），不隐藏光标以免无法恢复");
                        warp(park);
                        return true;
                    }
                    SYSTEM_HIDDEN.store(true, Ordering::SeqCst);
                    hide_system_cursors();
                }
                warp(park);
                self.hidden = true;
                true
            }
            #[cfg(debug_assertions)]
            Strategy::None => {
                warp(park);
                self.hidden = true;
                true
            }
        }
    }

    pub(crate) fn refresh_shape(&mut self, generation: u64) {
        let Some(request) = self.pending_park.filter(|r| r.generation == generation) else {
            return;
        };
        let Some(cover) = &self.cover else { return };
        #[cfg(all(test, debug_assertions))]
        if test_native::active() {
            nudge(test_native::position().unwrap());
            return;
        }
        let mut p = POINT::default();
        // SAFETY: refresh only a point now owned by this cover, after native callback return.
        // A higher-level window blocking the cover never authorizes a centre warp.
        if unsafe { GetCursorPos(&mut p).is_ok() && WindowFromPoint(p) == cover.hwnd } {
            cover_trace(crate::native_lifecycle::Kind::CoverCursor, request, Some(false));
            nudge(p);
        }
    }

    pub(crate) fn park_pending(&self) -> bool {
        self.pending_park.is_some()
    }

    pub(crate) fn park_generation(&self) -> Option<u64> {
        self.pending_park.map(|request| request.generation)
    }

    pub(crate) fn cancel_park(&mut self) {
        self.pending_park = None;
    }

    /// Read-only permission for the hook-ordered native park input. The expanded
    /// cover stays in place until SendInput completes and placement is read back.
    pub(crate) fn park_target(&self, generation: u64) -> Option<POINT> {
        let request = self.pending_park.filter(|r| r.generation == generation)?;
        let cover = self.cover.as_ref()?;
        cover_ready(cover.hwnd, request).then_some(request.park)
    }

    /// Complete an already applied native move; never SetCursorPos here. Keeping
    /// pending_park until this point also prevents raw/game clipping the input
    /// to its old source before Windows has applied the new absolute position.
    pub(crate) fn finish_park(&mut self, generation: u64) -> Option<(POINT, POINT)> {
        let park = self.park_target(generation)?;
        if super::hook::cursor_pos() != park {
            return None;
        }
        let request = self.pending_park.take()?;
        self.covered = Some(park);
        self.cover.as_ref()?.request(CoverRequest { from: park, ..request });
        Some((request.from, park))
    }

    pub(crate) fn show(&mut self) {
        self.show_at(None);
    }

    pub(crate) fn show_at(&mut self, at: Option<POINT>) {
        super::trace::action(crate::native_lifecycle::Kind::Show, at, 0, None);
        self.cancel_park();
        if !self.hidden {
            return;
        }
        self.generation += 1;
        match self.strategy {
            Strategy::Window => {
                if let Some(cover) = &self.cover {
                    let point = at.or(self.covered).unwrap_or_default();
                    cover.request(CoverRequest {
                        generation: self.generation,
                        reply: self.reply.0 as usize,
                        from: point,
                        park: self.covered.unwrap_or(point),
                        visible: false,
                    });
                }
            }
            #[cfg(debug_assertions)]
            Strategy::SystemCursors => {
                restore_system_cursors();
                let _ = std::fs::remove_file(marker_path());
            }
            #[cfg(debug_assertions)]
            Strategy::None => {}
        }
        self.hidden = false;
    }

    pub(crate) fn show_moved_away(&mut self) {
        self.show_at(None);
    }

    /// Uncover completion, never a wait or a stale warp to the old entry point.
    pub(crate) fn finish_show(&mut self, generation: u64) {
        if self.hidden || generation != self.generation {
            return;
        }
        let Some(covered) = self.covered.take() else { return };
        let mut p = POINT::default();
        // SAFETY: read current position after the cover owner hid its window.
        let read = unsafe { GetCursorPos(&mut p) }.is_ok();
        if crate::native_lifecycle::lifecycle_enabled() {
            crate::native_lifecycle::emit(
                crate::native_lifecycle::Kind::Show,
                crate::native_lifecycle::Data {
                    cursor: read.then_some([p.x, p.y]),
                    hidden: Some(false),
                    extra: generation,
                    reason: 2,
                    success: Some(read),
                    ..Default::default()
                },
            );
        }
        if read && (p.x - covered.x).abs() <= COVER / 2 && (p.y - covered.y).abs() <= COVER / 2 {
            nudge(p);
        }
    }
}

impl Drop for Hider {
    fn drop(&mut self) {
        #[cfg(all(test, debug_assertions))]
        if test_native::active() {
            self.cover = None;
            return;
        }
        self.show();
        if let Some(mut c) = self.cover.take() {
            // SAFETY: 让遮罩线程销毁自己的窗口并退出消息循环。
            unsafe {
                let _ = PostMessageW(Some(c.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            if let Some(t) = c.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(debug_assertions)]
fn warp(p: POINT) -> bool {
    super::trace::action(crate::native_lifecycle::Kind::Park, Some(p), 0, None);
    let result = super::warp::place(p).is_ok();
    super::trace::action(crate::native_lifecycle::Kind::ParkCommitted, Some(p), 0, Some(result));
    result
}

/// 原地「移动」一下光标（让系统重新询问光标形状）。
fn nudge(p: POINT) {
    #[cfg(all(test, debug_assertions))]
    if test_native::refresh(p) {
        return;
    }
    let _ = super::warp::place(POINT { x: p.x + 1, y: p.y });
    let _ = super::warp::place(p);
}

/// 启动遮罩线程：创建（隐藏的）遮罩窗口，一直取消息，收到 `WM_CLOSE` 时销毁窗口并退出。
fn spawn_cover() -> Option<Cover> {
    let (tx, rx) = mpsc::channel::<Option<isize>>();
    let request = Arc::new(Snapshot::new(None));
    let requested = request.clone();
    let thread = std::thread::Builder::new()
        .name("oink-cursor-cover".into())
        .spawn(move || {
            let _dpi = ThreadDpiGuard::per_monitor_v2();
            COVER_REQUEST.with(|slot| *slot.borrow_mut() = Some(requested));
            let Some(hwnd) = create_cover() else {
                let _ = tx.send(None);
                return;
            };
            let _ = tx.send(Some(hwnd.0 as isize));
            let mut msg = MSG::default();
            // SAFETY: 本线程的标准消息循环。
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    DispatchMessageW(&msg);
                }
            }
        })
        .ok()?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(h)) => Some(Cover {
            hwnd: HWND(h as *mut core::ffi::c_void),
            thread: Some(thread),
            request,
        }),
        _ => None,
    }
}

fn create_cover() -> Option<HWND> {
    // SAFETY: 标准的窗口类注册与窗口创建。
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let class = w!("OinkCursorCover");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(cover_proc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            hCursor: HCURSOR::default(),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return None;
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            class,
            w!("Oink cursor cover"),
            WS_POPUP,
            0,
            0,
            COVER,
            COVER,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .ok()?;
        // alpha = 1：肉眼不可见，但仍参与命中测试（alpha = 0 的分层窗口不接收鼠标）
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
        Some(hwnd)
    }
}

#[cfg(debug_assertions)]
fn hide_system_cursors() {
    // 32×32 单色光标：AND 掩码全 1（保留背景）、XOR 掩码全 0（不改变）→ 完全透明
    let and_mask = [0xFFu8; 32 * 4];
    let xor_mask = [0u8; 32 * 4];
    // SAFETY: 掩码缓冲区大小为 32×32 位；SetSystemCursor 接管（并销毁）传入的光标句柄，所以每次传一个副本。
    unsafe {
        let Ok(blank) = CreateCursor(None, 0, 0, 32, 32, and_mask.as_ptr().cast(), xor_mask.as_ptr().cast()) else {
            tracing::warn!("CreateCursor 失败，无法隐藏系统光标");
            return;
        };
        for id in SYSTEM_CURSORS {
            if let Ok(copy) = CopyIcon(HICON(blank.0))
                && SetSystemCursor(HCURSOR(copy.0), id).is_err()
            {
                let _ = DestroyCursor(HCURSOR(copy.0));
            }
        }
        let _ = DestroyCursor(blank);
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) mod test_native {
    use super::*;
    use std::cell::RefCell;
    #[derive(Default)]
    struct Recording {
        position: POINT,
        showing: bool,
        moves: Vec<(POINT, bool)>,
        refreshes: Vec<POINT>,
    }
    thread_local! { static NATIVE: RefCell<Option<Recording>> = const { RefCell::new(None) }; }
    pub(super) fn refresh(p: POINT) -> bool {
        NATIVE.with(|n| {
            let mut n = n.borrow_mut();
            let Some(n) = n.as_mut() else { return false };
            n.refreshes.push(p);
            true
        })
    }
    pub(in crate::windows) fn hidden() -> Option<bool> {
        NATIVE.with(|n| n.borrow().as_ref().map(|n| !n.showing))
    }
    pub(in crate::windows) fn active() -> bool {
        NATIVE.with(|n| n.borrow().is_some())
    }
    pub(in crate::windows) fn warp(p: POINT) -> bool {
        NATIVE.with(|n| {
            let mut n = n.borrow_mut();
            let Some(n) = n.as_mut() else { return false };
            n.moves.push((p, n.showing));
            n.position = p;
            true
        })
    }
    pub(in crate::windows) fn position() -> Option<POINT> {
        NATIVE.with(|n| n.borrow().as_ref().map(|n| n.position))
    }
    pub(in crate::windows) fn set_showing(showing: bool) {
        NATIVE.with(|n| n.borrow_mut().as_mut().unwrap().showing = showing);
    }
    pub(in crate::windows) struct Guard;
    impl Guard {
        pub(in crate::windows) fn at(position: POINT) -> Self {
            super::super::warp::reset();
            NATIVE.with(|n| {
                *n.borrow_mut() = Some(Recording {
                    position,
                    showing: true,
                    ..Default::default()
                })
            });
            Self
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            NATIVE.with(|n| *n.borrow_mut() = None);
        }
    }
    pub(in crate::windows) fn hider() -> Hider {
        Hider {
            strategy: Strategy::Window,
            cover: Some(Cover {
                hwnd: HWND(std::ptr::dangling_mut()),
                thread: None,
                request: Arc::new(Snapshot::new(None)),
            }),
            hidden: false,
            covered: None,
            pending_park: None,
            generation: 0,
            reply: HWND::default(),
        }
    }
    fn moves() -> Vec<(POINT, bool)> {
        NATIVE.with(|n| n.borrow().as_ref().unwrap().moves.clone())
    }

    #[test]
    fn missing_hide_acknowledgement_never_parks_a_visible_cursor() {
        let from = POINT { x: 2094, y: 2156 };
        let park = POINT { x: 1920, y: 1080 };
        let _native = Guard::at(from);
        let mut hider = hider();
        hider.hide(from, park, HWND::default());
        assert!(moves().is_empty(), "requesting a cover is not a hide acknowledgement");
        assert_eq!(hider.finish_park(hider.generation), None);
        hider.refresh_shape(hider.generation);
        assert_eq!(position(), Some(from));
        assert_eq!(
            NATIVE.with(|n| n.borrow().as_ref().unwrap().refreshes.clone()),
            vec![from]
        );
        assert!(
            moves().iter().all(|(_, showing)| !showing),
            "park occurred while cursor was visible: {:?}",
            moves()
        );
        set_showing(false);
        assert_eq!(hider.park_target(hider.generation), Some(park));
        assert_eq!(hider.finish_park(hider.generation), None, "ACK must not pre-warp");
        assert!(moves().is_empty());
        warp(park); // Native INPUT commits after the accepting hook returns.
        assert_eq!(hider.finish_park(hider.generation), Some((from, park)));
        assert_eq!(moves(), vec![(park, false)]);
        assert_eq!(
            hider.finish_park(hider.generation),
            None,
            "duplicate acknowledgement is inert"
        );
    }

    #[test]
    fn cancelled_and_stale_hide_acknowledgements_cannot_park_after_return() {
        let from = POINT { x: 2094, y: 2156 };
        let park = POINT { x: 1920, y: 1080 };
        let entry = POINT { x: 1809, y: 2158 };
        let _native = Guard::at(from);
        let mut hider = hider();
        hider.hide(from, park, HWND::default());
        let old = hider.generation;
        hider.cancel_park(); // Local restore can arrive before the cover thread gets scheduled.
        set_showing(false);
        warp(entry); // Model native restore marker commit, while still hidden.
        hider.show_at(Some(entry));
        set_showing(true);
        assert_eq!(hider.finish_park(old), None);
        assert_eq!(position(), Some(entry));
        hider.hide(entry, park, HWND::default());
        set_showing(false);
        assert_eq!(
            hider.finish_park(old),
            None,
            "old ack must not complete a new departure"
        );
        assert_eq!(hider.park_target(hider.generation), Some(park));
        assert_eq!(hider.finish_park(hider.generation), None);
        warp(park);
        assert_eq!(hider.finish_park(hider.generation), Some((entry, park)));
        assert_eq!(moves(), vec![(entry, false), (park, false)]);
    }
}
