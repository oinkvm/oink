//! No hooks, input injection, windows or desktops are created by these tests.
//! The real hook entrance runs with a recording handler and a fake next hook.
use super::*;
use std::sync::{Arc, Mutex, MutexGuard, atomic::AtomicBool};
use windows::Win32::UI::WindowsAndMessaging::{
    KBDLLHOOKSTRUCT_FLAGS, LLKHF_LOWER_IL_INJECTED, LLMHF_LOWER_IL_INJECTED, WM_KEYUP, WM_SYSKEYUP,
};

struct Owner(Arc<AtomicBool>);
impl crate::capture_loop::CaptureLoop for Owner {
    fn set_wake(&mut self, _: crate::capture_loop::Wake) {}
    fn event(&mut self, _: &CapturedEvent) -> Disposition {
        panic!("recording handler owns these events")
    }
    fn poll(&mut self, _: Instant) -> Option<Instant> {
        None
    }
    fn stopped(&self) -> bool {
        false
    }
    fn forward_injected_input(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

struct Fixture {
    forward: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    _cursor: tests::TestCursor,
    _physical_snapshot: MutexGuard<'static, ()>,
}
static PHYSICAL_SNAPSHOT: Mutex<()> = Mutex::new(());
impl Fixture {
    fn new() -> Self {
        let physical_snapshot = PHYSICAL_SNAPSHOT.lock().unwrap();
        external::gap();
        let cursor = tests::TestCursor::at(POINT { x: 1920, y: 1080 });
        let events = Arc::new(Mutex::new(Vec::<CapturedEvent>::new()));
        let output = events.clone();
        let mut state = tests::remote_state();
        state.handler = Arc::new(move |event| {
            output.lock().unwrap().push(event.clone());
            Disposition::Swallow
        });
        STATE.with(|slot| {
            assert!(slot.borrow_mut().replace(state).is_none());
        });
        let forward = Arc::new(AtomicBool::new(false));
        crate::capture_loop::install(Box::new(Owner(forward.clone())), Arc::new(|| {}));
        Self {
            forward,
            events,
            _cursor: cursor,
            _physical_snapshot: physical_snapshot,
        }
    }
    fn mouse(&self, msg: u32, flags: u32) -> isize {
        let info = MSLLHOOKSTRUCT {
            pt: POINT { x: 1920, y: 1080 },
            flags,
            mouseData: 120 << 16,
            ..Default::default()
        };
        mouse_proc_with_next(
            0,
            WPARAM(msg as usize),
            LPARAM((&info as *const MSLLHOOKSTRUCT) as isize),
            || LRESULT(0),
        )
        .0
    }
    fn key(&self, msg: u32, flags: u32) -> isize {
        let info = KBDLLHOOKSTRUCT {
            vkCode: 0x41,
            scanCode: 0x1e,
            flags: KBDLLHOOKSTRUCT_FLAGS(flags),
            ..Default::default()
        };
        keyboard_proc_with_next(
            0,
            WPARAM(msg as usize),
            LPARAM((&info as *const KBDLLHOOKSTRUCT) as isize),
            || LRESULT(0),
        )
        .0
    }
}

#[test]
fn held_snapshot_excludes_external_and_own_keys_and_gap_cannot_rebuild_them() {
    let f = Fixture::new();
    assert_eq!(f.key(WM_KEYDOWN, LLKHF_INJECTED.0), 0);
    assert!(physical_keys().is_empty());
    assert_eq!(f.key(WM_KEYDOWN, 0), 1);
    assert_eq!(physical_keys(), vec![KeyCode::new(0x07, 0x04)]);
    assert_eq!(f.key(WM_KEYUP, LLKHF_INJECTED.0), 0);
    assert_eq!(physical_keys(), vec![KeyCode::new(0x07, 0x04)]);
    external::gap();
    assert!(physical_keys().is_empty());
    assert_eq!(f.key(WM_KEYDOWN, LLKHF_INJECTED.0), 0);
    assert!(physical_keys().is_empty());
    let info = KBDLLHOOKSTRUCT {
        vkCode: 0x41,
        scanCode: 0x1e,
        dwExtraInfo: INJECT_TAG,
        ..Default::default()
    };
    assert_eq!(
        keyboard_proc_with_next(
            0,
            WPARAM(WM_KEYDOWN as usize),
            LPARAM((&info as *const KBDLLHOOKSTRUCT) as isize),
            || LRESULT(0)
        )
        .0,
        0
    );
    assert!(physical_keys().is_empty());
}
impl Drop for Fixture {
    fn drop(&mut self) {
        STATE.with(|slot| slot.borrow_mut().take());
        crate::capture_loop::clear();
        external::gap();
    }
}

#[test]
fn external_buttons_and_keys_do_not_release_physical_remote_holds_in_either_order() {
    for external_first in [false, true] {
        let f = Fixture::new();
        if external_first {
            assert_eq!(f.mouse(WM_LBUTTONDOWN, LLMHF_INJECTED), 0);
            assert_eq!(f.key(WM_KEYDOWN, LLKHF_INJECTED.0), 0);
        }
        assert_eq!(f.mouse(WM_LBUTTONDOWN, 0), 1);
        assert_eq!(f.key(WM_KEYDOWN, 0), 1);
        if !external_first {
            assert_eq!(f.mouse(WM_LBUTTONDOWN, LLMHF_INJECTED), 0);
            assert_eq!(f.key(WM_KEYDOWN, LLKHF_INJECTED.0), 0);
        }
        assert_eq!(f.mouse(WM_LBUTTONUP, LLMHF_INJECTED), 0);
        assert_eq!(f.key(WM_KEYUP, LLKHF_INJECTED.0), 0);
        assert_eq!(with_state(|s| s.keys.len()), Some(1));
        assert_eq!(f.events.lock().unwrap().len(), 2);
        assert_eq!(f.mouse(WM_LBUTTONUP, 0), 1);
        assert_eq!(f.key(WM_KEYUP, 0), 1);
        let events = f.events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|event| event.physical));
        assert_eq!(with_state(|s| s.keys.len()), Some(0));
        assert!(
            with_state(|s| s.continuity.transition(false, |_| false, 0))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn native_entrance_applies_live_toggle_only_after_external_chord_and_drag_end() {
    for before in [false, true] {
        let f = Fixture::new();
        f.forward.store(before, Ordering::Relaxed);
        assert_eq!(f.mouse(WM_LBUTTONDOWN, LLMHF_INJECTED), isize::from(before));
        f.forward.store(!before, Ordering::Relaxed);
        assert_eq!(f.key(WM_KEYDOWN, LLKHF_INJECTED.0), isize::from(before));
        assert_eq!(f.mouse(WM_LBUTTONUP, LLMHF_INJECTED), isize::from(before));
        assert_eq!(f.key(WM_KEYUP, LLKHF_INJECTED.0), isize::from(before));
        assert_eq!(f.events.lock().unwrap().len(), if before { 4 } else { 0 });
        assert_eq!(f.mouse(WM_MOUSEWHEEL, LLMHF_INJECTED), isize::from(!before));
        assert_eq!(f.events.lock().unwrap().len(), if before { 4 } else { 1 });
    }
}

#[test]
fn external_move_realigns_the_next_physical_step_without_changing_mode_or_restore_target() {
    let f = Fixture::new();
    let park = POINT { x: 1920, y: 1080 };
    let external = POINT { x: 0, y: 100 }; // A screen edge must not enter routing.
    let mut info = MSLLHOOKSTRUCT {
        pt: external,
        flags: LLMHF_INJECTED,
        ..Default::default()
    };
    let result = mouse_proc_with_next(
        0,
        WPARAM(WM_MOUSEMOVE as usize),
        LPARAM((&info as *const MSLLHOOKSTRUCT) as isize),
        || {
            assert_eq!(
                super::super::warp::sample(park),
                external,
                "register before downstream reentry"
            );
            LRESULT(0)
        },
    );
    assert_eq!(result.0, 0);
    assert!(f.events.lock().unwrap().is_empty());
    assert_eq!(with_state(|s| s.park), Some(park));
    assert!(with_state(|s| matches!(s.mode, CaptureMode::Remote { .. })).unwrap());
    info.pt = POINT { x: 3, y: 98 };
    info.flags = 0;
    assert_eq!(
        mouse_proc_with_next(
            0,
            WPARAM(WM_MOUSEMOVE as usize),
            LPARAM((&info as *const MSLLHOOKSTRUCT) as isize),
            || panic!("physical move stays swallowed")
        )
        .0,
        1
    );
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, InputEvent::PointerDelta { dx: 3., dy: -2. });
    assert_eq!(events[0].cursor, None);
}

#[test]
fn external_move_reentry_is_accounted_and_downstream_rejection_preserves_newer_commits() {
    let _f = Fixture::new();
    let park = POINT { x: 1920, y: 1080 };
    let newer = POINT { x: 400, y: 300 };
    let info = MSLLHOOKSTRUCT {
        pt: POINT { x: 50, y: 60 },
        flags: LLMHF_INJECTED,
        ..Default::default()
    };
    STATE.with(|slot| {
        let _borrow = slot.borrow_mut();
        let result = mouse_proc_with_next(
            0,
            WPARAM(WM_MOUSEMOVE as usize),
            LPARAM((&info as *const MSLLHOOKSTRUCT) as isize),
            || {
                super::super::warp::accepted(park, newer);
                LRESULT(17)
            },
        );
        assert_eq!(result.0, 17);
        assert_eq!(super::super::warp::sample(park), newer);
    });
}

#[test]
fn default_local_path_covers_lower_il_unknown_keys_all_buttons_and_both_wheels() {
    let f = Fixture::new();
    for flags in [
        LLMHF_INJECTED,
        LLMHF_LOWER_IL_INJECTED,
        LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED,
    ] {
        for msg in [
            WM_MOUSEMOVE,
            WM_LBUTTONDOWN,
            WM_LBUTTONUP,
            WM_RBUTTONDOWN,
            WM_RBUTTONUP,
            WM_MBUTTONDOWN,
            WM_MBUTTONUP,
            WM_XBUTTONDOWN,
            WM_XBUTTONUP,
            WM_MOUSEWHEEL,
            WM_MOUSEHWHEEL,
        ] {
            assert_eq!(f.mouse(msg, flags), 0);
        }
    }
    for flags in [LLKHF_INJECTED.0, LLKHF_LOWER_IL_INJECTED.0] {
        for msg in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
            let info = KBDLLHOOKSTRUCT {
                vkCode: 0xe7,
                flags: KBDLLHOOKSTRUCT_FLAGS(flags),
                ..Default::default()
            };
            assert_eq!(
                keyboard_proc_with_next(
                    0,
                    WPARAM(msg as usize),
                    LPARAM((&info as *const KBDLLHOOKSTRUCT) as isize),
                    || LRESULT(23)
                )
                .0,
                23
            );
        }
    }
    assert!(f.events.lock().unwrap().is_empty());
    assert_eq!(with_state(|s| s.keys.len()), Some(0));
}
