//! Synchronous cursor-write ownership for the source's frozen display region.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};

use super::system;

type ConnectionFn = unsafe extern "C" fn() -> i32;
type RegionFn = unsafe extern "C" fn(*const CGRect, *mut *mut CFType) -> i32;
type LockFn = unsafe extern "C" fn(i32, *const CFType) -> i32;

struct Api {
    connection: ConnectionFn,
    region: RegionFn,
    lock: LockFn,
}
static API: OnceLock<Option<Api>> = OnceLock::new();
static OWNED: AtomicI32 = AtomicI32::new(0);

fn api() -> Option<&'static Api> {
    API.get_or_init(|| {
        let connection = system::find_symbol(c"SLSMainConnectionID");
        let region = system::find_symbol(c"CGSNewRegionWithRect");
        let lock = system::find_symbol(c"SLSSetCursorRegionLock");
        if [connection, region, lock].iter().any(|p| p.is_null()) {
            return None;
        }
        // SAFETY: Signatures/CF ownership checked against the macOS 26 client and
        // server implementations. Lock accepts a borrowed region; null releases it.
        Some(unsafe {
            Api {
                connection: std::mem::transmute::<*mut c_void, ConnectionFn>(connection),
                region: std::mem::transmute::<*mut c_void, RegionFn>(region),
                lock: std::mem::transmute::<*mut c_void, LockFn>(lock),
            }
        })
    })
    .as_ref()
}

/// Called under cursor_lock, outside the event tap, before the first Hide.
///
/// On macOS 26.6.2, Mission Control admits a background Hide (and NSCursor::set)
/// without applying either, even after our transparent panel wins the window hit
/// test. Damage and a balanced Show/Hide do not fix admission. A cursor region
/// lock grants this connection cursor-write ownership synchronously, so that the
/// first Hide applies while Mission Control stays open. This API does not confine,
/// warp or post mouse input; the server records only a cursor owner and its region.
/// It refuses another process's existing lock instead of replacing it.
/// Cover all active displays through the hidden return warp; leaving the region
/// before Show can surrender ownership. Release only after balancing our Hide,
/// including pause/stop/exit. The existing transparent panel still handles edge
/// departure and glyph retirement; no foreground activation or retry is added.
/// macOS 14.7 has not been verified. If any private symbol is absent, silently
/// retain the existing Hide/transparent-panel behavior. A fatal process exit also
/// closes the owning WindowServer connection, which retires its region lock.
pub(super) fn acquire() {
    if OWNED.load(Ordering::Acquire) != 0 {
        return;
    }
    let Some(api) = api() else {
        return;
    };
    let result = (|| {
        let bounds = super::display_bounds();
        let region_bounds = bounds
            .into_iter()
            .map(|r| CGRect::new(CGPoint::new(r.x, r.y), CGSize::new(r.w, r.h)))
            .reduce(|a, b| {
                let x = a.origin.x.min(b.origin.x);
                let y = a.origin.y.min(b.origin.y);
                CGRect::new(
                    CGPoint::new(x, y),
                    CGSize::new(
                        (a.origin.x + a.size.width).max(b.origin.x + b.size.width) - x,
                        (a.origin.y + a.size.height).max(b.origin.y + b.size.height) - y,
                    ),
                )
            })
            .ok_or("cursor-region displays unavailable")?;
        acquire_with(api, &OWNED, region_bounds)
    })();
    if let Err(reason) = result {
        tracing::warn!(reason, "source cursor-write ownership was not acquired");
    }
}

fn acquire_with(api: &Api, owned: &AtomicI32, bounds: CGRect) -> Result<(), &'static str> {
    if owned.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // SAFETY: Valid out-pointer and rectangle. The +1 region is retained through
    // the synchronous lock request and released even on rejection.
    unsafe {
        let mut raw = std::ptr::null_mut();
        let error = (api.region)(&bounds, &mut raw);
        let region = NonNull::new(raw).map(|p| CFRetained::from_raw(p));
        if error != 0 {
            return Err("cursor-region creation failed");
        }
        let region = region.ok_or("cursor-region unavailable")?;
        let cid = (api.connection)();
        if cid == 0 || (api.lock)(cid, &*region) != 0 {
            return Err("cursor-region ownership rejected");
        }
        owned.store(cid, Ordering::Release);
    }
    Ok(())
}

