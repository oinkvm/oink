//! Additional application delivery has its own press identity and held ledger.
//! No snapshot can manufacture a DOWN; retirement never touches baseline refs.
use std::collections::HashMap;

use oink_foundation::{input::KeyCode, session::Boot};
use serde::{Deserialize, Serialize};

use super::Route;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRoute {
    pub epoch: u64,
    pub pointer: Boot,
    pub revision: u64,
    pub destination: Boot,
    pub press_id: u64,
}
impl KeyRoute {
    pub fn matches(&self, route: &Route) -> bool {
        self.press_id != 0
            && self.epoch == route.claim.epoch
            && self.pointer == route.claim.pointer
            && self.revision == route.revision
            && self.destination == route.destination
    }
}

#[derive(Debug)]
pub struct HeldSnapshot {
    pub source: Boot,
    pub route: KeyRoute,
    pub hops: u8,
    pub keys: Vec<(KeyCode, u64)>,
}

#[derive(Debug, Default)]
pub struct Outgoing {
    sequence: u64,
    held: HashMap<(Boot, KeyCode), KeyRoute>,
    sync: HashMap<Boot, (KeyRoute, u8)>,
}
impl Outgoing {
    /// Called only for a newly delivered baseline DOWN on P. Repeated DOWN and
    /// held-state repair cannot start another application press.
    pub fn press(&mut self, source: Boot, code: KeyCode, route: &Route) -> Option<KeyRoute> {
        if self.held.contains_key(&(source, code)) {
            return None;
        }
        self.sequence = self.sequence.checked_add(1)?;
        let token = KeyRoute {
            epoch: route.claim.epoch,
            pointer: route.claim.pointer,
            revision: route.revision,
            destination: route.destination,
            press_id: self.sequence,
        };
        self.held.insert((source, code), token.clone());
        Some(token)
    }
    pub fn track_sync(&mut self, source: Boot, token: &KeyRoute, hops: u8) {
        self.sync.insert(source, (token.clone(), hops));
    }
    /// Keep empty snapshots until route retirement: a lost UP and its first
    /// repair must both heal without synthesizing any new DOWN.
    pub fn snapshots(&self) -> Vec<HeldSnapshot> {
        self.sync
            .iter()
            .map(|(source, (route, hops))| {
                let mut keys: Vec<_> = self
                    .held
                    .iter()
                    .filter(|((boot, _), _)| boot == source)
                    .map(|((_, code), token)| (*code, token.press_id))
                    .collect();
                keys.sort_by_key(|(code, _)| code.0);
                HeldSnapshot {
                    source: *source,
                    route: route.clone(),
                    hops: *hops,
                    keys,
                }
            })
            .collect()
    }
    pub fn release(&mut self, source: Boot, code: KeyCode) -> Option<KeyRoute> {
        self.held.remove(&(source, code))
    }
    pub fn repeat(&self, source: Boot, code: KeyCode) -> Option<KeyRoute> {
        self.held.get(&(source, code)).cloned()
    }
    pub fn retire(&mut self, route: &Route) {
        self.held.retain(|_, token| !token.matches(route));
        self.sync.retain(|_, (token, _)| !token.matches(route));
    }
    pub fn retire_source(&mut self, source: Boot) {
        self.held.retain(|(boot, _), _| *boot != source);
        self.sync.remove(&source);
    }
}

#[derive(Debug)]
struct Held {
    token: KeyRoute,
    mapped: KeyCode,
    repeat: u32,
}

