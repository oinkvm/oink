//! Pure single-exit claim rules. The configuration owner persists each proposed
//! record together with its revision and high-water mark before publishing it.
//! Cached records establish ordering only; current authenticated presence is a
//! separate input to media authorization.
use oink_foundation::DeviceId;
use oink_protocol::v1::{
    RawValue,
    dto::{AudioExitClaim, ControlFailure, ED25519, ExitClaimStamp},
    schema::{WireDto, WireValue},
};
use std::collections::{BTreeMap, BTreeSet};

/// All currently supported identities use Ed25519, so public-key byte order is
/// exactly their wire (algorithm, public-key) order. Unknown algorithms cannot
/// enter this executable projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub counter: u64,
    pub owner: DeviceId,
}
impl Stamp {
    pub fn from_wire(stamp: &ExitClaimStamp) -> Result<Self, ControlFailure> {
        let invalid = |_| ControlFailure::InvalidValue;
        let owner = stamp.owner().map_err(invalid)?;
        owner.require_supported().map_err(|_| ControlFailure::Unsupported)?;
        Ok(Self {
            counter: stamp.counter().map_err(invalid)?,
            owner: DeviceId(
                owner
                    .public_key()
                    .map_err(invalid)?
                    .0
                    .try_into()
                    .map_err(|_| ControlFailure::InvalidValue)?,
            ),
        })
    }
    pub fn to_wire(self) -> ExitClaimStamp {
        ExitClaimStamp::from_raw(BTreeMap::from([
            (1, RawValue::Uint(self.counter)),
            (
                2,
                RawValue::Map(BTreeMap::from([
                    (1, RawValue::Text(ED25519.into())),
                    (2, RawValue::Bytes(self.owner.0.to_vec())),
                ])),
            ),
        ]))
        .expect("validated nonzero claim stamp")
    }
}

