//! Privacy-safe MT metadata. No positions, velocities, device addresses or serials.
//! Raw size/density bits are uncalibrated sensor values, not physical pressure.
#[derive(Clone, Copy, Debug, Default)]
pub struct Contact {
    /// Tracking path index at ABI offset 16 (also the release-wire source ID).
    pub id: i32,
    /// Raw finger identity at ABI offset 24; distinct from the tracking path ID.
    pub finger_identity: i32,
    pub state: i32,
    pub frame: i32,
    pub size_bits: u32,
    pub density_bits: u32,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Observation {
    /// 0=enumeration, 1=sample. Device labels are process/subscription scoped.
    pub kind: u8,
    pub device: u32,
    pub callback_device: u32,
    pub devices: u16,
    pub built_in: u16,
    pub frame_count: u64,
    pub source_us: u64,
    pub received_us: u64,
    pub source_gap_us: i64,
    pub received_gap_us: u64,
    pub source_gap_max_us: u64,
    pub received_gap_max_us: u64,
    pub source_backwards: u32,
    pub samples: u64,
    pub suppressed: u64,
    pub raw_count: u8,
    pub accepted: u8,
    pub valid: bool,
    pub contacts: [Contact; 16],
}

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
#[derive(Default)]
pub(crate) struct Tracker {
    previous: Option<Observation>,
    window_us: u64,
    emitted: u8,
    suppressed: u64,
    samples: u64,
    source_gap_max_us: u64,
    received_gap_max_us: u64,
    source_backwards: u32,
}
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
impl Tracker {
    /// At most 16 observations/second, including topology changes; at least a
    /// periodic observation during steady input. Gap fields still track *every*
    /// callback, and suppressed counts disclose the bounded sampling loss.
    pub fn observe(&mut self, mut o: Observation) -> Option<Observation> {
        self.samples += 1;
        let changed = self.previous.is_none_or(|old| {
            old.raw_count != o.raw_count
                || old.accepted != o.accepted
                || old.valid != o.valid
                || old.callback_device != o.callback_device
                || old
                    .contacts
                    .iter()
                    .zip(o.contacts)
                    .any(|(a, b)| (a.id, a.state, a.finger_identity) != (b.id, b.state, b.finger_identity))
        });
        if let Some(old) = self.previous {
            o.source_gap_us =
                (i128::from(o.source_us) - i128::from(old.source_us)).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            o.received_gap_us = o.received_us.saturating_sub(old.received_us);
        }
        self.source_gap_max_us = self.source_gap_max_us.max(o.source_gap_us.max(0) as u64);
        self.received_gap_max_us = self.received_gap_max_us.max(o.received_gap_us);
        self.source_backwards += u32::from(o.source_gap_us < 0);
        self.previous = Some(o);
        let periodic = o.received_us.saturating_sub(self.window_us) >= 1_000_000;
        if periodic {
            self.window_us = o.received_us;
            self.emitted = 0;
        }
        if (changed || periodic) && self.emitted < 16 {
            self.emitted += 1;
            o.source_gap_max_us = std::mem::take(&mut self.source_gap_max_us);
            o.received_gap_max_us = std::mem::take(&mut self.received_gap_max_us);
            o.source_backwards = std::mem::take(&mut self.source_backwards);
            o.samples = self.samples;
            o.suppressed = std::mem::take(&mut self.suppressed);
            Some(o)
        } else {
            self.suppressed += 1;
            None
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_gap_or_clock_reset_between_sampled_rows_is_not_hidden() {
        let mut tracker = Tracker::default();
        let o = Observation {
            received_us: 2_000_000,
            source_us: 900_000,
            ..Default::default()
        };
        assert!(tracker.observe(o).is_some());
        assert!(
            tracker
                .observe(Observation {
                    received_us: 2_200_000,
                    source_us: 920_000,
                    ..o
                })
                .is_none()
        );
        assert!(
            tracker
                .observe(Observation {
                    received_us: 2_208_000,
                    source_us: 800_000,
                    ..o
                })
                .is_none()
        );
        let last = tracker
            .observe(Observation {
                received_us: 3_000_000,
                source_us: 808_000,
                ..o
            })
            .unwrap();
        assert_eq!(last.source_gap_us, 8000);
        assert_eq!(last.source_gap_max_us, 20000);
        assert_eq!(last.received_gap_max_us, 792000);
        assert_eq!(last.source_backwards, 1);
        assert_eq!(last.samples, 4);
    }
    #[test]
    fn steady_ghost_and_fast_state_changes_are_bounded_but_gaps_remain_per_sample() {
        let mut tracker = Tracker::default();
        let mut emitted = Vec::new();
        for i in 0..250 {
            let mut o = Observation {
                kind: 1,
                received_us: 10_000_000 + i * 8000,
                source_us: i * 8000,
                raw_count: 1,
                accepted: 1,
                ..Default::default()
            };
            o.contacts[0] = Contact {
                id: 7,
                state: if i < 100 { (i % 2 + 3) as i32 } else { 4 },
                finger_identity: 9,
                size_bits: 0.5f32.to_bits(),
                density_bits: 0.7f32.to_bits(),
                ..Default::default()
            };
            if let Some(o) = tracker.observe(o) {
                emitted.push(o);
            }
        }
        assert_eq!(emitted.len(), 17);
        assert_eq!(emitted.last().unwrap().source_gap_us, 8000);
        assert_eq!(emitted.last().unwrap().received_gap_us, 8000);
        assert_eq!(emitted.last().unwrap().contacts[0].id, 7);
        assert!(emitted.last().unwrap().suppressed > 0);
        assert_eq!(emitted.last().unwrap().samples, 126);
    }
}
