//! Releases observed while the native state or routing owner is borrowed.
use super::*;
use std::collections::VecDeque;
use windows::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{PostThreadMessageW, WM_KEYUP, WM_SYSKEYUP},
};

pub(super) const MESSAGE: u32 = WM_APP + 8;
struct Release {
    message: u32,
    info: KBDLLHOOKSTRUCT,
    instant: Instant,
    source_time: oink_foundation::input::CaptureTime,
}
thread_local! {
    static PENDING: RefCell<VecDeque<Release>> = const { RefCell::new(VecDeque::new()) };
}

pub(super) fn defer(message: u32, info: &KBDLLHOOKSTRUCT) {
    if !matches!(message, WM_KEYUP | WM_SYSKEYUP) || info.vkCode >= 256 || keyboard_scancode(info).is_none() {
        return;
    }
    // An LL hook can reenter during a native mode/foreground operation while
    // State or the capture owner is borrowed. The original UP must pass then,
    // but forgetting it leaves a hotkey's Dropped press alive after returning
    // home (and consumes the next complete press). Keep the exact observed UP
    // and finish its route at the first safe turn, in either capture mode.
    // Do not clear Dropped on focus change: a still-held key's repeats must not
    // trigger a second hotkey. Do not replay the already-passed OS input.
    // This is a Windows callback-reentry boundary, not an injected-input policy:
    // both physical and opted-in external releases enter here; own/external-local
    // input was filtered first. Native callback reentry and the affected tail
    // were reproduced through the Windows 11 hook entrance in recording tests.
    let mut queued = false;
    let wake = PENDING.with_borrow_mut(|pending| {
        // At most one unresolved release per Win32 VK (256 slots). Duplicate
        // UPs add no ownership and cannot consume unbounded memory on reentry.
        if pending.iter().any(|release| release.info.vkCode == info.vkCode) {
            return false;
        }
        let wake = pending.is_empty();
        queued = true;
        pending.push_back(Release {
            message,
            info: *info,
            instant: Instant::now(),
            source_time: crate::source_time::capture_time(),
        });
        wake
    });
    keyboard_trace::release(keyboard_trace::Stage::Deferred, info, queued);
    if wake {
        #[cfg(all(test, debug_assertions))]
        if TEST_CURSOR.with(|cursor| cursor.get().is_some()) {
            // Recording fixtures explicitly drive the next safe callback/turn.
            return;
        }
        // SAFETY: post to this hook thread's existing message queue. No pointer
        // escapes; the retained native struct is copied into thread-local storage.
        if let Err(error) = unsafe { PostThreadMessageW(GetCurrentThreadId(), MESSAGE, WPARAM(0), LPARAM(0)) } {
            tracing::warn!(%error, "cannot wake deferred keyboard release; next callback will drain it");
        }
    }
}

pub(super) fn drain() {
    if !crate::capture_loop::callback_available() || with_state(|_| ()).is_none() {
        return;
    }
    while let Some(release) = PENDING.with_borrow_mut(VecDeque::pop_front) {
        let _source_time = crate::source_time::CaptureScope::new(release.source_time);
        let prepared = with_state(|state| state.prepare_key(release.message, &release.info, release.instant));
        // Only retire the original route/held ledgers. The native UP already
        // passed through the nested callback; no second OS event is posted.
        let available = prepared.is_some();
        finish(prepared);
        keyboard_trace::release(keyboard_trace::Stage::Drained, &release.info, available);
    }
}

pub(super) fn clear() {
    local_releases::clear();
    PENDING.with_borrow_mut(|pending| {
        for release in pending.iter() {
            keyboard_trace::release(keyboard_trace::Stage::QueueCleared, &release.info, true);
        }
        pending.clear();
    });
}
