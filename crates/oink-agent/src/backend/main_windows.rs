//! Backend application-thread dispatch shared by native windows and drag/drop.
use std::sync::{OnceLock, mpsc};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostQuitMessage, PostThreadMessageW,
    TranslateMessage, WM_APP,
};
type Job = Box<dyn FnOnce() + Send>;
static MAIN: OnceLock<(u32, mpsc::Sender<Job>)> = OnceLock::new();
const WORK: u32 = WM_APP + 31;

pub(super) fn on_main(f: impl FnOnce() + Send + 'static) {
    if let Some((thread, queue)) = MAIN.get() {
        let _ = queue.send(Box::new(f));
        // SAFETY: The queue was created by run_main before publishing its thread ID.
        unsafe {
            let _ = PostThreadMessageW(*thread, WORK, WPARAM(0), LPARAM(0));
        }
    }
}

pub(super) fn run_main<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel::<Job>();
    // SAFETY: MSG storage is valid for these calls and no borrowed pointers escape.
    // Win32 only creates a thread message queue on its first user32 message call.
    unsafe {
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        MAIN.set((GetCurrentThreadId(), tx))
            .expect("backend main loop initialized once");
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let worker = std::thread::spawn(move || {
        let result = work();
        on_main(|| {
            // SAFETY: this callback runs on the owning message-loop thread.
            unsafe {
                PostQuitMessage(0);
            }
        });
        result
    });
    // SAFETY: MSG is initialized storage; all messages belong to this thread.
    unsafe {
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.message == WORK {
                for job in rx.try_iter() {
                    job();
                }
            } else {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    worker.join().expect("backend host panicked")
}
