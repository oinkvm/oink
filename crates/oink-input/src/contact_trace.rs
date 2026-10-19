//! Contact identities and native slot state only; never coordinates. Producers
//! copy fixed-size values, and the ordinary recorder formats the generic log.
use oink_foundation::contacts::{MAX_CONTACTS, Sample};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// 0=captured sample, 1=native submission, 2=native device closed.
    pub stage: u8,
    pub device: u32,
    /// Original source clock, shared with the contact frame on I0.
    pub time_us: u64,
    pub tick: u32,
    pub count: u8,
    pub points: u8,
    /// Capture: source IDs in report order. Injection: native pointer IDs in call order.
    pub ids: [u32; MAX_CONTACTS],
    pub flags: [u32; MAX_CONTACTS],
    /// Source IDs indexed by native slot minus one after this submission.
    /// At capture this is the hardware report order, not a Windows slot assignment.
    pub slots: [u32; MAX_CONTACTS],
    /// Shadow of successfully submitted INCONTACT pointers, not an OS recognizer query.
    pub active_mask: u8,
    /// A failed native call makes the shadow uncertain until device destruction.
    pub known: bool,
    pub success: bool,
}
impl Observation {
    pub fn capture(sample: &Sample) -> Self {
        let valid = sample.valid();
        let mut ids = [0; MAX_CONTACTS];
        if valid {
            for (id, contact) in ids.iter_mut().zip(sample.points()) {
                *id = contact.id;
            }
        }
        Self {
            device: sample.device,
            time_us: sample.time_us,
            count: sample.len,
            points: sample.len,
            ids,
            slots: ids,
            success: valid,
            ..Default::default()
        }
    }
}

#[cfg(any(windows, all(test, debug_assertions)))]
#[derive(Debug)]
pub(crate) struct Submitted {
    pub mask: u8,
    pub known: bool,
}
#[cfg(any(windows, all(test, debug_assertions)))]
impl Default for Submitted {
    fn default() -> Self {
        Self { mask: 0, known: true }
    }
}
#[cfg(any(windows, all(test, debug_assertions)))]
impl Submitted {
    pub fn observe(&mut self, ids: &[u32; MAX_CONTACTS], flags: &[u32; MAX_CONTACTS], success: bool) {
        if !success {
            self.known = false;
            return;
        }
        for (&id, &flags) in ids.iter().zip(flags) {
            if (1..=MAX_CONTACTS as u32).contains(&id) {
                let bit = 1 << (id - 1);
                if flags & 4 != 0 {
                    self.mask |= bit;
                } else {
                    self.mask &= !bit;
                }
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn submission_shadow_tracks_release_reuse_failure_and_device_destruction() {
        let mut state = Submitted::default();
        state.observe(&[1, 3, 0, 0, 0], &[0x4006, 0x4006, 0, 0, 0], true);
        assert_eq!(state.mask, 0b00101);
        state.observe(&[1, 3, 4, 0, 0], &[0x4000, 0x4006, 0x4006, 0, 0], true);
        assert_eq!(state.mask, 0b01100);
        state.observe(&[3, 4, 0, 0, 0], &[0xc000, 0xc000, 0, 0, 0], false);
        assert_eq!(state.mask, 0b01100);
        assert!(!state.known);
        state.observe(&[3, 4, 0, 0, 0], &[0xc000, 0xc000, 0, 0, 0], true);
        assert_eq!(state.mask, 0);
        assert!(
            !state.known,
            "success cannot prove a previously rejected partial update was atomic"
        );
        state = Submitted::default();
        assert!(state.known && state.mask == 0);
    }
}
