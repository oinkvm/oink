//! 监听显示器配置变化：专用线程上的隐藏顶层窗口（消息窗口 `HWND_MESSAGE` 收不到广播消息）。
//!
//! 触发：`WM_DISPLAYCHANGE`（分辨率、插拔、排列）、`WM_DPICHANGED`（缩放）、`WM_SETTINGCHANGE`、
//! `WM_DEVICECHANGE(DBT_DEVNODES_CHANGED)`。每次触发重置一个 300 ms 定时器（合并抖动）；定时器到期后
//! 重新枚举，列表有变化（或收到过 `WM_DISPLAYCHANGE`/`WM_DPICHANGED`）才调用回调。

use std::cell::RefCell;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use oink_foundation::display::DisplayInfo;
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetTimer, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_DEVICECHANGE,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::w;

use super::dpi::ThreadDpiGuard;
use crate::{ChangeHandler, DisplayError};

const DEBOUNCE_MS: u32 = 300;
const TIMER_ID: usize = 1;

pub(crate) type Handlers = Arc<Mutex<Vec<ChangeHandler>>>;

struct State {
    handlers: Handlers,
    last: Vec<DisplayInfo>,
    force: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// 监听线程的句柄。Drop 时关闭窗口，线程随之退出。
#[derive(Debug)]
pub(crate) struct Watcher {
    hwnd: isize,
}

impl Watcher {
    pub(crate) fn spawn(handlers: Handlers) -> Result<Self, DisplayError> {
        let (tx, rx) = mpsc::channel::<Result<isize, String>>();
        std::thread::Builder::new()
            .name("oink-display-watch".into())
            .spawn(move || run(handlers, tx))
            .map_err(|e| DisplayError::Platform(format!("创建监听线程失败：{e}")))?;
        let hwnd = rx
            .recv()
            .map_err(|_| DisplayError::Platform("监听线程意外退出".into()))?
            .map_err(DisplayError::Platform)?;
        Ok(Self { hwnd })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: 窗口属于监听线程；PostMessage 可以跨线程调用，窗口已销毁时只是失败。
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn run(handlers: Handlers, tx: mpsc::Sender<Result<isize, String>>) {
    let _dpi = ThreadDpiGuard::per_monitor_v2();
    // SAFETY: 标准的窗口类注册、窗口创建与消息循环；窗口过程只访问本线程的 thread_local。
    unsafe {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(format!("GetModuleHandleW: {e}")));
                return;
            }
        };
        let class = w!("OinkDisplayWatcher");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            let _ = tx.send(Err(format!("RegisterClassExW: {:?}", GetLastError())));
            return;
        }
        let last = super::enumerate::displays().unwrap_or_default();
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                handlers,
                last,
                force: false,
            })
        });
        // 隐藏的顶层窗口（从不显示）：只有顶层窗口能收到 WM_DISPLAYCHANGE 等广播
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WINDOW_EX_STYLE(0),
            class,
            w!("Oink display watcher"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(format!("CreateWindowExW: {e}")));
                return;
            }
        };
        let _ = tx.send(Ok(hwnd.0 as isize));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
    STATE.with(|s| s.borrow_mut().take());
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE | WM_DEVICECHANGE => {
            if msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED {
                STATE.with(|s| {
                    if let Some(st) = s.borrow_mut().as_mut() {
                        st.force = true;
                    }
                });
            }
            tracing::trace!("display watcher: message 0x{msg:04x}");
            // SAFETY: hwnd 是本线程的窗口；重复 SetTimer 会重置同一个定时器（去抖）。
            unsafe {
                SetTimer(Some(hwnd), TIMER_ID, DEBOUNCE_MS, None);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            // SAFETY: 同上。
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
            }
            fire();
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: 销毁本线程自己的窗口。
            unsafe {
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

fn fire() {
    let now = match super::enumerate::displays() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("显示器变化后重新枚举失败：{e}");
            return;
        }
    };
    let handlers = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.as_mut()?;
        let changed = st.force || st.last != now;
        st.force = false;
        st.last = now;
        changed.then(|| st.handlers.lock().map(|h| h.clone()).unwrap_or_default())
    });
    if let Some(handlers) = handlers {
        tracing::debug!("显示器配置变化，通知 {} 个回调", handlers.len());
        for h in handlers {
            h();
        }
    }
}
