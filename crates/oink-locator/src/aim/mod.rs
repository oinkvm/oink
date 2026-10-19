//! Aim game rules, rendering, and native full-screen overlays.
//!
//! - [`game`]：规则（权威设备上运行的纯状态机）；
//! - [`scene`] / [`raster`]：画面（两个平台共用的光栅化，输出盖在不透明背景上的小图块）；
//! - [`text`]：叠加层上的中文文字（各平台的系统文字接口）；
//! - 平台层：每块屏幕一个不透明、置顶的窗口，像普通的全屏游戏一样拿到**键盘焦点**（macOS：成为 key 窗口的
//!   `NSPanel`；Windows：前台窗口），鼠标点击与按键都由系统正常投递给它；层级在光标定位拖尾的窗口之下。
//!   进行期间键盘输入因此不会进到下面的应用；不挂任何键盘截获。
//!
//! [`AimOverlay`] 是可在任意线程调用的句柄。每次打开起一个绘制线程：按屏幕刷新率轮询点击、出帧，
//! 并且**自己负责收掉叠加层**——这是盖住整个屏幕的窗口，收不掉就等于锁死用户，所以下面三种退出都在
//! 这个线程里、只用本机的系统调用完成，不依赖网络、路由和调用方：
//!
//! 1. 按 Esc（窗口自己从系统投递的按键事件里收到；暂停时 Oink 的键盘截获已经摘掉，这条路照样成立）；
//! 2. 调用方超过 [`OpenSpec::stale_after`] 没有交来新的状态（权威掉线、服务的游戏任务停了）；
//! 3. 打开后超过 [`OpenSpec::lifetime`]（一局的时长加余量，无论如何都收）。
//!
//! 进程退出或崩溃时窗口随进程消失。

pub mod game;
pub mod model;
pub mod raster;
pub mod scene;
pub mod text;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use oink_foundation::Point;

pub use scene::{Scene, SceneDisplay, Sprite, View, ViewTarget};

/// 打开叠加层的参数。
#[derive(Clone, Debug)]
pub struct OpenSpec {
    /// 本机的全部显示器（每块都盖上）。
    pub displays: Vec<SceneDisplay>,
    /// 打开后最长保留多久，到时无条件收起。
    pub lifetime: Duration,
    /// 多久没有收到新的状态（[`AimSurface::update`]）就收起。
    pub stale_after: Duration,
}

/// 叠加层为什么收起。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// 调用方要求（[`AimSurface::close`]）。
    Requested,
    /// 用户在本机按了 Esc。
    Escape,
    /// 太久没有收到新的状态。
    Stale,
    /// 超过了最长保留时间。
    Lifetime,
    /// 窗口创建失败。
    Failed,
}

/// 叠加层报告的事件。在绘制线程上调用，不要阻塞。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AimEvent {
    /// 叠加层收到一次左键点击（本机全局坐标）。
    Click(Point),
    /// 叠加层已经收起（每次打开恰好报告一次）。
    Closed(CloseReason),
}

pub type AimEvents = Arc<dyn Fn(AimEvent) + Send + Sync>;

/// 本机的全屏叠加层。真实实现是 [`AimOverlay`]；测试用假的。
pub trait AimSurface: Send + Sync {
    /// 盖上叠加层。已经开着（或创建不了线程）时返回 `false`，不报告事件。
    fn open(&self, spec: OpenSpec, events: AimEvents) -> bool;
    /// 交来新的游戏状态（也算一次「调用方还在」）。
    fn update(&self, view: View);
    /// 收起。没开着时什么也不做。
    fn close(&self);
}

#[derive(Debug, Default)]
struct State {
    /// 每次打开递增；绘制线程只认自己那一次。
    session: u64,
    open: bool,
    close: bool,
    view: Option<View>,
    updated: Option<Instant>,
}

