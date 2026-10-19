//! Windows：每块显示器一个不透明的置顶弹出窗口，按脏矩形把图块合成后画上去。
//!
//! - 窗口：`WS_POPUP`，`WS_EX_TOPMOST | WS_EX_TOOLWINDOW`，正好盖住一块显示器（坐标是 PMv2 物理像素，与
//!   `DisplayInfo` 一致）。不透明、不是分层窗口：整屏的分层窗口每次更新都要上传整幅位图，普通窗口可以
//!   只画变化的那一小块。
//! - **键盘焦点**：打开时第一块显示器的窗口成为前台窗口（[`take_foreground`]），之后点哪一块哪一块被激活
//!   ——像普通的全屏游戏一样，按键由系统正常投递给它（`WM_KEYDOWN` 等），窗口过程里除 Esc 外什么也不做，
//!   所以进行期间键盘输入不会进到下面的应用。不挂键盘钩子。关闭时把前台还给打开之前的那个窗口
//!   （[`Backend::drop`]）。
//! - 代价：叠加层在服务进程里，这个进程的窗口在前台期间，系统不调用
//!   它装的低级键盘钩子——进行期间本机键盘不被 Oink 截获（不转发给别的设备、Oink 的快捷键与长按 Esc
//!   不起作用）。键盘在这个游戏里本来就不用；Esc 由叠加层自己收到并结束整局，结束后截获照常。
//!   鼠标钩子不受影响。
//! - 点击 / 按键：`WM_LBUTTONDOWN`（窗口类不带 `CS_DBLCLKS`，连点两下是两次按下）、`WM_KEYDOWN`。收到的
//!   是系统投递给窗口的消息：本机键鼠的原生输入，或 Oink 在本机注入的输入，叠加层不区分。
//! - 层级：排在光标定位拖尾窗口（如果它正开着）的后面，否则在置顶窗口的最上面；每 250 ms 重新排一次
//!   （见 [`Backend::raise`]）。
//! - 线程：窗口由绘制线程创建，消息也在这个线程上每帧处理一次；收起时销毁窗口（空闲线程不处理消息，
//!   留着顶层窗口会让 `SendMessage(HWND_BROADCAST)` 等它）。

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use oink_foundation::Point;
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, ReleaseDC, SetDIBitsToDevice, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::Input::Ime::{HIMC, ImmAssociateContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW,
    GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, HWND_TOPMOST, IDC_ARROW, IsWindow, LoadCursorW, MSG,
    PM_REMOVE, PeekMessageW, RegisterClassExW, SC_CLOSE, SC_KEYMENU, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SetForegroundWindow, SetWindowPos, ShowWindow, WM_CHAR, WM_CLOSE, WM_DEADCHAR, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_PAINT, WM_SYSCHAR, WM_SYSCOMMAND, WM_SYSDEADCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::w;

use super::scene::{PxRect, SceneDisplay, Sprite, compose};

pub(crate) fn available() -> bool {
    true
}

thread_local! {
    /// 本线程的窗口收到的点击（本机全局坐标 = 虚拟桌面的物理像素）。
    static CLICKS: RefCell<Vec<Point>> = const { RefCell::new(Vec::new()) };
    /// 系统要求重画（窗口刚显示、被别的窗口遮过）：下一帧整屏重画。
    static REPAINT: Cell<bool> = const { Cell::new(false) };
    /// 本线程的窗口收到了一次 Esc 按下（不是按住不放的重复）。
    static ESCAPE: Cell<bool> = const { Cell::new(false) };
}

struct Win {
    hwnd: HWND,
    w: i32,
    h: i32,
}

pub(crate) struct Backend {
    wins: Vec<Win>,
    /// 上一次画的那一帧（重画整屏时用）。
    last: Vec<Vec<Sprite>>,
    buf: Vec<u32>,
    raised: Instant,
    /// 打开之前的前台窗口：关闭时把前台还给它。
    previous: Option<HWND>,
}

/// 多久重新排一次层级。
const RAISE_EVERY: Duration = Duration::from_millis(250);

impl Backend {
    pub(crate) fn open(displays: &[SceneDisplay]) -> Option<Self> {
        // SAFETY: 只传入系统预定义的 DPI 上下文常量；本线程只做叠加层窗口。
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        CLICKS.with(|c| c.borrow_mut().clear());
        ESCAPE.set(false);
        // SAFETY: 只读查询。
        let previous = Some(unsafe { GetForegroundWindow() }).filter(|h| !h.is_invalid());
        let mut this = Self {
            wins: Vec::new(),
            last: vec![Vec::new(); displays.len()],
            buf: Vec::new(),
            raised: Instant::now(),
            previous,
        };
        for d in displays {
            // 创建失败时 `this` 被丢弃，已创建的窗口随之销毁。
            let win = create_window(d)?;
            this.wins.push(win);
        }
        REPAINT.set(true);
        if let Some(first) = this.wins.first() {
            // 只记日志：没拿到前台时按键会进下面的应用（Esc 也到不了），用户点一下叠加层即可拿到。
            tracing::info!(foreground = take_foreground(first.hwnd), "aim overlay keyboard focus");
        }
        pump();
        Some(this)
    }

    /// 自上次调用以来窗口是否收到过 Esc（消息在 [`Backend::poll_clicks`] 里处理，先调用它）。
    pub(crate) fn take_escape(&mut self) -> bool {
        pump();
        ESCAPE.replace(false)
    }

    /// 把窗口重新排到置顶窗口的最上面（拖尾窗口开着时排在它后面）。
    ///
    /// 任务栏也是置顶窗口，而且会在前台窗口变化等时候把自己重新提到所有置顶窗口之上。系统只在**前台**
    /// 窗口是全屏时才让任务栏让位：叠加层没拿到前台、或用户切到了别的应用（Alt+Tab、Win 键）之后，任务栏会
    /// 盖回来，落在任务栏那一条上的标靶点不到（Windows 11 实测：叠加层不在前台、只在创建时排一次时，
    /// 屏幕底部 72 像素内的点击到不了窗口；定时重排后都能收到）。定时重排只改层级（不移动、不改大小、
    /// 不激活——不会把用户切过去的应用的键盘焦点抢回来），不影响别的窗口；拖尾窗口每次创建都在最上面，
    /// 这里重排时把自己放在它后面，不会盖住它。
    fn raise(&mut self) {
        self.raised = Instant::now();
        let after = below_trails();
        // SAFETY: 本线程创建的窗口；只改层级。
        unsafe {
            for w in &self.wins {
                let _ = SetWindowPos(
                    w.hwnd,
                    Some(after),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    pub(crate) fn poll_clicks(&mut self) -> Vec<Point> {
        if self.raised.elapsed() >= RAISE_EVERY {
            self.raise();
        }
        pump();
        if REPAINT.replace(false) {
            for i in 0..self.wins.len() {
                let full = PxRect {
                    x: 0,
                    y: 0,
                    w: self.wins[i].w,
                    h: self.wins[i].h,
                };
                self.paint(i, &[full]);
            }
        }
        CLICKS.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }

    pub(crate) fn present(&mut self, frame: &[Vec<Sprite>], dirty: &[Vec<PxRect>]) {
        self.last = frame.to_vec();
        for (i, rects) in dirty.iter().enumerate().take(self.wins.len()) {
            if !rects.is_empty() {
                self.paint(i, rects);
            }
        }
    }

    /// 把窗口 `index` 里的这些矩形重新合成（背景色 + 各图块）并画上去。
    fn paint(&mut self, index: usize, rects: &[PxRect]) {
        let win = &self.wins[index];
        let bounds = PxRect {
            x: 0,
            y: 0,
            w: win.w,
            h: win.h,
        };
        let sprites = self.last.get(index).map_or(&[][..], Vec::as_slice);
        // SAFETY: 本线程创建的窗口；DC 用完即还。位图信息与缓冲区大小一致（自上而下的 32 位 DIB）。
        unsafe {
            let dc = GetDC(Some(win.hwnd));
            if dc.is_invalid() {
                return;
            }
            for r in rects {
                let Some(r) = r.intersect(&bounds) else { continue };
                compose(r, sprites, &mut self.buf);
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: r.w,
                        biHeight: -r.h,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetDIBitsToDevice(
                    dc,
                    r.x,
                    r.y,
                    r.w as u32,
                    r.h as u32,
                    0,
                    0,
                    0,
                    r.h as u32,
                    self.buf.as_ptr().cast(),
                    &info,
                    DIB_RGB_COLORS,
                );
            }
            ReleaseDC(Some(win.hwnd), dc);
        }
    }

    pub(crate) fn wait_frame(&mut self, frame_start: Instant) {
        // DwmFlush 等到下一次桌面合成（与刷新率同步）；合成关闭或失败时按 60 Hz。
        // SAFETY: 无参数调用。
        let ok = unsafe { DwmFlush() }.is_ok();
        let elapsed = frame_start.elapsed();
        if !ok || elapsed < Duration::from_millis(2) {
            let target = super::DEFAULT_FRAME;
            if elapsed < target {
                std::thread::sleep(target - elapsed);
            }
        }
    }
}

impl Drop for Backend {
    /// 销毁窗口，并把前台还给打开之前的那个窗口。
    ///
    /// 为什么要自己还：前台窗口被销毁时，系统把前台交给层级里的下一个窗口；叠加层是置顶窗口，它下面
    /// 紧挨着的可能是别的置顶窗口（任务栏等），不一定是用户原来在用的应用。所以关闭时如果前台还在叠加层
    /// 这里，就明确交还给打开之前的前台窗口——那个应用自己恢复它的输入框焦点。用户中途已经切到别的应用
    /// （前台不在叠加层）时不动。本进程此刻是前台进程，`SetForegroundWindow` 不受前台锁定限制。
    /// Windows 11 实测。
    fn drop(&mut self) {
        // SAFETY: 只读查询；窗口都是本线程创建的；`previous` 先确认仍然是一个窗口。
        unsafe {
            let foreground = GetForegroundWindow();
            let ours = self.wins.iter().any(|w| w.hwnd == foreground);
            for w in self.wins.drain(..) {
                let _ = DestroyWindow(w.hwnd);
            }
            if ours
                && let Some(previous) = self.previous
                && IsWindow(Some(previous)).as_bool()
            {
                let _ = SetForegroundWindow(previous);
            }
        }
        pump();
    }
}

/// 让叠加层成为前台窗口（拿到键盘焦点）。返回怎么拿到的：`"direct"`（直接调用就行）、`"attached"`
/// （并入前台线程之后）、`"failed"`（没拿到）。
///
/// 为什么需要：按键要由系统投递给叠加层而不是下面的应用，它就得是前台窗口。
///
/// 为什么是这个形态：Windows 限制后台进程调用 `SetForegroundWindow`（前台锁定）——叠加层所在的进程多半
/// 不是当前的前台进程（发起的那台，用户刚点的是界面进程里的「开始」；其它设备上没有人在操作本进程），
/// 直接调用只会让任务栏按钮闪烁。先直接试；不行时用 `AttachThreadInput` 把本线程的输入状态临时并到当前
/// 前台线程上再调用——并入之后系统把本线程当作与前台线程共享输入，允许它切换前台；调用完立即分开。
/// 没有用「合成一次 Alt 按键来解除前台锁定」的做法：它会向系统注入按键（Oink 的截获、前台应用的菜单
/// 都会看到）。
///
/// 不影响什么：并入只持续这几个调用；原来的前台窗口只是失去前台，关闭时还给它（见 `Drop`）。
/// 前台是以管理员身份运行的窗口时并入会失败（权限隔离），这时拿不到前台，用户点一下叠加层即可。
///
/// 实测：Windows 11，独立进程（启动时不是前台进程、自己没有收到过输入，前台是另一个进程的文本框窗口）
/// 里直接调用每次都成功；并入这条路在测试里还没有被触发过（待用前台锁定的条件复测）。
fn take_foreground(hwnd: HWND) -> &'static str {
    // SAFETY: 本线程创建的窗口；其余是只读查询与对本线程输入状态的并入 / 分开。
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        let mut how = "direct";
        if GetForegroundWindow() != hwnd {
            how = "attached";
            let foreground = GetForegroundWindow();
            let (me, other) = (GetCurrentThreadId(), GetWindowThreadProcessId(foreground, None));
            if other != 0 && other != me && AttachThreadInput(me, other, true).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                let _ = BringWindowToTop(hwnd);
                let _ = AttachThreadInput(me, other, false);
            }
        }
        if GetForegroundWindow() == hwnd {
            let _ = SetFocus(Some(hwnd));
            how
        } else {
            "failed"
        }
    }
}

fn create_window(d: &SceneDisplay) -> Option<Win> {
    let (x, y) = (d.bounds.x.round() as i32, d.bounds.y.round() as i32);
    let (w, h) = (d.bounds.w.round() as i32, d.bounds.h.round() as i32);
    // SAFETY: 标准的窗口类注册与窗口创建。
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let class = w!("OinkAimOverlay");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            tracing::warn!("aim overlay: RegisterClassExW failed");
            return None;
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("Oink aim game"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .ok()?;
        // 排到光标定位拖尾窗口的后面（它正开着的话）；否则放到置顶窗口的最上面（之后定时重排，见 raise）。
        let _ = SetWindowPos(
            hwnd,
            Some(below_trails()),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        // 这个窗口不接受文字输入：去掉输入法上下文，按字母键时不会弹出输入法的候选窗口。
        let _ = ImmAssociateContext(hwnd, HIMC::default());
        let _ = ShowWindow(hwnd, SW_SHOW);
        Some(Win { hwnd, w, h })
    }
}

/// 叠加层要排在谁的后面：最下面的那个拖尾窗口（可能不止一个：服务的拖尾、暂停时小游戏自己的拖尾）；
/// 一个都没有时是「置顶窗口的最上面」。
fn below_trails() -> HWND {
    let mut lowest = None;
    // 顶层窗口按从上到下的层级枚举；最后找到的在最下面。数量有上限，防止层级正在变化时转圈。
    for _ in 0..16 {
        // SAFETY: 只读的窗口枚举；`lowest` 是上一次枚举得到的句柄（已销毁时调用失败，循环结束）。
        match unsafe { FindWindowExW(None, lowest, w!("OinkCursorTrail"), None) } {
            Ok(h) if !h.is_invalid() => lowest = Some(h),
            _ => break,
        }
    }
    lowest.unwrap_or(HWND_TOPMOST)
}

/// 处理本线程的窗口消息（不阻塞）。
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: 标准消息循环。
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            // 不调用 TranslateMessage：窗口不接受文字输入，不需要字符消息。
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            // 客户区坐标（有符号 16 位）→ 虚拟桌面坐标。
            let x = i32::from(lparam.0 as u16 as i16);
            let y = i32::from((lparam.0 >> 16) as u16 as i16);
            let mut r = RECT::default();
            // SAFETY: 系统传入的有效窗口句柄。
            if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
                CLICKS.with(|c| {
                    c.borrow_mut()
                        .push(Point::new(f64::from(r.left + x), f64::from(r.top + y)))
                });
            }
            LRESULT(0)
        }
        // 按键到此为止：Esc（不是按住不放的重复：lParam 第 30 位是「之前已经按着」）结束游戏，其它键什么也
        // 不做。不交给默认处理：Alt+F4 会关窗口、Alt / F10 会进系统菜单。Alt+Tab、Win 键等由系统在按键到达
        // 窗口之前处理，不经过这里。
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if wparam.0 == usize::from(VK_ESCAPE.0) && lparam.0 & (1 << 30) == 0 {
                ESCAPE.set(true);
            }
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR | WM_CLOSE => LRESULT(0),
        WM_SYSCOMMAND if matches!((wparam.0 & 0xFFF0) as u32, SC_KEYMENU | SC_CLOSE) => LRESULT(0),
        // 背景与内容都由我们自己画。
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // SAFETY: 系统传入的有效窗口句柄；整个客户区标为已画，下一帧由绘制线程重画。
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            REPAINT.set(true);
            LRESULT(0)
        }
        // SAFETY: 系统调用的窗口过程，参数原样交给默认处理。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
