//! The sole native cursor-placement boundary. Placement uses tagged SendInput;
//! confinement refuses an implicit relocation. SendInput (including the privileged
//! helper and DnD's combined MOVE|DOWN) registers the resolved pixel in its tagged
//! hook; same-thread placement also registers before submission.
//!
//! Windows 11 can deliver a target based on a new absolute commit while
//! GetCursorPos still returns the displaced position. Register before calling
//! the OS, then use one accounted snapshot for classification AND subtraction.
//! Merely ignoring our injected event does not protect the next hardware event.
//! This exact readback repair has no rate limit, delta threshold or expiry.
//! The existing old-coordinate-flight heuristic remains a separate classifier.

use std::cell::RefCell;

use oink_foundation::Point;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, SendInput};
use windows::Win32::UI::WindowsAndMessaging::{
    ClipCursor, GetClipCursor, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetCursorPos,
};

use crate::stale_step::StaleSteps;
use crate::warp_base::{Accounting, Registration};

thread_local! {
    // Cursor/clip APIs run on the hook owner. Injection workers do not touch
    // this ledger: their tagged callback, with OS-resolved coordinates, does.
    static BASE: RefCell<Accounting> = RefCell::new(Accounting::default());
}

fn point(p: POINT) -> Point {
    Point::new(f64::from(p.x), f64::from(p.y))
}

/// A park may be rejected by a later LL hook. Rollback is conditional on this
/// still being the newest registration, so reentrant/newer placements survive.
pub(super) struct Acceptance((Registration, StaleSteps));

pub(super) fn accepted(before: POINT, target: POINT) -> Acceptance {
    Acceptance(BASE.with_borrow_mut(|base| {
        base.register(
            point(before),
            point(target),
            super::hook::time_us(std::time::Instant::now()),
        )
    }))
}

pub(super) fn accepted_refresh(before: POINT, target: POINT) -> Acceptance {
    Acceptance(BASE.with_borrow_mut(|base| base.register_refresh(point(before), point(target))))
}

pub(super) fn accepted_external(before: Option<POINT>, target: POINT) -> Acceptance {
    if let Some(before) = before {
        return accepted(before, target);
    }
    Acceptance(BASE.with_borrow_mut(|accounting| {
        let previous = accounting.stale;
        let before = accounting.base.registered_target();
        let registration = accounting.base.register_unobserved(point(target));
        if let Some(before) = before.filter(|before| *before != point(target)) {
            accounting
                .stale
                .switched(before, super::hook::time_us(std::time::Instant::now()));
        } else if before.is_none() {
            accounting.stale.clear();
        }
        (registration, previous)
    }))
}

pub(super) fn rejected(acceptance: Acceptance) -> bool {
    BASE.with_borrow_mut(|base| base.cancel(acceptance.0))
}

pub(super) fn sample(observed: POINT) -> POINT {
    let p = BASE.with_borrow_mut(|base| base.base.sample(point(observed)));
    POINT {
        x: p.x as i32,
        y: p.y as i32,
    }
}

pub(super) fn passed(target: POINT) {
    BASE.with_borrow_mut(|base| base.base.passed(point(target)));
}

pub(super) fn stale_base_for(pt: POINT, current: POINT) -> Option<POINT> {
    BASE.with_borrow_mut(|base| {
        base.stale_base_for(
            point(pt),
            point(current),
            super::hook::time_us(std::time::Instant::now()),
        )
        .map(|p| POINT {
            x: p.x as i32,
            y: p.y as i32,
        })
    })
}

fn clamped(p: POINT, rect: RECT) -> POINT {
    POINT {
        x: p.x.clamp(rect.left, rect.right.saturating_sub(1).max(rect.left)),
        y: p.y.clamp(rect.top, rect.bottom.saturating_sub(1).max(rect.top)),
    }
}

/// Tagged placement (held anchor, in-hook restore, shape refresh).
/// Ordinary cover parking uses an allowed native INPUT instead. Accounting is
/// not hide permission for either path.
pub(super) fn place(p: POINT) -> windows::core::Result<()> {
    place_tagged(p, super::inject::INJECT_TAG)
}

/// Only cursor::nudge uses this provenance; ordinary park/restore stays a real placement.
pub(super) fn refresh(p: POINT) -> windows::core::Result<()> {
    place_tagged(p, super::inject::SHAPE_REFRESH_TAG)
}

