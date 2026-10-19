//! Bounded receiver and native lifecycle, independently testable without injection.
use oink_foundation::contacts::{Contact, Frame, MAX_CHANGES, MAX_CONTACTS, Phase, Surface};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pointer {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub flags: u32,
}
pub(crate) trait Sink {
    fn create(&mut self, surface: Surface) -> bool;
    fn frame(&mut self, contacts: &[Pointer], tick: u32) -> bool;
    fn close(&mut self);
}
#[derive(Debug, Default)]
pub(crate) struct Receiver {
    active: bool,
    device: u32,
    surface: Surface,
    slots: [Option<Contact>; MAX_CONTACTS],
    first_us: u64,
    last_us: u64,
    tick: u32,
    begin_tick: u32,
    frames: u64,
    submitted: crate::contact_trace::Submitted,
}
impl Receiver {
    pub fn active(&self) -> bool {
        self.active
    }
    /// Focus leave, peer detach, secure desktop, shutdown and malformed input all
    /// share this path. A failed cancel closes the native device immediately.
    pub fn cancel(&mut self, sink: &mut impl Sink) -> bool {
        if !self.active {
            self.close(sink);
            return true;
        }
        let mut points = [Pointer::default(); MAX_CONTACTS];
        let mut len = 0;
        for (slot, contact) in self.slots.iter().enumerate() {
            if let Some(c) = contact {
                points[len] = Pointer {
                    id: slot as u32 + 1,
                    x: c.x,
                    y: c.y,
                    flags: 0xc000,
                };
                len += 1;
            }
        }
        let tick = self.tick.wrapping_add(1);
        let ok = len == 0 || sink.frame(&points[..len], tick);
        self.observe_submission(&points[..len], &[None; MAX_CONTACTS], self.last_us, tick, ok);
        self.active = false;
        self.slots = [None; MAX_CONTACTS];
        self.tick = 0;
        // Destruction is also the cleanup guarantee after a failed native cancel.
        self.close(sink);
        ok
    }
    pub fn apply(&mut self, frame: &Frame, sink: &mut impl Sink) -> bool {
        if frame.phase == Phase::Cancel {
            return if frame.device != self.device {
                true
            } else {
                self.cancel(sink)
            };
        }
        if !self.valid(frame) {
            self.cancel(sink);
            return false;
        }
        if frame.phase == Phase::Begin {
            if !sink.create(frame.surface) {
                self.close(sink);
                return false;
            }
            // A normal End retains the native handle for inertia. Keep its
            // clock monotonic across gestures instead of restarting dwTime at 1.
            let gap = if self.device == frame.device {
                (frame.time_us.saturating_sub(self.last_us) / 1_000) as u32
            } else {
                1
            };
            self.begin_tick = if self.tick == 0 {
                1
            } else {
                self.tick.wrapping_add(gap.max(1)).max(1)
            };
            self.device = frame.device;
            self.surface = frame.surface;
            self.first_us = frame.time_us;
            self.last_us = frame.time_us;
            self.active = true;
            self.frames = 0;
        }
        let tick = self
            .begin_tick
            .wrapping_add(((frame.time_us - self.first_us) / 1_000) as u32)
            .max(1);
        let old = self.slots;
        let mut next = old;
        let mut departing = [false; MAX_CONTACTS];
        // Validate first; a bad change must never leave a half-applied frame.
        for change in frame.changes() {
            if let Some(slot) = old.iter().position(|c| c.is_some_and(|c| c.id == change.contact.id)) {
                if change.phase.ends() {
                    departing[slot] = true;
                    next[slot] = None;
                } else {
                    next[slot] = Some(change.contact);
                }
            }
        }
        if frame.phase == Phase::End {
            departing = old.map(|c| c.is_some());
            next = [None; MAX_CONTACTS];
        }
        // A hardware report may replace all five IDs. Release the old set before
        // reusing slots; each native call remains at most five pointers. Both
        // submissions use the same source tick (no invented movement interval).
        let additions = frame.changes().iter().filter(|c| c.phase == Phase::Begin).count();
        let split = old.iter().flatten().count() + additions > MAX_CONTACTS;
        if split && !self.submit(sink, &old, &next, &departing, frame.time_us, tick) {
            self.cancel(sink);
            return false;
        }
        for change in frame.changes().iter().filter(|c| c.phase == Phase::Begin) {
            let slot = next
                .iter()
                .enumerate()
                .position(|(slot, c)| c.is_none() && (split || old[slot].is_none()))
                .expect("validated contact limit");
            next[slot] = Some(change.contact);
        }
        let submitted = if split {
            self.submit(
                sink,
                &[None; MAX_CONTACTS],
                &next,
                &[false; MAX_CONTACTS],
                frame.time_us,
                tick,
            )
        } else {
            self.submit(sink, &old, &next, &departing, frame.time_us, tick)
        };
        if !submitted {
            self.cancel(sink);
            return false;
        }
        self.slots = next;
        self.last_us = frame.time_us;
        self.tick = tick;
        self.frames += 1;
        if frame.phase == Phase::End {
            self.active = false;
            // Keep the handle alive for Windows' raw-gesture inertia. Creating the
            // next raw device or a lifecycle cancellation retires it independently.
        }
        true
    }
    fn submit(
        &mut self,
        sink: &mut impl Sink,
        old: &[Option<Contact>; MAX_CONTACTS],
        next: &[Option<Contact>; MAX_CONTACTS],
        departing: &[bool; MAX_CONTACTS],
        time_us: u64,
        tick: u32,
    ) -> bool {
        let mut points = [Pointer::default(); MAX_CONTACTS];
        let mut len = 0;
        for slot in 0..MAX_CONTACTS {
            if departing[slot]
                && let Some(c) = old[slot]
            {
                points[len] = Pointer {
                    id: slot as u32 + 1,
                    x: c.x,
                    y: c.y,
                    flags: 0x4000,
                };
                len += 1;
            }
            if let Some(c) = next[slot] {
                // When a departed slot is reused the release must precede its Begin.
                // The non-split path allocates free native IDs below instead.
                points[len] = Pointer {
                    id: slot as u32 + 1,
                    x: c.x,
                    y: c.y,
                    flags: 0x4006,
                };
                len += 1;
            }
        }
        let ok = len == 0 || sink.frame(&points[..len], tick);
        self.observe_submission(&points[..len], next, time_us, tick, ok);
        ok
    }
    fn observe_submission(
        &mut self,
        points: &[Pointer],
        slots: &[Option<Contact>; MAX_CONTACTS],
        time_us: u64,
        tick: u32,
        success: bool,
    ) {
        let mut ids = [0; MAX_CONTACTS];
        let mut flags = [0; MAX_CONTACTS];
        for ((id, flags), point) in ids.iter_mut().zip(&mut flags).zip(points) {
            *id = point.id;
            *flags = point.flags;
        }
        self.submitted.observe(&ids, &flags, success);
        crate::native_lifecycle::emit_contacts(crate::contact_trace::Observation {
            stage: 1,
            device: self.device,
            time_us,
            tick,
            count: slots.iter().flatten().count() as u8,
            points: points.len() as u8,
            ids,
            flags,
            slots: slots.map(|c| c.map_or(0, |c| c.id)),
            active_mask: self.submitted.mask,
            known: self.submitted.known,
            success,
        });
    }
    fn close(&mut self, sink: &mut impl Sink) {
        sink.close();
        self.submitted = crate::contact_trace::Submitted::default();
        crate::native_lifecycle::emit_contacts(crate::contact_trace::Observation {
            stage: 2,
            device: self.device,
            time_us: self.last_us,
            known: true,
            success: true,
            ..Default::default()
        });
    }
    fn valid(&self, frame: &Frame) -> bool {
        if frame.device == 0 || !frame.surface.valid() || usize::from(frame.len) > MAX_CHANGES {
            return false;
        }
        if frame.phase == Phase::Begin {
            if self.active || frame.len == 0 || usize::from(frame.len) > MAX_CONTACTS {
                return false;
            }
        } else if !self.active
            || frame.device != self.device
            || frame.surface != self.surface
            || frame.time_us < self.last_us
            || frame.time_us - self.last_us > 5_000_000
        {
            return false;
        }
        let mut count = self.slots.iter().flatten().count() as isize;
        for (i, change) in frame.changes().iter().enumerate() {
            if !change.contact.valid(frame.surface)
                || frame.changes()[..i].iter().any(|c| c.contact.id == change.contact.id)
            {
                return false;
            }
            let exists = self.slots.iter().flatten().any(|c| c.id == change.contact.id);
            match change.phase {
                Phase::Begin if !exists && frame.phase != Phase::End => count += 1,
                Phase::Move if exists && frame.phase != Phase::Begin => {}
                Phase::End | Phase::Cancel if exists && frame.phase != Phase::Begin => count -= 1,
                _ => return false,
            }
        }
        (0..=MAX_CONTACTS as isize).contains(&count)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::contacts::Sample;
    #[derive(Default)]
    struct Fake {
        opened: Vec<Surface>,
        frames: Vec<(Vec<Pointer>, u32)>,
        closed: usize,
        fail: bool,
    }
    impl Sink for Fake {
        fn create(&mut self, s: Surface) -> bool {
            self.opened.push(s);
            !self.fail
        }
        fn frame(&mut self, points: &[Pointer], tick: u32) -> bool {
            self.frames.push((points.to_vec(), tick));
            !self.fail
        }
        fn close(&mut self) {
            self.closed += 1;
        }
    }
    fn sample(t: u64, ids: &[u32]) -> Sample {
        let mut s = Sample {
            device: 1,
            surface: Surface {
                width: 15_600,
                height: 9_600,
            },
            time_us: t,
            len: ids.len() as u8,
            ..Default::default()
        };
        for (p, &id) in s.contacts.iter_mut().zip(ids) {
            *p = Contact {
                id,
                x: id as i32 * 100,
                y: 4000,
            };
        }
        s
    }
    #[test]
    fn consecutive_gestures_keep_native_clock_and_source_gap() {
        let mut receiver = Receiver::default();
        let mut sink = Fake::default();
        let a = sample(1_000_000, &[1, 2, 3]);
        let end = sample(1_008_000, &[]);
        assert!(receiver.apply(&Frame::between(None, &a), &mut sink));
        assert!(receiver.apply(&Frame::between(Some(&a), &end), &mut sink));
        let b = sample(1_100_000, &[4, 5, 6]);
        assert!(receiver.apply(&Frame::between(None, &b), &mut sink));
        assert_eq!(sink.frames.iter().map(|f| f.1).collect::<Vec<_>>(), [1, 9, 101]);
    }
    #[test]
    fn physical_geometry_source_clock_and_stable_native_ids() {
        let mut receiver = Receiver::default();
        let mut sink = Fake::default();
        let a = sample(1_000_000, &[11, 12, 13]);
        let mut b = sample(1_008_000, &[13, 11]);
        b.contacts[0].x += 17;
        assert!(receiver.apply(&Frame::between(None, &a), &mut sink));
        assert!(receiver.apply(&Frame::between(Some(&a), &b), &mut sink));
        assert_eq!(sink.opened, vec![a.surface]);
        assert_eq!(sink.frames[0].1, 1);
        assert_eq!(sink.frames[1].1, 9);
        assert_eq!(sink.frames[1].0.iter().find(|p| p.id == 3).unwrap().x, 1317);
        assert_eq!(sink.frames[1].0.iter().find(|p| p.id == 2).unwrap().flags, 0x4000);
    }
    #[test]
    fn five_finger_replacement_releases_before_slot_reuse() {
        let mut receiver = Receiver::default();
        let mut sink = Fake::default();
        let a = sample(0, &[1, 2, 3, 4, 5]);
        let b = sample(8000, &[6, 7, 8, 9, 10]);
        assert!(receiver.apply(&Frame::between(None, &a), &mut sink));
        assert!(receiver.apply(&Frame::between(Some(&a), &b), &mut sink));
        assert_eq!(sink.frames.len(), 3);
        assert!(sink.frames[1].0.iter().all(|c| c.flags == 0x4000));
        assert!(sink.frames[2].0.iter().all(|c| c.flags == 0x4006));
        assert!(sink.frames.iter().all(|f| f.0.len() == 5));
        assert_eq!(sink.frames[1].1, sink.frames[2].1);
    }
    #[test]
    fn partial_replacement_does_not_duplicate_native_id_in_one_call() {
        let mut receiver = Receiver::default();
        let mut sink = Fake::default();
        let a = sample(0, &[1, 2, 3]);
        let b = sample(8000, &[2, 3, 4]);
        assert!(receiver.apply(&Frame::between(None, &a), &mut sink));
        assert!(receiver.apply(&Frame::between(Some(&a), &b), &mut sink));
        let p = &sink.frames[1].0;
        assert_eq!(p.len(), 4);
        for (i, c) in p.iter().enumerate() {
            assert!(!p[..i].iter().any(|old| old.id == c.id));
        }
    }
    #[test]
    fn focus_disconnect_secure_desktop_and_shutdown_use_same_release() {
        for _boundary in ["focus leave", "disconnect", "secure desktop", "shutdown"] {
            let mut receiver = Receiver::default();
            let mut sink = Fake::default();
            assert!(receiver.apply(&Frame::between(None, &sample(0, &[1, 2, 3])), &mut sink));
            assert!(receiver.cancel(&mut sink));
            assert!(!receiver.active());
            assert_eq!(sink.closed, 1);
            assert!(sink.frames.last().unwrap().0.iter().all(|p| p.flags == 0xc000));
            let n = sink.frames.len();
            receiver.cancel(&mut sink);
            assert_eq!(sink.frames.len(), n);
        }
    }
    #[test]
    fn native_failure_closes_device_even_when_cancellation_fails() {
        let mut receiver = Receiver::default();
        let mut sink = Fake::default();
        let a = sample(0, &[1, 2, 3]);
        assert!(receiver.apply(&Frame::between(None, &a), &mut sink));
        sink.fail = true;
        assert!(!receiver.apply(&Frame::between(Some(&a), &sample(8000, &[1, 2, 3])), &mut sink));
        assert!(!receiver.active());
        assert_eq!(sink.closed, 1);
    }
    #[test]
    fn malformed_duplicate_backward_time_and_sleep_jump_cancel_whole_gesture() {
        for case in 0..4 {
            let mut receiver = Receiver::default();
            let mut sink = Fake::default();
            let a = sample(10_000, &[1, 2, 3]);
            assert!(receiver.apply(&Frame::between(None, &a), &mut sink));
            let mut f = Frame::between(Some(&a), &sample(18_000, &[1, 2, 3]));
            match case {
                0 => f.len = 255,
                1 => f.changes[1] = f.changes[0],
                2 => f.time_us = 0,
                _ => f.time_us = 6_000_000,
            }
            assert!(!receiver.apply(&f, &mut sink));
            assert!(!receiver.active());
            assert_eq!(sink.closed, 1);
        }
    }
    #[test]
    fn normal_end_is_release_without_cancel_or_scroll_inertia_actions() {
        let mut receiver = Receiver::default();
        let mut sink = Fake::default();
        let a = sample(0, &[1, 2]);
        receiver.apply(&Frame::between(None, &a), &mut sink);
        assert!(receiver.apply(&Frame::between(Some(&a), &sample(8000, &[])), &mut sink));
        assert!(!receiver.active());
        assert_eq!(sink.closed, 0);
        assert!(sink.frames.last().unwrap().0.iter().all(|p| p.flags == 0x4000));
        receiver.cancel(&mut sink);
        assert_eq!(sink.closed, 1, "lifecycle also closes idle device/inertia");
    }
}
