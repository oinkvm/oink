//! Fixed numeric modifier observations; no ordinary key, native tag or identity.
use crate::native_lifecycle::Data;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Stage {
    Enter = 1,
    Exit = 2,
    Deferred = 3,
    Drained = 4,
    QueueCleared = 5,
    Installed = 6,
    Removed = 7,
    Desktop = 8,
    Session = 9,
    Owner = 10,
    Duration = 11,
    Reconcile = 12,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Outcome {
    Unfinished = 0,
    Own = 1,
    ExternalLocal = 2,
    Reentrant = 3,
    LockGuard = 4,
    Unmapped = 5,
    Swallowed = 6,
    Passed = 7,
    StateUnavailable = 8,
    DisabledGuard = 9,
}
/// HID E0..E7 bit order. Only the eight unambiguous modifier VKs qualify;
/// neither ordinary keys nor arbitrary native VK/scan codes reach the queue.
pub(crate) fn modifier(vk: u32) -> u8 {
    match vk {
        0xa2 => 1,
        0xa0 => 2,
        0xa4 => 4,
        0x5b => 8,
        0xa3 => 16,
        0xa1 => 32,
        0xa5 => 64,
        0x5c => 128,
        _ => 0,
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counters {
    pub hook_generation: u64,
    pub entered: u64,
    pub exited: u64,
}
impl Counters {
    pub fn data(self, stage: Stage, serial: u64, flags: u32, held: Option<u8>) -> Data {
        Data {
            reason: stage as u32,
            flags,
            extra: self.hook_generation,
            reentries: serial,
            marker: Some(self.exited),
            steps: held.map(u32::from),
            ..Default::default()
        }
    }
    pub fn enter(&mut self) -> u64 {
        self.entered = self.entered.wrapping_add(1);
        self.entered
    }
    pub fn exit(&mut self) {
        self.exited = self.exited.wrapping_add(1);
    }
    pub fn install(&mut self) {
        self.hook_generation = self.hook_generation.wrapping_add(1);
    }
    pub fn reset_recorder(&mut self) {
        self.entered = 0;
        self.exited = 0;
    }
}
/// bit0..7 modifier; bit8 down; bit9 system message; bit10 translation succeeded;
/// bit11..12 origin (0 physical, 1 external, 2 own, 3 marker); bit16..23 outcome.
pub(crate) fn flags(modifier: u8, down: bool, system: bool, mapped: bool, origin: u8, outcome: Outcome) -> u32 {
    u32::from(modifier)
        | (u32::from(down) << 8)
        | (u32::from(system) << 9)
        | (u32::from(mapped) << 10)
        | (u32::from(origin & 3) << 11)
        | ((outcome as u32) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_and_unknown_keys_never_qualify() {
        assert_eq!((0..=0xffff).filter(|vk| modifier(*vk) != 0).count(), 8);
        assert_eq!(modifier(0x5b), 8);
        assert_eq!((0..256).fold(0, |mask, vk| mask | modifier(vk)), 255);
    }
    #[test]
    fn nested_callbacks_keep_their_serial_and_unknown_state() {
        let mut counts = Counters::default();
        counts.install();
        let outer = counts.enter();
        let inner = counts.enter();
        let f = flags(8, false, true, true, 0, Outcome::Reentrant);
        counts.exit();
        let nested = counts.data(Stage::Exit, inner, f, None);
        counts.exit();
        let parent = counts.data(Stage::Exit, outer, f, Some(8));
        assert_eq!((nested.reentries, parent.reentries), (2, 1));
        assert_eq!((nested.marker, parent.marker), (Some(1), Some(2)));
        assert_eq!((nested.steps, parent.steps), (None, Some(8)));
        assert_eq!(parent.extra, 1);
        assert_eq!(parent.flags & 255, 8);
        assert_eq!(parent.flags >> 16, Outcome::Reentrant as u32);
    }
    #[test]
    fn recorder_reset_keeps_hook_generation_and_does_not_change_input() {
        let mut counts = Counters::default();
        counts.install();
        counts.enter();
        counts.exit();
        counts.reset_recorder();
        assert_eq!((counts.hook_generation, counts.entered, counts.exited), (1, 0, 0));
        counts.install();
        let d = counts.data(Stage::Installed, 0, 0, Some(0));
        assert_eq!(d.extra, 2);
        assert_eq!(d.steps, Some(0));
        assert_eq!(d.point, None);
        assert_eq!(d.chain_result, None);
    }
}

/// Only first physical DOWNs passed to the local system are eligible. Async
/// state cannot describe a swallowed remote key or an identity touched by an
/// independent synthetic source. Eight fixed bits; no per-event allocation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PassedModifiers(u8);
impl PassedModifiers {
    pub fn first_down(&mut self, key: crate::KeyCode, passed_physical: bool) {
        if key.is_modifier() {
            let bit = 1 << (key.usage() - 0xe0);
            self.0 = (self.0 & !bit) | if passed_physical { bit } else { 0 };
        }
    }
    pub fn invalidate(&mut self, mask: u8) {
        self.0 &= !mask;
    }
    pub fn candidates(self, requested: u8, locally_passed: u8) -> u8 {
        self.0 & requested & locally_passed
    }
    pub fn released(self, requested: u8, locally_passed: u8, system_down: Option<u8>) -> u8 {
        system_down.map_or(0, |down| self.candidates(requested, locally_passed) & !down)
    }
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;
    use crate::KeyCode;
    #[test]
    fn async_zero_never_releases_swallowed_unknown_or_synthetic_holds() {
        let mut held = PassedModifiers::default();
        held.first_down(KeyCode::LEFT_GUI, true);
        held.first_down(KeyCode::RIGHT_GUI, false);
        assert_eq!(held.released(255, 255, Some(0)), 8);
        assert_eq!(held.released(255, 255, None), 0);
        assert_eq!(held.released(255, 0, Some(0)), 0);
        assert_eq!(held.released(255, 255, Some(8)), 0);
        held.invalidate(8);
        assert_eq!(held.released(255, 255, Some(0)), 0);
        held.first_down(KeyCode::LEFT_GUI, true);
        assert_eq!(held.released(8, 8, Some(0)), 8);
        // A new swallowed press cannot inherit eligibility from an old press.
        held.first_down(KeyCode::LEFT_GUI, false);
        assert_eq!(held.released(255, 255, Some(0)), 0);
    }
}
