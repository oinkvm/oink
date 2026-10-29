//! Fixed-copy, partitioned M1 receive lanes. Network dispatch is the producer;
//! audio callback/control handover is the consumer. Neither endpoint waits.
use super::{DatagramError, QueueSpec};
use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Packet {
    bytes: [u8; oink_protocol::v1::limits::DATAGRAM],
    len: usize,
    arrived: Instant,
}
struct Slot {
    packet: UnsafeCell<Packet>,
    stamp: AtomicU64,
}
struct Lane {
    slots: Box<[Slot]>,
    read: AtomicUsize,
    write: AtomicUsize,
    bytes: AtomicUsize,
    // One bounded ownership claim also makes accidental concurrent public
    // readers safe. Failure means no work, never CAS retry/spin/wait.
    reading: AtomicBool,
    writing: AtomicBool,
    dropped: AtomicU64,
}
// SAFETY: an endpoint's single successful ownership claim serializes its cursor;
// release/acquire cursors protect unpublished/read slots from the other endpoint.
unsafe impl Sync for Lane {}
struct Claim<'a>(&'a AtomicBool);
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
fn claim(flag: &AtomicBool) -> Option<Claim<'_>> {
    flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| Claim(flag))
}
impl Lane {
    fn new(capacity: usize, epoch: Instant) -> Self {
        Self {
            slots: (0..=capacity)
                .map(|_| Slot {
                    packet: UnsafeCell::new(Packet {
                        bytes: [0; oink_protocol::v1::limits::DATAGRAM],
                        len: 0,
                        arrived: epoch,
                    }),
                    stamp: AtomicU64::new(0),
                })
                .collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            reading: AtomicBool::new(false),
            writing: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        }
    }
    fn next(&self, cursor: usize) -> usize {
        if cursor + 1 == self.slots.len() { 0 } else { cursor + 1 }
    }
    fn len(&self) -> usize {
        let r = self.read.load(Ordering::Acquire);
        let w = self.write.load(Ordering::Acquire);
        (w + self.slots.len() - r) % self.slots.len()
    }
    fn push(&self, bytes: &[u8], arrived: Instant, epoch: Instant, limit: usize) -> bool {
        let Some(_claim) = claim(&self.writing) else {
            return false;
        };
        let w = self.write.load(Ordering::Relaxed);
        let next = self.next(w);
        if next == self.read.load(Ordering::Acquire) || self.bytes.load(Ordering::Acquire) + bytes.len() > limit {
            return false;
        }
        // SAFETY: this producer owns the unpublished slot until write.release.
        let packet = unsafe { &mut *self.slots[w].packet.get() };
        packet.len = bytes.len();
        packet.arrived = arrived;
        packet.bytes[..bytes.len()].copy_from_slice(bytes);
        self.slots[w].stamp.store(
            arrived
                .saturating_duration_since(epoch)
                .as_nanos()
                .min(u128::from(u64::MAX - 1)) as u64
                + 1,
            Ordering::Relaxed,
        );
        self.bytes.fetch_add(bytes.len(), Ordering::Relaxed);
        self.write.store(next, Ordering::Release);
        true
    }
    fn pop(&self, out: &mut [u8]) -> Result<Option<(usize, Instant)>, DatagramError> {
        let Some(_claim) = claim(&self.reading) else {
            return Ok(None);
        };
        let r = self.read.load(Ordering::Relaxed);
        if r == self.write.load(Ordering::Acquire) {
            return Ok(None);
        }
        // SAFETY: this consumer owns the published slot until read.release.
        let packet = unsafe { &*self.slots[r].packet.get() };
        let result = if let Some(dst) = out.get_mut(..packet.len) {
            dst.copy_from_slice(&packet.bytes[..packet.len]);
            Ok(Some((packet.len, packet.arrived)))
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            Err(DatagramError::TooLarge)
        };
        self.bytes.fetch_sub(packet.len, Ordering::Relaxed);
        self.slots[r].stamp.store(0, Ordering::Relaxed);
        self.read.store(self.next(r), Ordering::Release);
        result
    }
}
pub(super) struct Partitions {
    types: [[u16; 2]; 2],
    lanes: [Lane; 2],
    epoch: Instant,
    byte_limits: [usize; 2],
}
impl Partitions {
    pub(super) fn new(spec: QueueSpec, types: [[u16; 2]; 2]) -> Self {
        let epoch = Instant::now();
        // Partition the existing total, rather than letting a dormant consumer
        // monopolize shared admission. No extra ingress or overflow reservoir.
        let count = spec.messages as usize;
        let bytes = spec.bytes as usize;
        Self {
            types,
            lanes: [Lane::new(count.div_ceil(2), epoch), Lane::new(count / 2, epoch)],
            epoch,
            byte_limits: [bytes.div_ceil(2), bytes / 2],
        }
    }
    pub(super) fn post(&self, bytes: &[u8], at: Instant) -> bool {
        let Ok(frame) = oink_protocol::v1::frame::OpaqueFrame::parse_exact(bytes, oink_protocol::v1::limits::DATAGRAM)
        else {
            return false;
        };
        let Some(lane) = self
            .types
            .iter()
            .position(|types| types.contains(&frame.header.type_id))
        else {
            return false;
        };
        if self.lanes[lane].push(bytes, at, self.epoch, self.byte_limits[lane]) {
            return true;
        }
        self.lanes[lane].dropped.fetch_add(1, Ordering::Relaxed);
        false
    }
    pub(super) fn take(&self, lane: usize, bytes: &mut [u8]) -> Result<Option<(usize, Instant)>, DatagramError> {
        self.lanes[lane].pop(bytes)
    }
    pub(super) fn dropped(&self, lane: usize) -> u64 {
        self.lanes[lane].dropped.load(Ordering::Relaxed)
    }
    pub(super) fn len(&self) -> usize {
        self.lanes.iter().map(Lane::len).sum()
    }
    pub(super) fn oldest(&self) -> Option<Instant> {
        self.lanes
            .iter()
            .flat_map(|lane| lane.slots.iter())
            .map(|s| s.stamp.load(Ordering::Acquire))
            .filter(|s| *s != 0)
            .min()
            .map(|s| self.epoch + Duration::from_nanos(s - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_readers_keep_order_while_other_lane_is_dormant() {
        let epoch = Instant::now();
        let lanes = std::sync::Arc::new([Lane::new(32, epoch), Lane::new(32, epoch)]);
        for seq in 0u64..32 {
            assert!(lanes[1].push(&seq.to_le_bytes(), epoch, epoch, 1024));
        }
        let writer = lanes.clone();
        let producer = std::thread::spawn(move || {
            for seq in 0u64..20_000 {
                while !writer[0].push(&seq.to_le_bytes(), epoch, epoch, 1024) {
                    std::thread::yield_now();
                }
                assert!(!writer[1].push(&seq.to_le_bytes(), epoch, epoch, 1024));
            }
        });
        let mut bytes = [0; 8];
        for seq in 0u64..20_000 {
            while lanes[0].pop(&mut bytes).unwrap().is_none() {
                std::thread::yield_now();
            }
            assert_eq!(u64::from_le_bytes(bytes), seq);
        }
        producer.join().unwrap();
        for seq in 0u64..32 {
            assert!(lanes[1].pop(&mut bytes).unwrap().is_some());
            assert_eq!(u64::from_le_bytes(bytes), seq);
        }
        assert_eq!(lanes[0].bytes.load(Ordering::Acquire), 0);
        assert_eq!(lanes[1].bytes.load(Ordering::Acquire), 0);
    }
}
