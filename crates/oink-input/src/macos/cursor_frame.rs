//! Submit display damage after a successful native cursor hide.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGEvent;

use super::system;

type ConnectionFn = unsafe extern "C" fn() -> i32;
type RegionFn = unsafe extern "C" fn(*const CGRect, *mut *mut CFType) -> i32;
type CreateFn = unsafe extern "C" fn(i32) -> *mut CFType;
type UpdateFn = unsafe extern "C" fn(*const CFType, *const CFType);
type CommitFn = unsafe extern "C" fn(*const CFType, u32);

struct Api {
    connection: ConnectionFn,
    region: RegionFn,
    create: CreateFn,
    update: UpdateFn,
    commit: CommitFn,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let connection = system::find_symbol(c"SLSMainConnectionID");
        let region = system::find_symbol(c"CGSNewRegionWithRect");
        let create = system::find_symbol(c"SLSTransactionCreate");
        let update = system::find_symbol(c"SLSTransactionUpdateRegion");
        let commit = system::find_symbol(c"SLSTransactionCommit");
        if [connection, region, create, update, commit].iter().any(|p| p.is_null()) {
            return None;
        }
        // SAFETY: These signatures and CF ownership were checked against the macOS 14/26
        // client implementations. The transaction belongs to our own connection.
        Some(unsafe {
            Api {
                connection: std::mem::transmute::<*mut c_void, ConnectionFn>(connection),
                region: std::mem::transmute::<*mut c_void, RegionFn>(region),
                create: std::mem::transmute::<*mut c_void, CreateFn>(create),
                update: std::mem::transmute::<*mut c_void, UpdateFn>(update),
                commit: std::mem::transmute::<*mut c_void, CommitFn>(commit),
            }
        })
    })
    .as_ref()
}

/// Called outside the event callback, under cursor_lock, once per successful hide.
///
/// A background hardware-cursor hide can publish the hidden flag without producing the
/// display damage needed for a full update. Association is foreground-only on the server,
/// so an associate(true) reply is not a display flush. On macOS 26 a quiet desktop retained
/// the old hardware surface until a later full update; one nonempty update region ended
/// that wait. Invalidating one point on the cursor's display requests that update without
/// drawing content, posting input, moving the pointer, taking focus, or changing hide counts.
/// No window, repeated hide, timer, or wait for presentation is needed.
pub(super) fn request_update() -> Result<(), &'static str> {
    let api = api().ok_or("cursor display-update API unavailable")?;
    let event = CGEvent::new(None).ok_or("cursor position unavailable")?;
    submit(api, CGEvent::location(Some(&event)))
}

fn submit(api: &Api, point: CGPoint) -> Result<(), &'static str> {
    let rect = CGRect {
        origin: CGPoint {
            x: point.x.floor(),
            y: point.y.floor(),
        },
        size: CGSize {
            width: 1.0,
            height: 1.0,
        },
    };
    // SAFETY: The rectangle and out-pointer are valid for the call. Both constructors
    // return +1 CF objects, retained through commit and released on every return path.
    // UpdateRegion only invalidates display content; it does not paint the rectangle.
    unsafe {
        let mut raw_region = std::ptr::null_mut();
        let error = (api.region)(&rect, &mut raw_region);
        let region = NonNull::new(raw_region).map(|p| CFRetained::from_raw(p));
        if error != 0 {
            return Err("cursor update region creation failed");
        }
        let region = region.ok_or("cursor update region unavailable")?;
        let transaction = NonNull::new((api.create)((api.connection)()))
            .map(|p| CFRetained::from_raw(p))
            .ok_or("cursor update transaction unavailable")?;
        (api.update)(&*transaction, &*region);
        (api.commit)(&*transaction, 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use objc2_core_foundation::CFString;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum Call {
        Region(CGRect),
        Connection,
        Create(i32),
        Update,
        Commit(u32),
    }

    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
        region_error: i32,
        null_region: bool,
        null_transaction: bool,
        region: usize,
        transaction: usize,
    }

    thread_local! {
        static FAKE: RefCell<Fake> = RefCell::default();
    }

    // Real +1 CF objects exercise the helper's ownership without contacting WindowServer.
    fn object(label: &str) -> *mut CFType {
        CFRetained::into_raw(CFString::from_str(label)).cast().as_ptr()
    }

    unsafe extern "C" fn region(rect: *const CGRect, out: *mut *mut CFType) -> i32 {
        FAKE.with_borrow_mut(|f| {
            // SAFETY: submit supplies live rectangle and out-pointer arguments.
            f.calls.push(Call::Region(unsafe { *rect }));
            if f.region_error == 0 && !f.null_region {
                let p = object("cursor-frame-test-region");
                f.region = p as usize;
                // SAFETY: submit provides a writable pointer and assumes ownership of +1.
                unsafe { *out = p };
            }
            f.region_error
        })
    }

    unsafe extern "C" fn connection() -> i32 {
        FAKE.with_borrow_mut(|f| f.calls.push(Call::Connection));
        17
    }

    unsafe extern "C" fn create(connection: i32) -> *mut CFType {
        FAKE.with_borrow_mut(|f| {
            f.calls.push(Call::Create(connection));
            if f.null_transaction {
                return std::ptr::null_mut();
            }
            let p = object("cursor-frame-test-transaction");
            f.transaction = p as usize;
            p
        })
    }

    unsafe extern "C" fn update(transaction: *const CFType, region: *const CFType) {
        FAKE.with_borrow_mut(|f| {
            assert_eq!(transaction as usize, f.transaction);
            assert_eq!(region as usize, f.region);
            f.calls.push(Call::Update);
        });
    }

    unsafe extern "C" fn commit(transaction: *const CFType, flags: u32) {
        FAKE.with_borrow_mut(|f| {
            assert_eq!(transaction as usize, f.transaction);
            f.calls.push(Call::Commit(flags));
        });
    }

    fn fake_api() -> Api {
        Api {
            connection,
            region,
            create,
            update,
            commit,
        }
    }

    #[test]
    fn commits_one_nonempty_region_on_the_cursor_display() {
        for point in [CGPoint::new(15.75, 3.5), CGPoint::new(-1919.25, -0.25)] {
            FAKE.set(Fake::default());
            assert_eq!(submit(&fake_api(), point), Ok(()));
            FAKE.with_borrow(|f| {
                assert_eq!(
                    f.calls,
                    [
                        Call::Region(CGRect::new(
                            CGPoint::new(point.x.floor(), point.y.floor()),
                            CGSize::new(1.0, 1.0),
                        )),
                        Call::Connection,
                        Call::Create(17),
                        Call::Update,
                        Call::Commit(1),
                    ]
                );
            });
        }
    }

    #[test]
    fn failed_region_never_creates_or_commits_a_transaction() {
        for fake in [
            Fake {
                region_error: 1000,
                ..Default::default()
            },
            Fake {
                null_region: true,
                ..Default::default()
            },
        ] {
            FAKE.set(fake);
            assert!(submit(&fake_api(), CGPoint::new(1.0, 2.0)).is_err());
            FAKE.with_borrow(|f| assert!(matches!(f.calls.as_slice(), [Call::Region(_)])));
        }
    }

    #[test]
    fn failed_transaction_is_not_updated_or_committed() {
        FAKE.set(Fake {
            null_transaction: true,
            ..Default::default()
        });
        assert!(submit(&fake_api(), CGPoint::new(1.0, 2.0)).is_err());
        FAKE.with_borrow(|f| {
            assert!(matches!(
                f.calls.as_slice(),
                [Call::Region(_), Call::Connection, Call::Create(17)]
            ));
        });
    }
}
