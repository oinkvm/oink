//! Quicker compatibility mode: the real mouse hook entrance inside an Oink
//! injection window. No hook is installed and nothing is injected.
use super::*;
use crate::quicker_compat::{MOUSE, WINDOW};

const DOWNS: [u32; 7] = [
    WM_MOUSEMOVE,
    WM_LBUTTONDOWN,
    WM_RBUTTONDOWN,
    WM_MBUTTONDOWN,
    WM_XBUTTONDOWN,
    WM_MOUSEWHEEL,
    WM_MOUSEHWHEEL,
];
const UPS: [u32; 4] = [WM_LBUTTONUP, WM_RBUTTONUP, WM_MBUTTONUP, WM_XBUTTONUP];

/// A capture gap would release nothing: no press is left in the ledger.
/// The probe gap is closed again so later events still reach the handler.
fn no_stale_releases() -> bool {
    with_state(|s| {
        let released = s.continuity.transition(false, |_| false, 0);
        s.continuity.transition(true, |_| false, 0);
        released.is_empty()
    })
    .unwrap()
}

#[test]
fn injected_non_release_mouse_input_inside_the_window_never_enters_capture() {
    let f = Fixture::new();
    f.forward.store(true, Ordering::Relaxed); // the default: routed otherwise
    MOUSE.arm(Instant::now());
    for flags in [LLMHF_INJECTED, LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED] {
        for msg in DOWNS {
            assert_eq!(f.mouse(msg, flags), 0, "passed to the next hook: {msg:#x}");
        }
    }
    assert!(f.events.lock().unwrap().is_empty());
    assert!(no_stale_releases());
    // Outside the window (elapsed: see the unit tests) the same injected DOWN is routed.
    let now = Instant::now(); // the window was armed at or before this instant
    assert!(!quicker_window_skips(
        WM_RBUTTONDOWN,
        &MSLLHOOKSTRUCT {
            flags: LLMHF_INJECTED,
            ..Default::default()
        },
        now + WINDOW
    ));
    MOUSE.clear();
    assert_eq!(f.mouse(WM_RBUTTONDOWN, LLMHF_INJECTED), 1);
    assert_eq!(f.events.lock().unwrap().len(), 1);
    assert!(!f.events.lock().unwrap()[0].physical);
}

#[test]
fn every_release_and_all_physical_input_keep_the_ordinary_path_inside_the_window() {
    let f = Fixture::new();
    f.forward.store(true, Ordering::Relaxed);
    MOUSE.arm(Instant::now());
    // Physical input is never affected: the Remote-mode fixture routes it.
    assert_eq!(f.mouse(WM_LBUTTONDOWN, 0), 1);
    assert_eq!(f.mouse(WM_LBUTTONUP, 0), 1);
    assert_eq!(f.events.lock().unwrap().len(), 2);
    assert!(f.events.lock().unwrap().iter().all(|e| e.physical));
    // Oink's own untagged press is skipped, its release is processed. The
    // release has no recorded press, so it leaves no ledger entry behind.
    for (down, up) in DOWNS[1..5].iter().zip(UPS) {
        assert_eq!(f.mouse(*down, LLMHF_INJECTED), 0);
        f.mouse(up, LLMHF_INJECTED);
    }
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 6);
    assert!(
        events[2..]
            .iter()
            .all(|e| !e.physical && matches!(e.event, InputEvent::Button { pressed: false, .. }))
    );
    drop(events);
    assert!(no_stale_releases());
}

#[test]
fn opted_out_release_inside_the_window_still_closes_the_external_ledger() {
    // forward_injected_input off: an external press before the window stays
    // local; its release inside the window must still close that ledger, or a
    // phantom hold would freeze the live toggle.
    let f = Fixture::new();
    assert_eq!(f.mouse(WM_LBUTTONDOWN, LLMHF_INJECTED), 0);
    MOUSE.arm(Instant::now());
    assert_eq!(f.mouse(WM_LBUTTONUP, LLMHF_INJECTED), 0);
    MOUSE.clear();
    f.forward.store(true, Ordering::Relaxed);
    assert_eq!(f.mouse(WM_RBUTTONDOWN, LLMHF_INJECTED), 1, "new toggle applies at once");
    assert_eq!(f.events.lock().unwrap().len(), 1);
}

#[test]
fn physical_hold_with_an_injected_press_and_release_of_the_same_button() {
    // The local user holds the left button; the remote injects left Down/Up.
    let f = Fixture::new();
    f.forward.store(true, Ordering::Relaxed);
    assert_eq!(f.mouse(WM_LBUTTONDOWN, 0), 1);
    MOUSE.arm(Instant::now());
    assert_eq!(f.mouse(WM_LBUTTONDOWN, LLMHF_INJECTED), 0, "skipped");
    f.mouse(WM_LBUTTONUP, LLMHF_INJECTED);
    // The injected Up closes the hold (as any injected Up already does with
    // forwarding on); the later physical Up is an unrecorded release.
    assert!(no_stale_releases());
    f.mouse(WM_LBUTTONUP, 0);
    assert!(no_stale_releases());
    let events = f.events.lock().unwrap();
    assert_eq!(
        events.iter().map(|e| (e.event.clone(), e.physical)).collect::<Vec<_>>(),
        [
            (btn(MouseButton::Left, true), true),
            (btn(MouseButton::Left, false), false),
            (btn(MouseButton::Left, false), true),
        ]
    );
}

#[test]
fn own_tagged_events_and_markers_keep_their_paths_inside_the_window() {
    let f = Fixture::new();
    MOUSE.arm(Instant::now());
    for extra in [INJECT_TAG, inject::SHAPE_REFRESH_TAG] {
        let info = MSLLHOOKSTRUCT {
            flags: LLMHF_INJECTED,
            dwExtraInfo: extra,
            ..Default::default()
        };
        assert!(!quicker_window_skips(WM_MOUSEMOVE, &info, Instant::now()));
        assert!(!quicker_window_skips(WM_LBUTTONDOWN, &info, Instant::now()));
    }
    let marker = MSLLHOOKSTRUCT {
        flags: LLMHF_INJECTED,
        dwExtraInfo: TRANSITION_TAG,
        ..Default::default()
    };
    assert!(!quicker_window_skips(WM_MOUSEMOVE, &marker, Instant::now()));
    let untagged = MSLLHOOKSTRUCT {
        flags: LLMHF_INJECTED,
        ..Default::default()
    };
    assert!(quicker_window_skips(WM_LBUTTONDOWN, &untagged, Instant::now()));
    assert!(!quicker_window_skips(WM_LBUTTONUP, &untagged, Instant::now()));
    let physical = MSLLHOOKSTRUCT::default();
    assert!(!quicker_window_skips(WM_LBUTTONDOWN, &physical, Instant::now()));
    assert!(f.events.lock().unwrap().is_empty());
}
