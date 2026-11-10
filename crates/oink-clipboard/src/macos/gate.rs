//! One Oink thread at a time inside NSPasteboard.
//!
//! NSPasteboard and NSPasteboardItem are not thread-safe: AppKit's headers are
//! sendability-audited and leave both non-Sendable (thread-safe AppKit classes
//! such as NSColor are marked `NS_SWIFT_SENDABLE`), and `+generalPasteboard`
//! hands every thread the same instance. Oink used it from the watcher thread
//! (changeCount every 75 ms), blocking-pool captures, reads and writes, and the
//! main thread's lazy data callbacks at the same time. A probe that rewrites a
//! private board on one thread while two others read changeCount/types crashes
//! within seconds with EXC_BAD_ACCESS in objc_msgSend under
//! `-[NSPasteboard _updateTypeCacheIfNeeded]`; with one mutex around every use
//! it runs clean (Sentry RUST-A/9/7/5, evidence in sentry-1109/probe).
//!
//! The gate is reentrant: AppKit runs an in-process read of Oink's own lazy
//! item synchronously on the reading thread, so a data provider can be entered
//! while that thread already holds its turn. Hold a turn only around
//! pasteboard calls, never across a wait for remote data or the main thread.
use objc2::rc::Retained;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::NSString;
use std::marker::PhantomData;
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread::ThreadId;

struct State {
    owner: Option<ThreadId>,
    depth: usize,
}
static STATE: Mutex<State> = Mutex::new(State { owner: None, depth: 0 });
static FREE: Condvar = Condvar::new();

/// This thread's turn; pasteboard objects obtained under it are used before
/// it drops. Not `Send`: a turn belongs to the thread that took it.
pub(crate) struct Turn {
    _thread: PhantomData<*const ()>,
}

pub(crate) fn enter() -> Turn {
    let me = std::thread::current().id();
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    while state.owner.is_some_and(|owner| owner != me) {
        state = FREE.wait(state).unwrap_or_else(PoisonError::into_inner);
    }
    state.owner = Some(me);
    state.depth += 1;
    Turn { _thread: PhantomData }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        state.depth -= 1;
        if state.depth == 0 {
            state.owner = None;
            FREE.notify_all();
        }
    }
}

/// `None` is the system clipboard; `Some(name)` a named board (tests,
/// side-by-side instances).
pub(crate) fn board(_turn: &Turn, name: Option<&str>) -> Retained<NSPasteboard> {
    match name {
        None => NSPasteboard::generalPasteboard(),
        Some(name) => NSPasteboard::pasteboardWithName(&NSString::from_str(name)),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    /// RUST-A/9/7/5 through Oink's own entry points. A received clip is
    /// published with lazy data providers (the pasteboard's owners); the next
    /// publication's clearContents releases that owners collection while the
    /// watcher's changeCount on another thread walks it in
    /// `-[_NSPasteboardOwnersCollection handleOwnershipChange]` (the stack of
    /// a reported oink-backend crash). A capture-style read races the same
    /// board. Before the gate this crashed the test process.
    #[test]
    fn concurrent_watch_capture_and_lazy_publication_on_one_board_are_serialized() {
        let name = format!("com.hachiwii.oink.test.gate.{}", std::process::id());
        let spec = crate::NativeSpec {
            inventory: crate::content::Provider::inventory(&crate::content::tests::fixture()),
            contents: Arc::new(crate::content::tests::fixture()),
            files: Vec::new(),
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let polls = Arc::new(AtomicUsize::new(0));
        let readers: Vec<_> = [false, true]
            .into_iter()
            .map(|capture| {
                let (name, stop, polls) = (name.clone(), stop.clone(), polls.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        super::super::pasteboard::change_count(Some(&name));
                        if capture {
                            let _ = super::super::pasteboard::read(Some(&name));
                        }
                        polls.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        let started = std::time::Instant::now();
        let mut writes = 0u64;
        while started.elapsed() < std::time::Duration::from_secs(5) {
            super::super::content::write(Some(&name), &spec, &Default::default(), &|| Ok(())).unwrap();
            writes += 1;
        }
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }
        assert!(writes > 0 && polls.load(Ordering::Relaxed) > 0);
        objc2::rc::autoreleasepool(|_| {
            let turn = enter();
            // SAFETY: releaseGlobally is a public NSPasteboard selector; this
            // test owns the private board.
            unsafe {
                let _: () = objc2::msg_send![&*board(&turn, Some(&name)), releaseGlobally];
            }
        });
    }

    #[test]
    fn a_turn_is_reentrant_on_its_thread_and_exclusive_across_threads() {
        let outer = enter();
        let inner = enter();
        let entered = Arc::new(AtomicBool::new(false));
        let other = {
            let entered = entered.clone();
            std::thread::spawn(move || {
                let _turn = enter();
                entered.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!entered.load(Ordering::SeqCst));
        drop(inner);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!entered.load(Ordering::SeqCst), "still held by the outer turn");
        drop(outer);
        other.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }
}
