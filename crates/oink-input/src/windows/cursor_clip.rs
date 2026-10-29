//! Serialize raw-capture ownership with read-only application-clip snapshots.
//! The query is called by the ordinary input loop, not a low-level hook.
use crate::cursor_clip::Ownership;
use oink_foundation::Rect;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClipCursor, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

fn owner() -> &'static Mutex<Ownership> {
    static OWNER: OnceLock<Mutex<Ownership>> = OnceLock::new();
    OWNER.get_or_init(|| Mutex::new(Ownership::default()))
}
fn rect(r: RECT) -> Rect {
    Rect::new(
        f64::from(r.left),
        f64::from(r.top),
        f64::from(r.right) - f64::from(r.left),
        f64::from(r.bottom) - f64::from(r.top),
    )
}
fn current() -> windows::core::Result<RECT> {
    let mut r = RECT::default();
    // SAFETY: initialized output rectangle, caller is in PMv2 coordinates.
    unsafe { GetClipCursor(&mut r) }?;
    Ok(r)
}
pub(super) fn application() -> windows::core::Result<Option<Rect>> {
    let _dpi = super::dpi::ThreadDpiGuard::per_monitor_v2();
    let mut owner = owner().lock().unwrap_or_else(|e| e.into_inner());
    let clip = current()?;
    // SAFETY: numeric desktop metrics, no borrowed handles.
    let desktop = unsafe {
        Rect::new(
            f64::from(GetSystemMetrics(SM_XVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_YVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
        )
    };
    Ok(owner.application(rect(clip), desktop))
}
/// A remote-game source lease hides our cursor, not the application's retained
/// confinement. Keep the underlying clip so another owner's game mode cannot
/// clear this device's passive-lock entry protection while focus is away.
pub(super) fn acquire(clip: RECT, previous: RECT) -> windows::core::Result<()> {
    acquire_confinement(clip, previous)
}
pub(super) fn acquire_confinement(clip: RECT, previous: RECT) -> windows::core::Result<()> {
    let mut owner = owner().lock().unwrap_or_else(|e| e.into_inner());
    if current()? != previous {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "application cursor clip changed",
        ));
    }
    // SAFETY: read-only desktop metrics in physical coordinates.
    let desktop = unsafe {
        Rect::new(
            f64::from(GetSystemMetrics(SM_XVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_YVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_CXVIRTUALSCREEN)),
            f64::from(GetSystemMetrics(SM_CYVIRTUALSCREEN)),
        )
    };
    super::warp::clip(clip)?;
    owner.acquired_confinement(rect(clip), rect(previous), desktop);
    Ok(())
}
pub(super) fn restore(previous: RECT, owned: RECT) -> bool {
    let mut owner = owner().lock().unwrap_or_else(|e| e.into_inner());
    // Keep the guard's existing conditional restore: never overwrite a later
    // application clip. The guard process has its own empty ownership ledger.
    let observed = match current() {
        Ok(clip) => clip,
        Err(e) => {
            tracing::warn!(%e, "reading game-mode cursor clip before restore failed");
            return false; // Unknown is not evidence that our clip was released.
        }
    };
    if observed == owned {
        let result = super::warp::clip(previous);
        super::trace::clip_action(previous, 0, result.is_ok());
        if let Err(e) = result {
            tracing::warn!(%e, "restoring game-mode cursor clip failed");
            return false; // Still our clip: a failed restore must not turn it into an application lock.
        }
    }
    owner.released(rect(owned));
    true
}
