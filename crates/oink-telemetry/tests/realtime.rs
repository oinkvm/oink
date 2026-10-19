use oink_telemetry::{Metric, realtime::Ring};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    time::{Duration, Instant},
};
struct Counting;
thread_local! {static TRACK:Cell<bool>=const {Cell::new(false)};static ALLOCATIONS:Cell<usize>=const {Cell::new(0)};}
// SAFETY: forwards exactly the original allocation contract to System.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        TRACK.with(|active| {
            if active.get() {
                ALLOCATIONS.with(|n| n.set(n.get() + 1));
            }
        });
        // SAFETY: forwarding the caller's valid allocation layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: forwarding the pointer and original allocation layout.
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        TRACK.with(|active| {
            if active.get() {
                ALLOCATIONS.with(|n| n.set(n.get() + 1));
            }
        });
        // SAFETY: forwarding the caller's valid reallocation contract.
        unsafe { System.realloc(p, layout, new_size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
#[test]
fn hot_path_has_zero_allocations_enabled_off_full_and_stale() {
    let at = Instant::now();
    let end = at + Duration::from_micros(120);
    let ring = Ring::new(8, at, 42);
    ring.arm(1, at);
    let stamp = ring.stamp(at).unwrap();
    ALLOCATIONS.with(|n| n.set(0));
    TRACK.with(|t| t.set(true));
    for _ in 0..16 {
        ring.record(stamp, Metric::InputReceiveToNativeComplete, end);
        let _ = ring.input_stamp(at, true);
    }
    ring.disarm();
    for _ in 0..16 {
        ring.record(stamp, Metric::InputReceiveToNativeComplete, end);
        assert!(ring.stamp(at).is_none());
    }
    ring.arm(2, at);
    ring.record(stamp, Metric::InputReceiveToNativeComplete, end);
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    let mut count = 0;
    while ring.pop().is_some() {
        count += 1;
    }
    assert_eq!(count, 8);
}