/// A None claim is an authenticated baseline, unlike an absent owner record.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub owner: DeviceId,
    pub revision: u64,
    pub claim: Option<AudioExitClaim>,
}
impl Record {
    pub fn stamp(&self) -> Option<Stamp> {
        self.claim
            .as_ref()
            .map(|c| Stamp::from_wire(&c.stamp().expect("validated claim")).expect("validated identity"))
    }
    pub fn claimed(&self) -> bool {
        self.claim.as_ref().is_some_and(|c| c.claimed() == Ok(true))
    }
    fn validate(&self) -> Result<(), ControlFailure> {
        if self.revision == 0 {
            return Err(ControlFailure::InvalidValue);
        }
        if let Some(claim) = &self.claim {
            let stamp = Stamp::from_wire(&claim.stamp().map_err(|_| ControlFailure::InvalidValue)?)?;
            if stamp.owner != self.owner {
                return Err(ControlFailure::NotMember);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    #[default]
    Unknown,
    None {
        barrier: Option<Stamp>,
    },
    Exit {
        stamp: Stamp,
        available: bool,
    },
}
impl Resolution {
    pub fn stamp(self) -> Option<Stamp> {
        match self {
            Self::Exit { stamp, .. } => Some(stamp),
            _ => None,
        }
    }
    pub fn owner(self) -> Option<DeviceId> {
        self.stamp().map(|stamp| stamp.owner)
    }
    pub fn available(self) -> bool {
        matches!(self, Self::Exit { available: true, .. })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Claims {
    records: BTreeMap<DeviceId, Record>,
    seen_counter: u64,
    watermark: Option<AudioExitClaim>,
}

/// A relayed ordering witness can stop an obsolete route, but cannot supply an
/// owner's authenticated record or authorize playback. False wins at one stamp.
pub fn claim_order(claim: &AudioExitClaim) -> Result<(Stamp, bool), ControlFailure> {
    Ok((
        Stamp::from_wire(&claim.stamp().map_err(|_| ControlFailure::InvalidValue)?)?,
        !claim.claimed().map_err(|_| ControlFailure::InvalidValue)?,
    ))
}
impl Claims {
    /// Restore durable authenticated baselines, including removed members and
    /// false withdrawals. Callers retain the wire trees in their owner store.
    pub fn restore(records: impl IntoIterator<Item = Record>, seen_counter: u64) -> Result<Self, ControlFailure> {
        let mut claims = Self {
            records: BTreeMap::new(),
            seen_counter,
            watermark: None,
        };
        for record in records {
            record.validate()?;
            if claims.records.contains_key(&record.owner) {
                return Err(ControlFailure::StateConflict);
            }
            claims.seen_counter = claims.seen_counter.max(record.stamp().map_or(0, |s| s.counter));
            if let Some(claim) = &record.claim {
                claims.observe_watermark(claim.clone())?;
            }
            claims.records.insert(record.owner, record);
        }
        Ok(claims)
    }
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }
    pub fn seen_counter(&self) -> u64 {
        self.seen_counter
    }
    pub fn watermark(&self) -> Option<&AudioExitClaim> {
        self.watermark.as_ref()
    }
    pub fn observe_watermark(&mut self, claim: AudioExitClaim) -> Result<bool, ControlFailure> {
        let order = claim_order(&claim)?;
        if self
            .watermark
            .as_ref()
            .map(claim_order)
            .transpose()?
            .is_some_and(|old| old >= order)
        {
            return Ok(false);
        }
        self.seen_counter = self.seen_counter.max(order.0.counter);
        self.watermark = Some(claim);
        Ok(true)
    }
    pub fn record(&self, owner: DeviceId) -> Option<&Record> {
        self.records.get(&owner)
    }
    fn maximum(&self) -> Option<&Record> {
        self.records
            .values()
            .filter(|r| r.claim.is_some())
            .max_by_key(|r| r.stamp())
    }
    pub fn resolve(&self, members: &BTreeSet<DeviceId>, available: &BTreeSet<DeviceId>) -> Resolution {
        if members.iter().any(|m| !self.records.contains_key(m)) {
            return Resolution::Unknown;
        }
        let maximum = self.maximum();
        if self
            .watermark
            .as_ref()
            .map(claim_order)
            .transpose()
            .expect("validated watermark")
            > maximum
                .and_then(|r| r.claim.as_ref())
                .map(claim_order)
                .transpose()
                .expect("validated claim")
        {
            return Resolution::Unknown;
        }
        match maximum {
            Some(record) if record.claimed() => Resolution::Exit {
                stamp: record.stamp().expect("claim exists"),
                available: members.contains(&record.owner) && available.contains(&record.owner),
            },
            record => Resolution::None {
                barrier: record.and_then(Record::stamp),
            },
        }
    }
    /// Apply to a candidate copy, commit that copy durably, then publish. An
    /// authenticated endpoint may update only its own record, never a relay's.
    pub fn observe(&mut self, authenticated: DeviceId, record: Record) -> Result<bool, ControlFailure> {
        record.validate()?;
        if authenticated != record.owner {
            return Err(ControlFailure::NotMember);
        }
        if let Some(previous) = self.records.get(&record.owner) {
            if record.revision < previous.revision {
                return Ok(false);
            }
            if record.revision == previous.revision {
                return if previous == &record {
                    Ok(false)
                } else {
                    Err(ControlFailure::StateConflict)
                };
            }
            if record.stamp() < previous.stamp()
                || (record.stamp() == previous.stamp() && !previous.claimed() && record.claimed())
            {
                return Err(ControlFailure::StateConflict);
            }
            // Same-stamp edits may change only the claimed bit. Unknown retained
            // fields cannot be rewritten under the guise of an automatic yield.
            if record.stamp() == previous.stamp() {
                let mut expected = previous.claim.clone();
                if let Some(claim) = &mut expected {
                    claim
                        .edit(|raw| {
                            raw.insert(2, RawValue::Bool(record.claimed()));
                        })
                        .map_err(|_| ControlFailure::InvalidValue)?;
                }
                if expected != record.claim {
                    return Err(ControlFailure::StateConflict);
                }
            }
        }
        self.seen_counter = self.seen_counter.max(record.stamp().map_or(0, |s| s.counter));
        if let Some(claim) = &record.claim {
            self.observe_watermark(claim.clone())?;
        }
        self.records.insert(record.owner, record);
        Ok(true)
    }
    /// An explicit owner command produces a new stamp. Clear is legal only at
    /// the current winning owner; automatic yield uses a different operation.
    pub fn command(
        &self,
        owner: DeviceId,
        expected_revision: u64,
        claimed: bool,
        members: &BTreeSet<DeviceId>,
    ) -> Result<Option<Record>, ControlFailure> {
        if !members.contains(&owner) {
            return Err(ControlFailure::NotMember);
        }
        let previous = self.records.get(&owner).ok_or(ControlFailure::Busy)?;
        if previous.revision != expected_revision {
            return Err(ControlFailure::StaleRevision);
        }
        if members.iter().any(|member| !self.records.contains_key(member)) {
            return Err(ControlFailure::Busy);
        }
        let winner = match self.resolve(members, &BTreeSet::new()) {
            // Complete owner baselines plus a stronger ordering witness permit
            // an explicit takeover above that witness, never guessed playback.
            Resolution::Unknown if claimed => None,
            Resolution::Unknown => return Err(ControlFailure::Busy),
            Resolution::Exit { stamp, .. } => Some(stamp.owner),
            Resolution::None { .. } => None,
        };
        if claimed && winner == Some(owner) {
            return Ok(None);
        }
        if !claimed && winner != Some(owner) {
            return Err(ControlFailure::NotExit);
        }
        let stamp = Stamp {
            counter: self.seen_counter.checked_add(1).ok_or(ControlFailure::StateConflict)?,
            owner,
        };
        let claim = AudioExitClaim::from_raw(BTreeMap::from([
            (1, stamp.to_wire().write()),
            (2, RawValue::Bool(claimed)),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)?;
        Ok(Some(Record {
            owner,
            revision: previous.revision.checked_add(1).ok_or(ControlFailure::StateConflict)?,
            claim: Some(claim),
        }))
    }
    /// Retire media and held volume presses before committing this proposal.
    /// Keeping the stamp prevents the loser from overtaking the winner again.
    pub fn yield_to_winner(&self, owner: DeviceId) -> Result<Option<Record>, ControlFailure> {
        let Some(previous) = self.records.get(&owner) else {
            return Ok(None);
        };
        if !previous.claimed()
            || self
                .watermark
                .as_ref()
                .map(claim_order)
                .transpose()?
                .map(|order| order.0)
                <= previous.stamp()
        {
            return Ok(None);
        }
        let mut next = previous.clone();
        next.revision = next.revision.checked_add(1).ok_or(ControlFailure::StateConflict)?;
        next.claim
            .as_mut()
            .expect("claimed record")
            .edit(|raw| {
                raw.insert(2, RawValue::Bool(false));
            })
            .map_err(|_| ControlFailure::InvalidValue)?;
        Ok(Some(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }
    fn record(owner: u8, revision: u64, counter: u64, claimed: bool) -> Record {
        Record {
            owner: id(owner),
            revision,
            claim: (counter != 0).then(|| {
                AudioExitClaim::from_raw(BTreeMap::from([
                    (
                        1,
                        Stamp {
                            counter,
                            owner: id(owner),
                        }
                        .to_wire()
                        .write(),
                    ),
                    (2, claimed.into()),
                ]))
                .unwrap()
            }),
        }
    }
    fn members() -> BTreeSet<DeviceId> {
        BTreeSet::from([id(1), id(2)])
    }
    #[test]
    fn ordering_witness_stops_stale_playback_but_only_explicit_selection_can_replace_it() {
        for claimed in [false, true] {
            let historical = record(9, 4, 7, claimed).claim.unwrap();
            let mut fresh = Claims::restore([record(1, 2, 1, true), record(2, 1, 0, false)], 1).unwrap();
            assert!(fresh.observe_watermark(historical.clone()).unwrap());
            assert_eq!(fresh.resolve(&members(), &members()), Resolution::Unknown);
            assert!(
                fresh.record(id(9)).is_none(),
                "a witness cannot impersonate the removed owner"
            );
            let yielded = fresh.yield_to_winner(id(1)).unwrap().unwrap();
            fresh.observe(id(1), yielded).unwrap();
            let mut restored = Claims::restore(fresh.records().cloned(), fresh.seen_counter()).unwrap();
            restored.observe_watermark(fresh.watermark().unwrap().clone()).unwrap();
            assert_eq!(restored.resolve(&members(), &members()), Resolution::Unknown);
            let selection = restored.command(id(1), 3, true, &members()).unwrap().unwrap();
            assert_eq!(selection.stamp().unwrap().counter, 8);
            restored.observe(id(1), selection).unwrap();
            assert_eq!(restored.resolve(&members(), &members()).owner(), Some(id(1)));
            assert!(!restored.observe_watermark(historical).unwrap());
        }
    }
    #[test]
    fn equal_counter_owner_order_and_same_stamp_withdrawal_are_ordering_evidence() {
        let mut fresh = Claims::restore([record(1, 2, 1, true), record(2, 1, 0, false)], 1).unwrap();
        fresh.observe_watermark(record(9, 2, 1, true).claim.unwrap()).unwrap();
        assert_eq!(fresh.resolve(&members(), &members()), Resolution::Unknown);
        assert_eq!(
            fresh
                .command(id(1), 2, true, &members())
                .unwrap()
                .unwrap()
                .stamp()
                .unwrap()
                .counter,
            2
        );
        let mut claims = Claims::restore([record(1, 2, 1, true), record(2, 1, 0, false)], 1).unwrap();
        claims.observe_watermark(record(1, 3, 1, false).claim.unwrap()).unwrap();
        assert_eq!(claims.resolve(&members(), &members()), Resolution::Unknown);
        claims.observe(id(1), record(1, 3, 1, false)).unwrap();
        assert!(matches!(
            claims.resolve(&members(), &members()),
            Resolution::None { .. }
        ));
    }
    #[test]
    fn concurrent_claims_converge_in_either_arrival_order_and_yield_does_not_overtake() {
        for reverse in [false, true] {
            let mut claims = Claims::default();
            let mut records = vec![record(1, 2, 4, true), record(2, 2, 4, true)];
            if reverse {
                records.reverse();
            }
            for record in records {
                claims.observe(record.owner, record).unwrap();
            }
            let winner = Resolution::Exit {
                stamp: Stamp {
                    counter: 4,
                    owner: id(2),
                },
                available: true,
            };
            assert_eq!(claims.resolve(&members(), &members()), winner);
            let yielded = claims.yield_to_winner(id(1)).unwrap().unwrap();
            assert_eq!(yielded.stamp().unwrap().counter, 4);
            assert_eq!(yielded.revision, 3);
            assert!(!yielded.claimed());
            claims.observe(id(1), yielded).unwrap();
            assert_eq!(claims.resolve(&members(), &members()), winner);
        }
    }
    #[test]
    fn withdrawal_wins_over_older_true_and_no_stale_true_can_resurrect() {
        let mut claims = Claims::restore([record(1, 2, 1, true), record(2, 2, 2, true)], 0).unwrap();
        let clear = claims.command(id(2), 2, false, &members()).unwrap().unwrap();
        assert_eq!(clear.stamp().unwrap().counter, 3);
        claims.observe(id(2), clear.clone()).unwrap();
        assert!(matches!(
            claims.resolve(&members(), &members()),
            Resolution::None {
                barrier: Some(Stamp { counter: 3, .. })
            }
        ));
        assert!(!claims.observe(id(2), record(2, 2, 2, true)).unwrap());
        assert_eq!(
            claims.observe(id(2), record(2, 4, 3, true)),
            Err(ControlFailure::StateConflict)
        );
        assert_eq!(
            claims.command(id(1), 2, false, &members()),
            Err(ControlFailure::NotExit)
        );
        let restored = Claims::restore(claims.records().cloned(), claims.seen_counter()).unwrap();
        assert_eq!(
            restored.resolve(&members(), &members()),
            claims.resolve(&members(), &members())
        );
        assert_eq!(
            restored
                .command(id(1), 2, true, &members())
                .unwrap()
                .unwrap()
                .stamp()
                .unwrap()
                .counter,
            4
        );
    }
    #[test]
    fn incomplete_baseline_offline_and_removed_winner_never_select_runner_up() {
        let mut claims = Claims::restore([record(1, 2, 1, true)], 7).unwrap();
        assert_eq!(claims.resolve(&members(), &members()), Resolution::Unknown);
        assert_eq!(claims.command(id(1), 2, true, &members()), Err(ControlFailure::Busy));
        claims.observe(id(2), record(2, 2, 8, true)).unwrap();
        let first_only = BTreeSet::from([id(1)]);
        let unavailable = Resolution::Exit {
            stamp: Stamp {
                counter: 8,
                owner: id(2),
            },
            available: false,
        };
        assert_eq!(claims.resolve(&members(), &first_only), unavailable);
        assert_eq!(claims.resolve(&first_only, &first_only), unavailable);
        let takeover = claims.command(id(1), 2, true, &first_only).unwrap().unwrap();
        assert_eq!(takeover.stamp().unwrap().counter, 9);
    }
    #[test]
    fn no_claim_baseline_is_not_missing_and_commands_obey_cas_and_overflow() {
        let claims = Claims::restore([record(1, 1, 0, false), record(2, 1, 0, false)], 0).unwrap();
        assert_eq!(
            claims.resolve(&members(), &members()),
            Resolution::None { barrier: None }
        );
        assert_eq!(
            claims.command(id(1), 2, true, &members()),
            Err(ControlFailure::StaleRevision)
        );
        assert_eq!(
            claims
                .command(id(1), 1, true, &members())
                .unwrap()
                .unwrap()
                .stamp()
                .unwrap()
                .counter,
            1
        );
        let full = Claims::restore(claims.records().cloned(), u64::MAX).unwrap();
        assert_eq!(
            full.command(id(1), 1, true, &members()),
            Err(ControlFailure::StateConflict)
        );
    }
    #[test]
    fn authenticated_owner_and_revision_content_conflicts_are_rejected() {
        let mut claims = Claims::restore([record(1, 2, 4, true)], 4).unwrap();
        assert_eq!(
            claims.observe(id(2), record(1, 3, 5, true)),
            Err(ControlFailure::NotMember)
        );
        assert_eq!(
            claims.observe(id(1), record(1, 2, 4, false)),
            Err(ControlFailure::StateConflict)
        );
        assert_eq!(
            claims.observe(id(1), record(1, 3, 3, false)),
            Err(ControlFailure::StateConflict)
        );
        assert!(claims.observe(id(1), record(1, 3, 4, false)).unwrap());
        assert_eq!(
            claims.observe(id(1), record(1, 4, 0, false)),
            Err(ControlFailure::StateConflict)
        );
    }
}
