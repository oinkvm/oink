//! Bounded owner/requester bookkeeping shared by option, pause and volume edits.
use crate::api::DeviceKey;
use oink_protocol::v1::dto::ControlFailure;
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

pub const DEADLINE: Duration = Duration::from_secs(10);
pub const MAX_FRAME: usize = 4096;
const MAX_OUTSTANDING: usize = 32;
const PER_REQUESTER: usize = 4;
const TERMINAL_RESULTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Requester {
    pub device: DeviceKey,
    pub nonce: [u8; 16],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestKey {
    pub requester: Requester,
    pub id: [u8; 16],
}
#[derive(Clone, Debug)]
struct Pending {
    body: Vec<u8>,
    deadline: Instant,
}
#[derive(Clone, Debug)]
struct Terminal<T> {
    key: RequestKey,
    body: Vec<u8>,
    result: T,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Admission<T> {
    New,
    Pending,
    Replay(T),
}
#[derive(Debug)]
pub struct Transactions<T> {
    pending: BTreeMap<RequestKey, Pending>,
    terminal: VecDeque<Terminal<T>>,
}
impl<T: Clone> Default for Transactions<T> {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
            terminal: VecDeque::new(),
        }
    }
}
impl<T: Clone> Transactions<T> {
    pub fn admit(&mut self, key: RequestKey, body: &[u8], now: Instant) -> Result<Admission<T>, ControlFailure> {
        if body.len() > MAX_FRAME {
            return Err(ControlFailure::InvalidValue);
        }
        if let Some(found) = self.terminal.iter().find(|entry| entry.key == key) {
            return if found.body == body {
                Ok(Admission::Replay(found.result.clone()))
            } else {
                Err(ControlFailure::StateConflict)
            };
        }
        if let Some(found) = self.pending.get(&key) {
            return if found.body == body {
                Ok(Admission::Pending)
            } else {
                Err(ControlFailure::StateConflict)
            };
        }
        if self.pending.len() >= MAX_OUTSTANDING
            || self.pending.keys().filter(|k| k.requester == key.requester).count() >= PER_REQUESTER
        {
            return Err(ControlFailure::Busy);
        }
        self.pending.insert(
            key,
            Pending {
                body: body.to_owned(),
                deadline: now + DEADLINE,
            },
        );
        Ok(Admission::New)
    }
    pub fn complete(&mut self, key: RequestKey, result: T) -> bool {
        let Some(pending) = self.pending.remove(&key) else {
            return false;
        };
        if self.terminal.len() == TERMINAL_RESULTS {
            self.terminal.pop_front();
        }
        self.terminal.push_back(Terminal {
            key,
            body: pending.body,
            result,
        });
        true
    }
    /// Expiration cannot prove failure: the owner may already have committed.
    pub fn expired(&self, now: Instant) -> Vec<RequestKey> {
        self.pending
            .iter()
            .filter_map(|(key, p)| (p.deadline <= now).then_some(*key))
            .collect()
    }
    pub fn outstanding_from(&self, device: DeviceKey) -> Vec<RequestKey> {
        self.pending
            .keys()
            .filter(|key| key.requester.device == device)
            .copied()
            .collect()
    }
    pub fn retain_requesters(&mut self, mut admitted: impl FnMut(Requester) -> bool) {
        self.pending.retain(|key, _| admitted(key.requester));
        self.terminal.retain(|entry| admitted(entry.key.requester));
    }
    pub fn retire_requester(&mut self, requester: Requester) {
        self.pending.retain(|key, _| key.requester != requester);
        self.terminal.retain(|entry| entry.key.requester != requester);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(peer: u8, id: u8) -> RequestKey {
        RequestKey {
            requester: Requester {
                device: oink_foundation::DeviceId([peer; 32]).into(),
                nonce: [peer; 16],
            },
            id: [id; 16],
        }
    }
    #[test]
    fn replay_and_conflicts_are_scoped_to_authenticated_boot() {
        let mut cache = Transactions::default();
        let now = Instant::now();
        assert_eq!(cache.admit(key(1, 1), b"a", now), Ok(Admission::New));
        assert_eq!(cache.admit(key(1, 1), b"a", now), Ok(Admission::Pending));
        assert_eq!(cache.admit(key(1, 1), b"b", now), Err(ControlFailure::StateConflict));
        cache.complete(key(1, 1), 42);
        assert_eq!(cache.admit(key(1, 1), b"a", now), Ok(Admission::Replay(42)));
        assert_eq!(cache.admit(key(1, 1), b"b", now), Err(ControlFailure::StateConflict));
        let mut reboot = key(1, 1);
        reboot.requester.nonce = [9; 16];
        assert_eq!(cache.admit(reboot, b"b", now), Ok(Admission::New));
    }
    #[test]
    fn quotas_and_timeouts_include_every_pending_operation() {
        let mut cache = Transactions::<u32>::default();
        let now = Instant::now();
        for peer in 0..8 {
            for id in 0..4 {
                assert_eq!(cache.admit(key(peer, id), b"a", now), Ok(Admission::New));
            }
        }
        assert_eq!(cache.admit(key(0, 5), b"a", now), Err(ControlFailure::Busy));
        assert_eq!(cache.admit(key(9, 0), b"a", now), Err(ControlFailure::Busy));
        assert_eq!(cache.expired(now + DEADLINE).len(), 32);
        assert!(cache.expired(now + DEADLINE - Duration::from_nanos(1)).is_empty());
        cache.complete(key(0, 0), 1);
        assert_eq!(cache.admit(key(9, 0), b"a", now), Ok(Admission::New));
    }
    #[test]
    fn bounded_terminal_eviction_does_not_evict_pending() {
        let mut cache = Transactions::default();
        let now = Instant::now();
        cache.admit(key(0, 0), b"pending", now).unwrap();
        for n in 0..257 {
            let mut request = key(1, 0);
            request.id[..2].copy_from_slice(&(n as u16).to_le_bytes());
            cache.admit(request, b"done", now).unwrap();
            cache.complete(request, n);
        }
        assert_eq!(cache.terminal.len(), 256);
        assert_eq!(cache.admit(key(0, 0), b"pending", now), Ok(Admission::Pending));
    }
}
