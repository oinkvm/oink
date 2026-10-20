//! An admission gate, not an idle snapshot. Every business owner must register and
//! enter this gate for the entire lifetime of its activity (including deferred IO).
use crate::Error;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    Drag,
    Promise,
    FileProvider,
    ClipboardBulk,
    RemoteInput,
    HeldInput,
    RouterTransition,
    Audio,
    Microphone,
    Configuration,
    Locked,
    Suspended,
    SessionChanged,
    OwnerMissing,
    Consent,
    IntegrationUnavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Owner(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionId(pub [u8; 16]);
#[derive(Debug)]
struct State {
    activities: BTreeMap<Blocker, usize>,
    owners: BTreeSet<Owner>,
    acks: BTreeSet<Owner>,
    transaction: Option<TransactionId>,
    generation: u64,
    expires: Option<Instant>,
    shutdown: bool,
}
#[derive(Clone, Debug)]
pub struct Barrier(Arc<Mutex<State>>);
#[derive(Debug)]
pub struct Activity {
    barrier: Barrier,
    reason: Blocker,
}
#[derive(Debug)]
pub struct Quiesced {
    tx: TransactionId,
    generation: u64,
}
impl Barrier {
    pub fn new(_now: Instant) -> Self {
        Self(Arc::new(Mutex::new(State {
            activities: BTreeMap::new(),
            owners: BTreeSet::new(),
            acks: BTreeSet::new(),
            transaction: None,
            generation: 0,
            expires: None,
            shutdown: false,
        })))
    }
    pub fn register(&self, owner: Owner, now: Instant) {
        let mut s = self.0.lock().unwrap();
        invalidate(&mut s, now);
        s.owners.insert(owner);
    }
    pub fn enter(&self, reason: Blocker, now: Instant) -> Result<Activity, Error> {
        let mut s = self.0.lock().unwrap();
        expire(&mut s, now);
        if s.transaction.is_some() || s.shutdown {
            return Err(Error::Busy);
        }
        *s.activities.entry(reason).or_default() += 1;
        Ok(Activity {
            barrier: self.clone(),
            reason,
        })
    }
    /// Called for session topology changes and owner loss.
    /// Invalidates all previously issued tokens, reopening admission unless shutdown began.
    pub fn invalidate(&self, now: Instant) {
        invalidate(&mut self.0.lock().unwrap(), now);
    }
    pub fn begin(&self, tx: TransactionId, now: Instant) -> Result<(), Vec<Blocker>> {
        let mut s = self.0.lock().unwrap();
        expire(&mut s, now);
        if s.shutdown || s.transaction.is_some_and(|active| active != tx) {
            return Err(vec![Blocker::OwnerMissing]);
        }
        if s.transaction == Some(tx) {
            return Ok(());
        }
        // Manual intent closes admission despite current activity. Owners ACK
        // only after bounded interruption/release; activity counts never veto.
        s.transaction = Some(tx);
        s.acks.clear();
        s.expires = Some(now + Duration::from_secs(30));
        Ok(())
    }
    /// Owners ACK only after session teardown, input release and restoration flush.
    /// The Windows service supplies authenticated owners, never an arbitrary UI id.
    pub fn acknowledge(&self, tx: TransactionId, owner: Owner, now: Instant) -> Result<(), Error> {
        let mut s = self.0.lock().unwrap();
        expire(&mut s, now);
        if s.transaction != Some(tx) || !s.owners.contains(&owner) || s.shutdown {
            return Err(Error::Busy);
        }
        s.acks.insert(owner);
        Ok(())
    }
    pub fn quiesced(&self, tx: TransactionId, now: Instant) -> Result<Quiesced, Vec<Blocker>> {
        let mut s = self.0.lock().unwrap();
        expire(&mut s, now);
        if s.transaction != Some(tx) || s.acks != s.owners || s.shutdown {
            return Err(vec![Blocker::OwnerMissing]);
        }
        Ok(Quiesced {
            tx,
            generation: s.generation,
        })
    }
    pub fn shutdown(&self, token: Quiesced, now: Instant) -> Result<(), Error> {
        let mut s = self.0.lock().unwrap();
        expire(&mut s, now);
        if s.transaction != Some(token.tx) || s.generation != token.generation || s.acks != s.owners || s.shutdown {
            return Err(Error::Busy);
        }
        s.shutdown = true;
        Ok(())
    }
    pub fn abort(&self, tx: TransactionId, now: Instant) -> Result<(), Error> {
        let mut s = self.0.lock().unwrap();
        if s.transaction != Some(tx) || s.shutdown {
            return Err(Error::Transaction);
        }
        invalidate(&mut s, now);
        Ok(())
    }
}
fn invalidate(s: &mut State, _now: Instant) {
    s.generation = s.generation.wrapping_add(1);
    if !s.shutdown {
        s.transaction = None;
        s.acks.clear();
        s.expires = None;
    }
}
fn expire(s: &mut State, now: Instant) {
    if !s.shutdown && s.expires.is_some_and(|t| now >= t) {
        invalidate(s, now);
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        let mut s = self.barrier.0.lock().unwrap();
        if let Some(count) = s.activities.get_mut(&self.reason) {
            *count -= 1;
            if *count == 0 {
                s.activities.remove(&self.reason);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const TX: TransactionId = TransactionId([1; 16]);
    #[test]
    fn explicit_install_closes_admission_despite_all_activity_owners() {
        let now = Instant::now();
        for reason in [
            Blocker::Drag,
            Blocker::Promise,
            Blocker::Audio,
            Blocker::Microphone,
            Blocker::RemoteInput,
            Blocker::HeldInput,
            Blocker::FileProvider,
            Blocker::ClipboardBulk,
            Blocker::Locked,
            Blocker::Configuration,
            Blocker::Suspended,
        ] {
            let b = Barrier::new(now);
            b.register(Owner(1), now);
            let active = b.enter(reason, now).unwrap();
            b.begin(TX, now).unwrap();
            assert!(b.enter(reason, now).is_err());
            assert!(b.quiesced(TX, now).is_err());
            b.acknowledge(TX, Owner(1), now).unwrap();
            assert!(b.quiesced(TX, now).is_ok());
            drop(active);
        }
    }
    #[test]
    fn admission_and_prepare_are_atomic() {
        let now = Instant::now();
        let b = Barrier::new(now);
        b.register(Owner(1), now);
        b.begin(TX, now).unwrap();
        assert!(b.enter(Blocker::Drag, now).is_err());
        assert!(b.quiesced(TX, now).is_err());
        b.acknowledge(TX, Owner(1), now).unwrap();
        let token = b.quiesced(TX, now).unwrap();
        b.shutdown(token, now).unwrap();
        assert!(b.enter(Blocker::Audio, now).is_err());
    }
    #[test]
    fn lease_session_change_and_cancel_invalidate_tokens() {
        for elapsed in [0, 31] {
            let now = Instant::now();
            let b = Barrier::new(now);
            b.begin(TX, now).unwrap();
            let token = b.quiesced(TX, now).unwrap();
            if elapsed == 0 {
                b.invalidate(now);
            }
            assert!(b.shutdown(token, now + Duration::from_secs(elapsed)).is_err());
            assert!(b.enter(Blocker::Drag, now + Duration::from_secs(elapsed)).is_ok());
        }
        let now = Instant::now();
        let b = Barrier::new(now);
        b.begin(TX, now).unwrap();
        b.abort(TX, now).unwrap();
        assert!(b.enter(Blocker::Audio, now).is_ok());
    }
    #[test]
    fn explicit_install_has_no_idle_timer_but_requires_every_owner_fence() {
        let now = Instant::now();
        let b = Barrier::new(now);
        b.register(Owner(1), now);
        b.register(Owner(2), now);
        b.begin(TX, now).unwrap();
        b.acknowledge(TX, Owner(1), now).unwrap();
        assert!(b.quiesced(TX, now).is_err());
        b.acknowledge(TX, Owner(2), now).unwrap();
        assert!(b.quiesced(TX, now).is_ok());
    }
}