fn place_tagged(p: POINT, tag: usize) -> windows::core::Result<()> {
    let before = super::hook::cursor_pos_checked()?;
    #[cfg(all(test, debug_assertions))]
    if super::hook::test_place_cursor(p) {
        if tag == super::inject::SHAPE_REFRESH_TAG {
            accepted_refresh(before, p);
        } else {
            accepted(before, p);
        }
        return Ok(());
    }
    let mut clip = RECT::default();
    // SAFETY: physical-coordinate rectangle on the PMv2 hook owner.
    let target = if unsafe { GetClipCursor(&mut clip) }.is_ok() {
        clamped(p, clip)
    } else {
        p
    };
    let acceptance = if tag == super::inject::SHAPE_REFRESH_TAG {
        accepted_refresh(before, target)
    } else {
        accepted(before, target)
    };
    // SetCursorPos cannot carry a per-event marker. Use the same tagged,
    // acceleration-free absolute INPUT as ordinary remote placement instead.
    // Windows resolves clipping; accounting is registered before possible hook
    // reentry, including same-thread SendInput that bypasses our own hook on
    // Windows 11. No hook ACK is awaited from the hook thread.
    // SAFETY: system-metric queries have no borrowed handles or pointer arguments.
    let input = unsafe {
        let mut input = super::inject::move_abs(
            super::inject::normalize(
                p.x,
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
            ),
            super::inject::normalize(
                p.y,
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            ),
        );
        input.Anonymous.mi.dwExtraInfo = tag;
        input
    };
    let result = if send(&[input]) == 1 {
        Ok(())
    } else {
        Err(windows::core::Error::new(
            E_FAIL,
            "tagged cursor placement was rejected",
        ))
    };
    if result.is_err() {
        rejected(acceptance);
    }
    result
}

/// Only the bounded Local restore safety reset bypasses tagged input. Preserve
/// displacement accounting before possible native reentry, then verify readback
/// at the caller. Do not reuse this for ordinary parking or cursor refresh.
pub(super) fn restore_synchronously(target: POINT) -> windows::core::Result<()> {
    let before = super::hook::cursor_pos_checked()?;
    let acceptance = accepted(before, target);
    #[cfg(all(test, debug_assertions))]
    if super::hook::test_place_cursor(target) {
        return Ok(());
    }
    // SAFETY: physical point clamped to a real monitor; only the Local hook owner
    // calls this once for its current restore generation, outside a LL callback.
    let result = unsafe { SetCursorPos(target.x, target.y) };
    if result.is_err() {
        rejected(acceptance);
    }
    result
}

/// Confinement must not relocate the cursor. The crash guard uses the same
/// contract, but has no capture loop after the parent's death.
pub(super) fn clip(rect: RECT) -> windows::core::Result<()> {
    let before = super::hook::cursor_pos_checked()?;
    // ClipCursor has no event marker argument. It is a confinement operation,
    // never a relocation mechanism: callers must first place the cursor with
    // tagged input. Reject a rectangle that would visibly clamp it. Acquisition
    // runs before raw MOVE is allowed; restoration retains the covered park.
    if rect.left >= rect.right || rect.top >= rect.bottom || clamped(before, rect) != before {
        return Err(windows::core::Error::new(
            E_INVALIDARG,
            "cursor confinement would relocate the pointer",
        ));
    }
    #[cfg(all(test, debug_assertions))]
    if super::hook::test_place_cursor(before) {
        return Ok(());
    }
    // No displacement registration: a no-op would retire the still-needed
    // placement base for older queued motion. RawCapture checks lease ownership.
    // SAFETY: valid physical rectangle containing the current park position.
    unsafe { ClipCursor(Some(&rect)) }
}

/// The native commit is accounted in the receiving hook, not on the submitting
/// thread: Windows resolves absolute normalization/clipping before that callback.
pub(super) fn send(inputs: &[INPUT]) -> u32 {
    // SAFETY: callers construct fully initialized INPUT values.
    unsafe { SendInput(inputs, size_of::<INPUT>() as i32) }
}