/// 平台的全屏叠加层句柄。可以廉价克隆，在任意线程调用。
#[derive(Clone, Debug)]
pub struct AimOverlay {
    state: Arc<Mutex<State>>,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AimOverlay {
    /// 本机平台的实现；平台没有实现时为 `None`。
    pub fn platform() -> Option<Self> {
        platform::available().then(|| Self {
            state: Arc::new(Mutex::new(State::default())),
        })
    }

    pub fn is_open(&self) -> bool {
        lock(&self.state).open
    }
}

impl AimSurface for AimOverlay {
    fn open(&self, spec: OpenSpec, events: AimEvents) -> bool {
        let session = {
            let mut s = lock(&self.state);
            if s.open {
                return false;
            }
            s.session += 1;
            s.open = true;
            s.close = false;
            s.view = None;
            s.updated = Some(Instant::now());
            s.session
        };
        let state = self.state.clone();
        let spawned = std::thread::Builder::new()
            .name("oink-aim-overlay".into())
            .spawn(move || {
                let reason = run(&state, session, spec, &events);
                {
                    let mut s = lock(&state);
                    if s.session == session {
                        s.open = false;
                    }
                }
                tracing::info!(?reason, "aim overlay closed");
                events(AimEvent::Closed(reason));
            });
        if let Err(e) = spawned {
            tracing::warn!(%e, "cannot start the aim overlay thread");
            lock(&self.state).open = false;
            return false;
        }
        true
    }

    fn update(&self, view: View) {
        let mut s = lock(&self.state);
        if s.open {
            s.view = Some(view);
            s.updated = Some(Instant::now());
        }
    }

    fn close(&self) {
        let mut s = lock(&self.state);
        if s.open {
            s.close = true;
        }
    }
}

/// 绘制线程：直到叠加层收起才返回。窗口由 `Backend` 持有，离开这个函数时（包括 panic 展开）一定关闭。
fn run(state: &Mutex<State>, session: u64, spec: OpenSpec, events: &AimEvents) -> CloseReason {
    let Some(mut backend) = platform::Backend::open(&spec.displays) else {
        tracing::warn!("aim overlay: no platform window");
        return CloseReason::Failed;
    };
    let opened = Instant::now();
    let secs = |at: Instant| at.saturating_duration_since(opened).as_secs_f64();
    let mut scene = Scene::new(spec.displays.clone());
    let mut prev: Vec<Vec<Sprite>> = vec![Vec::new(); spec.displays.len()];
    tracing::info!(displays = spec.displays.len(), "aim overlay opened");
    loop {
        let frame_start = Instant::now();
        let (view, updated) = {
            let mut s = lock(state);
            if s.session != session || s.close {
                return CloseReason::Requested;
            }
            (s.view.take(), s.updated)
        };
        if frame_start.saturating_duration_since(opened) >= spec.lifetime {
            return CloseReason::Lifetime;
        }
        if updated.is_none_or(|u| frame_start.saturating_duration_since(u) >= spec.stale_after) {
            return CloseReason::Stale;
        }
        if backend.take_escape() {
            return CloseReason::Escape;
        }
        let now = secs(frame_start);
        if let Some(v) = view {
            scene.set_view(v, now);
        }
        for p in backend.poll_clicks() {
            tracing::debug!(x = p.x, y = p.y, "aim overlay click");
            scene.click(p, now);
            events(AimEvent::Click(p));
        }
        let next = scene.frame(now);
        let dirty: Vec<Vec<scene::PxRect>> = prev.iter().zip(&next).map(|(p, n)| scene::dirty_rects(p, n)).collect();
        if dirty.iter().any(|d| !d.is_empty()) {
            backend.present(&next, &dirty);
        }
        prev = next;
        backend.wait_frame(frame_start);
    }
}

/// 不知道刷新率时的帧间隔（60 Hz）。
#[cfg(any(target_os = "macos", windows))]
pub(crate) const DEFAULT_FRAME: Duration = Duration::from_micros(16_667);

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub(crate) fn available() -> bool {
        false
    }

    pub(crate) struct Backend;

    impl Backend {
        pub(crate) fn open(_: &[SceneDisplay]) -> Option<Self> {
            None
        }
        pub(crate) fn take_escape(&mut self) -> bool {
            false
        }
        pub(crate) fn poll_clicks(&mut self) -> Vec<Point> {
            Vec::new()
        }
        pub(crate) fn present(&mut self, _: &[Vec<Sprite>], _: &[Vec<scene::PxRect>]) {}
        pub(crate) fn wait_frame(&mut self, _: Instant) {}
    }
}
