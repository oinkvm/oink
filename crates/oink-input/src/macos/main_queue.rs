//! 把工作交给主线程（主 dispatch 队列）执行。
//!
//! **为什么**：Text Input Sources（TIS，输入法的读取与切换）只能在主线程调用。HIToolbox 发现进程是界面
//! 程序（「UI client」，例如 Tauri / NSApplication 打开了窗口）时，在其他线程调用会
//! `dispatch_assert_queue(main)` 失败，进程被 SIGTRAP 终止（2026-09-29 线上崩溃：移交焦点时 tokio 线程
//! 读取输入法）；不是界面程序时，调用线程与第一次调用 TIS 的线程不同也会 `abort`。所以本 crate 的所有 TIS
//! 调用都经这里放到主线程。
//!
//! **主线程必须在服务主队列**：界面程序里是 NSApplication 的事件循环；命令行程序用
//! `crate::run_with_main_run_loop`（主线程跑 CFRunLoop，也会执行主队列）。没有人服务主队列时（例如单元
//! 测试的线程），投递的工作一直不执行：不会崩溃，只是本机 Caps 设置与源端守卫不可用；排队的工作超过
//! [`MAX_PENDING`] 时不再投递并记一条日志。
//!
//! 只用 fire-and-forget 的 [`run_async`]：不在其他线程上同步等待主线程（主线程可能正在等调用方持有的
//! 锁，互相等待会死锁）。诊断接口需要结果时用带超时的 [`run_blocking`]。

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// 主队列里最多排多少个还没执行的工作（主队列没人服务时防止无限堆积）。
const MAX_PENDING: usize = 256;

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// `dispatch_get_main_queue()` 是返回这个对象地址的宏。
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_after_f(when: u64, queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_time(when: u64, delta_ns: i64) -> u64;
}

/// `DISPATCH_TIME_NOW`。
const DISPATCH_TIME_NOW: u64 = 0;

static PENDING: AtomicUsize = AtomicUsize::new(0);
static DROP_LOGGED: AtomicBool = AtomicBool::new(false);

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 当前线程是不是主线程。
pub(crate) fn is_main_thread() -> bool {
    // SAFETY: 无参数的查询函数。
    unsafe { libc::pthread_main_np() == 1 }
}

extern "C" fn trampoline(ctx: *mut c_void) {
    // SAFETY: ctx 是 run_async 里 Box::into_raw 得到的 Box<Job>，只执行一次。
    let job = unsafe { Box::from_raw(ctx as *mut Job) };
    PENDING.fetch_sub(1, Ordering::AcqRel);
    // 不让 panic 穿过 C 边界（那会直接终止进程）。
    if std::panic::catch_unwind(AssertUnwindSafe(job)).is_err() {
        tracing::error!("主线程上执行的输入法工作 panic");
    }
}

/// 在主线程上执行 `f`，不等待。当前就在主线程时立即执行。返回是否已执行或已投递（排队过多时放弃）。
pub(crate) fn run_async(f: impl FnOnce() + Send + 'static) -> bool {
    if is_main_thread() {
        f();
        return true;
    }
    let Some(job) = enqueue(f) else {
        return false;
    };
    // SAFETY: 主队列对象在进程内一直有效；context 由 trampoline 接管并释放。
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, job, trampoline) };
    true
}

/// 计入排队数并装箱（交给 trampoline 的 context）；排队过多时放弃。
fn enqueue(f: impl FnOnce() + Send + 'static) -> Option<*mut c_void> {
    if PENDING.fetch_add(1, Ordering::AcqRel) >= MAX_PENDING {
        PENDING.fetch_sub(1, Ordering::AcqRel);
        if !DROP_LOGGED.swap(true, Ordering::AcqRel) {
            tracing::warn!("主线程没有在服务主队列（没有运行 run loop）：本机 Caps 设置与源端守卫不可用");
        }
        return None;
    }
    let job: Box<Job> = Box::new(Box::new(f));
    Some(Box::into_raw(job) as *mut c_void)
}

/// `delay` 之后在主线程上执行 `f`（总是排队，即使当前就在主线程）。返回是否已投递。
pub(crate) fn run_after(delay: Duration, f: impl FnOnce() + Send + 'static) -> bool {
    let Some(job) = enqueue(f) else {
        return false;
    };
    let ns = i64::try_from(delay.as_nanos()).unwrap_or(i64::MAX);
    // SAFETY: 同 run_async。
    unsafe {
        dispatch_after_f(
            dispatch_time(DISPATCH_TIME_NOW, ns),
            &raw const _dispatch_main_q,
            job,
            trampoline,
        )
    };
    true
}

/// 在主线程上执行 `f` 并等待结果，最多等 `timeout`（诊断接口用；主线程不服务主队列时返回 `None`）。
/// 当前就在主线程时立即执行。
pub(crate) fn run_blocking<T: Send + 'static>(timeout: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    if is_main_thread() {
        return Some(f());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    if !run_async(move || {
        let _ = tx.send(f());
    }) {
        return None;
    }
    rx.recv_timeout(timeout).ok()
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    /// 单元测试的线程都不是主线程，libtest 的主线程也不服务主队列：投递不执行、不阻塞、不崩溃。
    #[test]
    fn off_main_without_a_main_run_loop_degrades() {
        assert!(!is_main_thread());
        let t = std::time::Instant::now();
        assert!(run_async(|| panic!("主队列在单元测试里不应被执行")));
        assert_eq!(run_blocking(Duration::from_millis(50), || 1), None);
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