#[cfg(all(test, debug_assertions))]
pub(super) fn reset() {
    BASE.with_borrow_mut(|base| *base = Accounting::default());
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::super::cursor::test_native as native;
    use super::*;

    #[test]
    fn external_commits_realign_base_without_forwarding_the_absolute_displacement() {
        let park = POINT { x: 1000, y: 600 };
        let external = POINT { x: 300, y: 200 };
        let _native = native::Guard::at(park);
        accepted(park, external);
        let target = POINT { x: 303, y: 198 };
        for readback in [park, external, park] {
            let current = sample(readback);
            let base = stale_base_for(target, current).unwrap_or(current);
            assert_eq!((target.x - base.x, target.y - base.y), (3, -2));
        }
        // Re-parking is a different commit; the old external-based step stays small.
        accepted(external, park);
        assert_eq!(stale_base_for(target, sample(park)), Some(external));
    }

    #[test]
    fn rejected_external_move_restores_exact_previous_ledger_and_stale_candidates() {
        let park = POINT { x: 1000, y: 600 };
        let entry = POINT { x: 300, y: 200 };
        let _native = native::Guard::at(park);
        accepted(park, entry);
        let rejected_move = accepted(entry, POINT { x: 50, y: 60 });
        rejected(rejected_move);
        // In particular, registration must snapshot BEFORE retiring an observed target.
        assert_eq!(sample(park), entry);
        assert_eq!(stale_base_for(POINT { x: 1001, y: 600 }, entry), Some(park));
    }

    #[test]
    fn clip_never_performs_an_untagged_cursor_relocation() {
        let from = POINT { x: 500, y: 400 };
        let _native = native::Guard::at(from);
        let outside = RECT {
            left: 100,
            top: 200,
            right: 101,
            bottom: 201,
        };
        assert!(clip(outside).is_err());
        assert_eq!(super::super::hook::cursor_pos(), from);
    }

    #[test]
    fn displaced_hook_targets_still_use_their_own_base_after_the_flight_window() {
        let departure = Point::new(501., 400.);
        let park = Point::new(1920., 1080.);
        let mut accounting = Accounting::default();
        accounting.register(departure, park, 0);
        // A correctly based callback retires the short in-flight heuristic.
        let fresh = Point::new(1921., 1080.);
        assert_eq!(accounting.stale_base_for(fresh, park, 0), None);
        let late = crate::stale_step::STALE_WINDOW_US + 10_000_000;
        for (native, target, expected) in [
            (park, Point::new(500., 402.), Point::new(-1., 2.)),
            (departure, Point::new(500., 400.), Point::new(-1., 0.)),
            (park, Point::new(501., 399.), Point::new(0., -1.)),
        ] {
            let current = accounting.base.sample(native);
            let base = accounting.stale_base_for(target, current, late).unwrap_or(current);
            assert_eq!(Point::new(target.x - base.x, target.y - base.y), expected);
        }
    }

    #[test]
    fn refresh_placement_source_preserves_park_motion_and_real_departure() {
        let departure = POINT { x: 500, y: 2100 };
        let park = POINT { x: 1920, y: 1080 };
        let _native = native::Guard::at(park);
        accepted(departure, park);
        refresh(POINT {
            x: park.x + 1,
            y: park.y,
        })
        .unwrap();
        refresh(park).unwrap();
        let current = sample(park);
        let next = POINT {
            x: park.x + 4,
            y: park.y - 2,
        };
        assert_eq!(stale_base_for(next, current), None);
        assert_eq!(sample(departure), park);
        assert_eq!(
            stale_base_for(
                POINT {
                    x: departure.x + 4,
                    y: departure.y - 2
                },
                current
            ),
            Some(departure)
        );
    }

    #[test]
    fn downstream_rejection_cannot_undo_a_newer_reentrant_native_commit() {
        let from = POINT { x: 2170, y: 2159 };
        let park = POINT { x: 1920, y: 1080 };
        let newer = POINT { x: 1800, y: 2159 };
        let _native = native::Guard::at(from);
        let old = accepted(from, park);
        accepted(from, newer);
        rejected(old);
        assert_eq!(sample(from), newer);
    }

    #[test]
    fn tagged_placement_and_nonmoving_clip_changes_share_accounting() {
        let from = POINT { x: 2170, y: 2159 };
        let park = POINT { x: 1920, y: 1080 };
        let _native = native::Guard::at(from);
        place(park).unwrap();
        assert_eq!(sample(from), park);
        assert_eq!(sample(park), park);
        clip(RECT {
            left: park.x,
            top: park.y,
            right: park.x + 1,
            bottom: park.y + 1,
        })
        .unwrap();
        assert_eq!(super::super::hook::cursor_pos(), park);
        clip(RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        })
        .unwrap();
        assert_eq!(super::super::hook::cursor_pos(), park);
        assert_eq!(sample(from), park);
    }
}
