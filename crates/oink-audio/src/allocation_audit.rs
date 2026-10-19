//! Thread-local heap audit for real-time data-path tests. The audit is enabled
//! only after constructing and warming all objects, and disabled before assert.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! {
    static AUDIT: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}
struct Audited;
fn count(allocation: bool) {
    let _ = AUDIT.try_with(|state| {
        if let Some((allocations, frees)) = state.get() {
            state.set(Some((
                allocations + usize::from(allocation),
                frees + usize::from(!allocation),
            )));
        }
    });
}
// SAFETY: delegates all pointer/layout operations unchanged to System. Audit
// state is a const-initialized TLS Cell and does not recursively allocate.
unsafe impl GlobalAlloc for Audited {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(true);
        // SAFETY: forward the caller's valid GlobalAlloc pointer/layout contract unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(true);
        // SAFETY: forward the caller's valid GlobalAlloc pointer/layout contract unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(true);
        count(false);
        // SAFETY: forward the caller's valid GlobalAlloc pointer/layout contract unchanged.
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(false);
        // SAFETY: forward the caller's valid GlobalAlloc pointer/layout contract unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Audited = Audited;

pub(crate) fn assert_no_heap(work: impl FnOnce()) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            AUDIT.set(None);
        }
    }
    assert!(AUDIT.get().is_none(), "nested allocation audit");
    AUDIT.set(Some((0, 0)));
    let reset = Reset;
    work();
    let counts = AUDIT.get().unwrap();
    drop(reset);
    assert_eq!(counts, (0, 0), "real-time work allocated or freed heap storage");
}