/// No AppKit, allocation, locks or lazy resolution: also used by exit/signal hooks.
/// A rejected acquisition never authorizes a release of another client's region.
fn release() {
    if let Some(Some(api)) = API.get() {
        release_with(api, &OWNED);
    }
}

/// Balance Show before surrendering cursor writes to Mission Control again.
pub(super) fn release_after<T>(restore: impl FnOnce() -> T) -> T {
    release_after_with(restore, release)
}

fn release_after_with<T>(restore: impl FnOnce() -> T, release: impl FnOnce()) -> T {
    let result = restore();
    release();
    result
}

fn release_with(api: &Api, owned: &AtomicI32) {
    let cid = owned.swap(0, Ordering::AcqRel);
    if cid != 0 {
        // SAFETY: Release only our accepted connection. The server rejects a
        // release if another process has since acquired the region.
        unsafe { (api.lock)(cid, std::ptr::null()) };
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use objc2_core_foundation::CFString;

    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: Vec<&'static str>,
        error: i32,
        region_error: i32,
    }
    thread_local! {
        static FAKE: RefCell<Fake> = RefCell::default();
    }
    unsafe extern "C" fn connection() -> i32 {
        17
    }
    unsafe extern "C" fn region(_: *const CGRect, out: *mut *mut CFType) -> i32 {
        FAKE.with_borrow_mut(|f| {
            f.calls.push("region");
            if f.region_error == 0 {
                // SAFETY: acquire_with provides writable storage and assumes +1 ownership.
                unsafe {
                    *out = CFRetained::into_raw(CFString::from_str("cursor-region-test"))
                        .cast()
                        .as_ptr()
                };
            }
            f.region_error
        })
    }
    unsafe extern "C" fn lock(cid: i32, region: *const CFType) -> i32 {
        assert_eq!(cid, 17);
        FAKE.with_borrow_mut(|f| {
            f.calls.push(if region.is_null() { "release" } else { "acquire" });
            f.error
        })
    }
    fn fake_api() -> Api {
        Api {
            connection,
            region,
            lock,
        }
    }

    #[test]
    fn restore_balances_show_before_releasing_even_when_show_fails() {
        for show_error in [0, 1000] {
            FAKE.set(Fake::default());
            let owned = AtomicI32::new(17);
            let result = release_after_with(
                || {
                    assert_eq!(owned.load(Ordering::Acquire), 17, "Show needs cursor-write ownership");
                    FAKE.with_borrow_mut(|f| f.calls.push("show"));
                    show_error
                },
                || release_with(&fake_api(), &owned),
            );
            assert_eq!(result, show_error);
            assert_eq!(owned.load(Ordering::Acquire), 0);
            FAKE.with_borrow(|f| assert_eq!(f.calls, ["show", "release"]));
        }
    }

    #[test]
    fn ownership_is_acquired_once_and_released_once() {
        FAKE.set(Fake::default());
        let owned = AtomicI32::new(0);
        let api = fake_api();
        for _ in 0..3 {
            assert_eq!(acquire_with(&api, &owned, CGRect::ZERO), Ok(()));
        }
        assert_eq!(owned.load(Ordering::Acquire), 17);
        release_with(&api, &owned);
        release_with(&api, &owned);
        assert_eq!(owned.load(Ordering::Acquire), 0);
        FAKE.with_borrow(|f| assert_eq!(f.calls, ["region", "acquire", "release"]));
    }

    #[test]
    fn rejected_ownership_never_releases_another_clients_lock() {
        for (region_error, error, calls) in [(1000, 0, vec!["region"]), (0, 1004, vec!["region", "acquire"])] {
            FAKE.set(Fake {
                region_error,
                error,
                ..Default::default()
            });
            let owned = AtomicI32::new(0);
            let api = fake_api();
            assert!(acquire_with(&api, &owned, CGRect::ZERO).is_err());
            release_with(&api, &owned);
            FAKE.with_borrow(|f| assert_eq!(f.calls, calls));
        }
    }
}
