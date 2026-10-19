//! Bounded single-producer/single-consumer hand-off for audio data.
//!
//! Construct and destroy endpoints on ordinary threads. Each hot operation has
//! a fixed number of atomic loads/stores and one Copy payload transfer: no CAS
//! retries, allocation, locks, wakeups, or payload destructors.
use std::{
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[repr(align(128))]
struct Cursor(AtomicUsize);
struct Shared<T: Copy> {
    slots: Box<[UnsafeCell<T>]>,
    read: Cursor,
    write: Cursor,
}
// SAFETY: only the unique Producer writes the unpublished slot, and only the
// unique Consumer reads a published slot. Release/acquire on write publishes
// its contents; release/acquire on read prevents reuse while it is being read.
// Endpoints cannot be cloned or shared (Cell marker) and operations need &mut.
unsafe impl<T: Copy + Send> Sync for Shared<T> {}

pub struct Producer<T: Copy> {
    shared: Arc<Shared<T>>,
    cursor: usize,
    _single_owner: PhantomData<Cell<()>>,
}
pub struct Consumer<T: Copy> {
    shared: Arc<Shared<T>>,
    cursor: usize,
    _single_owner: PhantomData<Cell<()>>,
}

impl<T: Copy> std::fmt::Debug for Producer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Producer")
            .field("capacity", &(self.shared.slots.len() - 1))
            .finish_non_exhaustive()
    }
}
impl<T: Copy> std::fmt::Debug for Consumer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consumer")
            .field("capacity", &(self.shared.slots.len() - 1))
            .finish_non_exhaustive()
    }
}

/// Allocate and touch all slots before the stream is started. `capacity` is the
/// usable capacity (one extra slot distinguishes full from empty).
pub fn channel<T: Copy + Send>(capacity: usize, initial: T) -> (Producer<T>, Consumer<T>) {
    assert!(capacity > 0 && capacity < usize::MAX);
    let shared = Arc::new(Shared {
        slots: (0..=capacity).map(|_| UnsafeCell::new(initial)).collect(),
        read: Cursor(AtomicUsize::new(0)),
        write: Cursor(AtomicUsize::new(0)),
    });
    (
        Producer {
            shared: shared.clone(),
            cursor: 0,
            _single_owner: PhantomData,
        },
        Consumer {
            shared,
            cursor: 0,
            _single_owner: PhantomData,
        },
    )
}
fn advance(cursor: usize, slots: usize) -> usize {
    if cursor + 1 == slots { 0 } else { cursor + 1 }
}
impl<T: Copy> Producer<T> {
    /// Full means drop/record according to the stream's policy; never wait.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let next = advance(self.cursor, self.shared.slots.len());
        if next == self.shared.read.0.load(Ordering::Acquire) {
            return Err(value);
        }
        // SAFETY: consumer released this slot; only this endpoint can write it.
        unsafe {
            *self.shared.slots[self.cursor].get() = value;
        }
        self.cursor = next;
        self.shared.write.0.store(next, Ordering::Release);
        Ok(())
    }
}
impl<T: Copy> Consumer<T> {
    pub fn pop(&mut self) -> Option<T> {
        if self.cursor == self.shared.write.0.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: producer published this initialized slot and cannot overwrite
        // it before the release below. Copy cannot run user code or Drop.
        let value = unsafe { *self.shared.slots[self.cursor].get() };
        self.cursor = advance(self.cursor, self.shared.slots.len());
        self.shared.read.0.store(self.cursor, Ordering::Release);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_capacity_and_wrap_preserve_rejected_value_and_order() {
        for capacity in [1, 3, 8, 54, 108] {
            let (mut tx, mut rx) = channel(capacity, 0);
            for round in 0..100 {
                assert_eq!(rx.pop(), None);
                for i in 0..capacity {
                    assert_eq!(tx.push(round * capacity + i), Ok(()));
                }
                assert_eq!(tx.push(usize::MAX), Err(usize::MAX));
                for i in 0..capacity {
                    assert_eq!(rx.pop(), Some(round * capacity + i));
                }
            }
        }
    }
    #[test]
    fn concurrent_wrap_never_tears_or_reorders_payloads() {
        let (mut tx, mut rx) = channel(3, [0_u64; 16]);
        let writer = std::thread::spawn(move || {
            for sequence in 1..=100_000 {
                let value = [sequence; 16];
                while tx.push(value).is_err() {
                    std::thread::yield_now();
                }
            }
            tx
        });
        for sequence in 1..=100_000 {
            let value = loop {
                if let Some(value) = rx.pop() {
                    break value;
                }
                std::thread::yield_now();
            };
            assert_eq!(value, [sequence; 16]);
        }
        drop(writer.join().unwrap());
        assert_eq!(rx.pop(), None);
    }
}