#[derive(Debug, Default)]
pub struct Incoming {
    held: HashMap<(Boot, KeyCode), Held>,
    // Every P assigns increasing IDs across all its outgoing keys. Retain the
    // watermark after UP so a duplicated DOWN cannot resurrect a released key.
    seen: HashMap<(Boot, Boot), u64>,
}
impl Incoming {
    /// The caller authenticates P, validates the accepted snapshot/Boot/scope,
    /// then increments its shared native refcount for every returned mapped key.
    /// Baseline and application refs are separate even for the same source/key.
    pub fn press(&mut self, source: Boot, physical: KeyCode, mapped: KeyCode, token: KeyRoute) -> Option<KeyCode> {
        if token.press_id == 0
            || self.held.contains_key(&(source, physical))
            || self
                .seen
                .get(&(token.pointer, source))
                .is_some_and(|id| *id >= token.press_id)
        {
            return None;
        }
        self.seen.insert((token.pointer, source), token.press_id);
        self.held.insert(
            (source, physical),
            Held {
                token,
                mapped,
                repeat: 0,
            },
        );
        Some(mapped)
    }
    pub fn release(&mut self, source: Boot, physical: KeyCode, token: &KeyRoute) -> Option<KeyCode> {
        if self
            .held
            .get(&(source, physical))
            .is_none_or(|held| held.token != *token)
        {
            return None;
        }
        self.held.remove(&(source, physical)).map(|held| held.mapped)
    }
    pub fn repeat(&mut self, source: Boot, physical: KeyCode, token: &KeyRoute, sequence: u32) -> Option<KeyCode> {
        let held = self.held.get_mut(&(source, physical))?;
        if held.token != *token || sequence <= held.repeat {
            return None;
        }
        held.repeat = sequence;
        Some(held.mapped)
    }
    pub fn sync(&mut self, source: Boot, route: &KeyRoute, keys: &[(KeyCode, u64)]) -> Vec<KeyCode> {
        // Snapshot watermark cannot release a press which began after sampling.
        let mut releases = Vec::new();
        self.held.retain(|(boot, code), held| {
            let token = &held.token;
            let release = *boot == source
                && token.pointer == route.pointer
                && token.epoch == route.epoch
                && token.revision == route.revision
                && token.destination == route.destination
                && token.press_id <= route.press_id
                && !keys.contains(&(*code, token.press_id));
            if release {
                releases.push(held.mapped);
            }
            !release
        });
        self.seen
            .entry((route.pointer, source))
            .and_modify(|id| *id = (*id).max(route.press_id))
            .or_insert(route.press_id);
        releases
    }
    /// Returns one release per reference, including multiple refs to one key.
    pub fn retire(&mut self, route: &Route) -> Vec<KeyCode> {
        self.release_where(|_, held| held.token.matches(route))
    }
    pub fn retire_source(&mut self, source: Boot) -> Vec<KeyCode> {
        self.release_where(|boot, _| boot == source)
    }
    pub fn forget_boot(&mut self, boot: Boot) -> Vec<KeyCode> {
        self.seen
            .retain(|(pointer, source), _| *pointer != boot && *source != boot);
        self.release_where(|source, held| source == boot || held.token.pointer == boot)
    }
    pub fn clear(&mut self) -> Vec<KeyCode> {
        self.release_where(|_, _| true)
    }
    fn release_where(&mut self, predicate: impl Fn(Boot, &Held) -> bool) -> Vec<KeyCode> {
        let mut releases = Vec::new();
        self.held.retain(|(boot, _), held| {
            if predicate(*boot, held) {
                releases.push(held.mapped);
                false
            } else {
                true
            }
        });
        releases.sort_by_key(|code| code.0);
        releases
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{application_focus::Claim, control::FocusScope};
    use oink_foundation::DeviceId;

    fn boot(n: u8) -> Boot {
        Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        }
    }
    fn route(revision: u64) -> Route {
        Route {
            claim: Claim {
                epoch: 7,
                pointer: boot(1),
                scope: FocusScope { members: vec![] },
            },
            revision,
            destination: boot(2),
        }
    }

    #[test]
    fn retirement_drops_tails_and_stale_up_cannot_release_a_later_press() {
        let mut outgoing = Outgoing::default();
        let mut incoming = Incoming::default();
        let code = KeyCode::LEFT_SHIFT;
        let old_route = route(1);
        let old = outgoing.press(boot(3), code, &old_route).unwrap();
        assert_eq!(incoming.press(boot(3), code, code, old.clone()), Some(code));
        assert!(outgoing.press(boot(3), code, &old_route).is_none());
        outgoing.retire(&old_route);
        assert_eq!(incoming.retire(&old_route), vec![code]);
        assert!(outgoing.repeat(boot(3), code).is_none());
        assert!(outgoing.release(boot(3), code).is_none());
        let new = outgoing.press(boot(3), code, &route(2)).unwrap();
        assert_ne!(new.press_id, old.press_id);
        assert_eq!(incoming.press(boot(3), code, code, new.clone()), Some(code));
        assert_eq!(incoming.release(boot(3), code, &old), None);
        assert_eq!(incoming.release(boot(3), code, &new), Some(code));
        assert_eq!(incoming.press(boot(3), code, code, new), None);
    }

    #[test]
    fn repair_is_release_only_and_old_snapshot_cannot_release_new_press() {
        let mut out = Outgoing::default();
        let mut incoming = Incoming::default();
        let code = KeyCode::LEFT_SHIFT;
        let first = out.press(boot(3), code, &route(1)).unwrap();
        out.track_sync(boot(3), &first, 2);
        incoming.press(boot(3), code, code, first.clone());
        out.release(boot(3), code);
        let empty = out.snapshots().pop().unwrap();
        assert!(empty.keys.is_empty());
        assert_eq!(incoming.sync(boot(3), &empty.route, &empty.keys), vec![code]);
        assert!(incoming.press(boot(3), code, code, first).is_none());
        let second = out.press(boot(3), code, &route(1)).unwrap();
        incoming.press(boot(3), code, code, second.clone());
        assert!(incoming.sync(boot(3), &empty.route, &[]).is_empty());
        assert_eq!(incoming.release(boot(3), code, &second), Some(code));
        assert!(incoming.sync(boot(3), &second, &[(code, second.press_id)]).is_empty());
        assert!(incoming.clear().is_empty(), "snapshot never creates a press");
    }

    #[test]
    fn caps_identity_and_other_source_refs_retire_independently() {
        let mut outgoing = Outgoing::default();
        let mut incoming = Incoming::default();
        let shift = KeyCode::LEFT_SHIFT;
        for (source, physical) in [(boot(1), KeyCode::CAPS_LOCK), (boot(1), shift), (boot(3), shift)] {
            let token = outgoing.press(source, physical, &route(1)).unwrap();
            assert_eq!(incoming.press(source, physical, shift, token.clone()), Some(shift));
            assert_eq!(incoming.repeat(source, physical, &token, 1), Some(shift));
            assert_eq!(incoming.repeat(source, physical, &token, 1), None);
        }
        assert_eq!(incoming.retire_source(boot(1)), vec![shift, shift]);
        assert_eq!(incoming.clear(), vec![shift]);
        assert!(incoming.clear().is_empty());
    }
}
