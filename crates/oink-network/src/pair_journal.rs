//! Durable pairing decisions, retained proofs and local outcome delivery. A
//! committed projection is replayed before discovery; its decision never rolls back.
use crate::{group_merge, group_state::State, roster::Anchor, wire_identity as wire};
use oink_foundation::{DeviceId, session::PairId};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
pub const FILE: &str = "pairing-recovery.cbor";
const MAX_FILE: usize = 64 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pairing journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("pairing journal validation failed: {0}")]
    Wire(#[from] v1::Error),
    #[error("pairing journal conflict")]
    Conflict,
    #[error("pairing decision is unavailable")]
    Missing,
    #[error("pairing participant is not authorized")]
    Unauthorized,
}
type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Source,
    Target,
    Observer,
}
impl Role {
    fn number(self) -> u8 {
        match self {
            Self::Source => 0,
            Self::Target => 1,
            Self::Observer => 2,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Pending,
    Committed(dto::MergeTransferBundle),
    Aborted(dto::PairProblem),
    Unconfirmed,
}
#[derive(Clone, Debug)]
pub struct Permit {
    pub pair: PairId,
    pub intent: [u8; 32],
    pub plan: [u8; 32],
    pub decision: u64,
}
#[derive(Clone, Debug)]
pub struct Outcome {
    pub id: u64,
    pub body: RawMap,
}
#[derive(Clone, Debug)]
pub struct Recovery {
    pub pair: PairId,
    pub source: DeviceId,
    pub target: DeviceId,
    pub plan: dto::MergePlan,
    pub decision: u64,
    pub outcome: Decision,
    pub receipt: bool,
    pub commit_sent: bool,
}
#[derive(Clone, Debug)]
pub struct Journal {
    path: PathBuf,
    local: DeviceId,
    raw: RawMap,
}
// Schema 1 root: 1 schema,2 local identity,3 transactions,4 endpoint permits,
// 5 next event id,6 acknowledged event id,7 outcome outbox,8 source sequences,
// 9 unapplied projection. Unknown executable root/row fields fail closed.
// Transaction:1 pair,2 source,3 target,4 plan,5 decision,6 status
// (0 prepared,1 committed,2 aborted,3 unconfirmed,4 historical proof),7 role
// (0 source,1 target,2 observer),8 bundle?,9 receipt,10 archive?,11 event_id?,
// 12 abort problem?,13 commit sent. Proofs and archives are retained wire trees.
impl Journal {
    pub fn load(path: &Path, local: DeviceId) -> Result<Self> {
        let raw = match std::fs::File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes)?;
                v1::cbor::decode_map(&bytes, MAX_FILE)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => [
                (1, 1u8.write()),
                (2, wire::device(local).write()),
                (3, RawValue::Array(Vec::new())),
                (4, RawValue::Array(Vec::new())),
                (5, 1u64.write()),
                (6, 0u64.write()),
                (7, RawValue::Array(Vec::new())),
                (8, RawValue::Array(Vec::new())),
            ]
            .into(),
            Err(e) => return Err(e.into()),
        };
        validate(&raw, local)?;
        Ok(Self {
            path: path.into(),
            local,
            raw,
        })
    }
    fn edit<T>(&mut self, apply: impl FnOnce(&mut RawMap) -> Result<T>) -> Result<T> {
        let mut candidate = self.raw.clone();
        let result = apply(&mut candidate)?;
        validate(&candidate, self.local)?;
        let bytes = v1::cbor::encode_map(&candidate, MAX_FILE)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        oink_foundation::storage::atomic_write(&self.path, &bytes)?;
        self.raw = candidate;
        Ok(result)
    }
    pub fn recoveries(&self) -> Result<Vec<Recovery>> {
        let mut result = Vec::new();
        for value in array(&self.raw, 3)? {
            let row = value.as_map()?;
            if get::<u8>(row, 7)? == 2 {
                continue;
            }
            let source = wire::read_device(&get::<dto::DeviceId>(row, 2)?)?;
            let target = wire::read_device(&get::<dto::DeviceId>(row, 3)?)?;
            let pair = wire::read_pair(&get::<dto::PairId>(row, 1)?)?;
            let plan = get::<dto::MergePlan>(row, 4)?;
            let decision = get(row, 5)?;
            result.push(Recovery {
                pair,
                source,
                target,
                outcome: self.decision(self.local, pair, group_merge::digest(&plan.intent()?)?, decision)?,
                plan,
                decision,
                receipt: get(row, 9)?,
                commit_sent: get(row, 13)?,
            });
        }
        Ok(result)
    }
    pub fn aborted_event(&mut self, pair: PairId, decision: u64, event: RawMap) -> Result<u64> {
        self.edit(|raw| {
            let row = transaction(raw, pair, decision)?.ok_or(Error::Missing)?;
            if get::<u8>(row, 6)? != 2 {
                return Err(Error::Conflict);
            }
            if let Some(id) = row.get(&11) {
                return Ok(u64::read(id)?);
            }
            let id = push_event(raw, event)?;
            transaction_mut(raw, pair, decision)?
                .expect("existing")
                .insert(11, id.write());
            Ok(id)
        })
    }
    pub fn next_source_sequence(&mut self, source: Anchor) -> Result<u64> {
        let mut known = 0;
        for value in array(&self.raw, 3)? {
            let row = value.as_map()?;
            let intent = get::<dto::MergePlan>(row, 4)?.intent()?;
            if group_merge::anchor(&intent.source()?)? == source {
                known = known.max(intent.source_transition_sequence()?);
            }
        }
        self.edit(|raw| {
            let rows = array_mut(raw, 8)?;
            let reference = group_merge::group_ref(source).write();
            if let Some(value) = rows
                .iter_mut()
                .find(|r| r.as_map().ok().and_then(|r| r.get(&1)) == Some(&reference))
            {
                let row = map_mut(value)?;
                let next = get::<u64>(row, 2)?.max(known).checked_add(1).ok_or(Error::Conflict)?;
                row.insert(2, next.write());
                Ok(next)
            } else {
                let next = known.checked_add(1).ok_or(Error::Conflict)?;
                rows.push(RawValue::Map([(1, reference), (2, next.write())].into()));
                Ok(next)
            }
        })
    }
    /// The configuration owner calls this after both endpoints agree on the
    /// complete prepared intent. The smaller endpoint key is the coordinator.
    pub fn permit(&mut self, plan: &dto::MergePlan) -> Result<Permit> {
        let intent = plan.intent()?;
        let source = wire::read_device(&intent.source_issuer()?)?;
        let target = wire::read_device(&intent.target_issuer()?)?;
        if self.local != source.min(target) {
            return Err(Error::Unauthorized);
        }
        let pair = wire::read_pair(&intent.pair_id()?)?;
        let intent_digest = group_merge::digest(&intent)?;
        let plan_digest = group_merge::digest(plan)?;
        self.edit(|raw| {
            let low = wire::device(source.min(target)).write();
            let high = wire::device(source.max(target)).write();
            let rows = array_mut(raw, 4)?;
            let index = if let Some(index) = rows.iter().position(|r| {
                r.as_map()
                    .ok()
                    .is_some_and(|r| r.get(&1) == Some(&low) && r.get(&2) == Some(&high))
            }) {
                index
            } else {
                rows.push(RawValue::Map([(1, low), (2, high), (3, 0u64.write())].into()));
                rows.len() - 1
            };
            let row = map_mut(&mut rows[index])?;
            if let Some(active) = row.get(&4) {
                if *active != wire::pair(pair).write()
                    || row.get(&5) != Some(&RawValue::Bytes(intent_digest.to_vec()))
                    || row.get(&6) != Some(&RawValue::Bytes(plan_digest.to_vec()))
                {
                    return Err(Error::Conflict);
                }
                return Ok(Permit {
                    pair,
                    intent: intent_digest,
                    plan: plan_digest,
                    decision: get(row, 3)?,
                });
            }
            let decision = get::<u64>(row, 3)?.checked_add(1).ok_or(Error::Conflict)?;
            row.insert(3, decision.write());
            row.insert(4, wire::pair(pair).write());
            row.insert(5, RawValue::Bytes(intent_digest.to_vec()));
            row.insert(6, RawValue::Bytes(plan_digest.to_vec()));
            Ok(Permit {
                pair,
                intent: intent_digest,
                plan: plan_digest,
                decision,
            })
        })
    }
    pub fn matches_permit(&self, permit: &Permit) -> Result<bool> {
        Ok(array(&self.raw, 4)?.iter().any(|r| {
            r.as_map().ok().is_some_and(|r| {
                r.get(&4) == Some(&wire::pair(permit.pair).write())
                    && r.get(&5) == Some(&RawValue::Bytes(permit.intent.to_vec()))
                    && r.get(&6) == Some(&RawValue::Bytes(permit.plan.to_vec()))
                    && r.get(&3) == Some(&permit.decision.write())
            })
        }))
    }
    pub fn prepare(
        &mut self,
        plan: &dto::MergePlan,
        decision: u64,
        role: Role,
        archive: Option<&dto::GroupState>,
    ) -> Result<()> {
        let intent = plan.intent()?;
        let source = wire::read_device(&intent.source_issuer()?)?;
        let target = wire::read_device(&intent.target_issuer()?)?;
        let pair = wire::read_pair(&intent.pair_id()?)?;
        if (role == Role::Source && self.local != source)
            || (role == Role::Target && self.local != target)
            || role == Role::Observer
        {
            return Err(Error::Unauthorized);
        }
        let plan = plan.clone();
        let archive = archive.cloned();
        self.edit(|raw| {
            if let Some(row) = transaction(raw, pair, decision)? {
                return if get::<dto::MergePlan>(row, 4)? == plan && get::<u8>(row, 6)? == 0 {
                    Ok(())
                } else {
                    Err(Error::Conflict)
                };
            }
            let mut row: RawMap = [
                (1, wire::pair(pair).write()),
                (2, wire::device(source).write()),
                (3, wire::device(target).write()),
                (4, plan.write()),
                (5, decision.write()),
                (6, 0u8.write()),
                (7, role.number().write()),
                (9, false.write()),
                (13, false.write()),
            ]
            .into();
            if let Some(archive) = archive {
                row.insert(10, archive.write());
            }
            array_mut(raw, 3)?.push(RawValue::Map(row));
            Ok(())
        })
    }
    /// Persist before transmitting a source authorization/Commit. Restart then
    /// queries the target instead of assuming absence of a reply meant abort.
    pub fn commit_sent(&mut self, pair: PairId, decision: u64) -> Result<()> {
        self.edit(|raw| {
            let row = transaction_mut(raw, pair, decision)?.ok_or(Error::Missing)?;
            if get::<u8>(row, 7)? != 0 || get::<u8>(row, 6)? != 0 {
                return Err(Error::Conflict);
            }
            row.insert(13, true.write());
            Ok(())
        })
    }
    pub fn commit(&mut self, bundle: &dto::MergeTransferBundle, current: &State) -> Result<()> {
        let certificate = bundle.certificate()?;
        let intent = certificate.intent()?;
        let pair = wire::read_pair(&intent.pair_id()?)?;
        let decision = certificate.permit_decision()?;
        if let Some(row) = transaction(&self.raw, pair, decision)?
            && get::<u8>(row, 6)? == 1
        {
            return if get::<dto::MergeTransferBundle>(row, 8)? == *bundle {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        let local = self.local;
        let source = wire::read_device(&intent.source_issuer()?)?;
        let target = wire::read_device(&intent.target_issuer()?)?;
        if local == source.min(target)
            && !self.matches_permit(&Permit {
                pair,
                intent: group_merge::digest(&intent)?,
                plan: group_merge::digest(&bundle.plan()?)?,
                decision,
            })?
        {
            return Err(Error::Unauthorized);
        }
        let result = group_merge::verify_bundle(bundle, group_merge::anchor(&intent.source()?)?)?;
        if local == source {
            group_merge::adopt(bundle, current, local)?;
        } else if local != target || current.digest()? != intent.target_base_digest()?.0 {
            return Err(Error::Unauthorized);
        }
        let bundle = bundle.clone();
        let projected = result.wire().clone();
        self.edit(|raw| {
            if raw.contains_key(&9) {
                return Err(Error::Conflict);
            }
            let row = transaction_mut(raw, pair, decision)?.ok_or(Error::Missing)?;
            if get::<u8>(row, 6)? == 1 {
                return if get::<dto::MergeTransferBundle>(row, 8)? == bundle {
                    Ok(())
                } else {
                    Err(Error::Conflict)
                };
            }
            if !matches!(get::<u8>(row, 6)?, 0 | 3) || get::<dto::MergePlan>(row, 4)? != bundle.plan()? {
                return Err(Error::Conflict);
            }
            row.insert(6, 1u8.write());
            row.insert(8, bundle.write());
            raw.insert(
                9,
                RawValue::Map(
                    [
                        (1, wire::pair(pair).write()),
                        (2, decision.write()),
                        (3, projected.write()),
                    ]
                    .into(),
                ),
            );
            Ok(())
        })
    }
    pub fn last_event_id(&self) -> Result<u64> {
        Ok(get::<u64>(&self.raw, 5)?.saturating_sub(1))
    }
    pub fn pending_bundle(&self) -> Result<Option<dto::MergeTransferBundle>> {
        let Some(projection) = self.raw.get(&9) else {
            return Ok(None);
        };
        let projection = projection.as_map()?;
        let pair = wire::read_pair(&get::<dto::PairId>(projection, 1)?)?;
        let row = transaction(&self.raw, pair, get(projection, 2)?)?.ok_or(Error::Missing)?;
        Ok(Some(get(row, 8)?))
    }
    pub fn pending_projection(&self) -> Result<Option<dto::GroupState>> {
        self.raw
            .get(&9)
            .map(|v| {
                let row = v.as_map()?;
                get(row, 3)
            })
            .transpose()
    }
    /// Call only after group-state.json's private atomic replacement succeeds.
    pub fn projected(&mut self, digest: [u8; 32]) -> Result<()> {
        self.edit(|raw| {
            let Some(projection) = raw.get(&9) else { return Ok(()) };
            let state = get::<dto::GroupState>(projection.as_map()?, 3)?;
            if group_merge::digest(&state)? != digest {
                return Err(Error::Conflict);
            }
            raw.remove(&9);
            Ok(())
        })
    }
    pub fn abort(&mut self, pair: PairId, decision: u64, problem: dto::PairProblem) -> Result<()> {
        self.edit(|raw| {
            let row = transaction_mut(raw, pair, decision)?.ok_or(Error::Missing)?;
            if matches!(get::<u8>(row, 6)?, 1 | 4) {
                return Err(Error::Conflict);
            }
            if get::<u8>(row, 7)? == 0 && get::<bool>(row, 13)? {
                return Err(Error::Conflict);
            }
            row.insert(6, 2u8.write());
            row.insert(12, problem.write());
            release(raw, pair, decision)?;
            Ok(())
        })
    }
    /// Only a target-authenticated durable decision reply can resolve uncertain
    /// source state. Missing entries never resolve to Aborted.
    pub fn resolved_abort(
        &mut self,
        pair: PairId,
        decision: u64,
        target: DeviceId,
        problem: dto::PairProblem,
    ) -> Result<()> {
        self.edit(|raw| {
            let row = transaction_mut(raw, pair, decision)?.ok_or(Error::Missing)?;
            if wire::read_device(&get::<dto::DeviceId>(row, 3)?)? != target
                || get::<u8>(row, 7)? != 0
                || matches!(get::<u8>(row, 6)?, 1 | 4)
            {
                return Err(Error::Unauthorized);
            }
            row.insert(6, 2u8.write());
            row.insert(12, problem.write());
            release(raw, pair, decision)?;
            Ok(())
        })
    }
    /// Run before accepting network traffic. An uncommitted target decision is
    /// fenced; a source that may have transmitted Commit keeps its permit.
    pub fn recover(&mut self) -> Result<()> {
        self.edit(|raw| {
            let mut releases = Vec::new();
            for value in array_mut(raw, 3)? {
                let row = map_mut(value)?;
                if get::<u8>(row, 6)? != 0 {
                    continue;
                }
                if get::<u8>(row, 7)? == 0 && get::<bool>(row, 13)? {
                    row.insert(6, 3u8.write());
                } else {
                    row.insert(6, 2u8.write());
                    row.insert(
                        12,
                        crate::pairing::problem(dto::PairCode::PeerClosed, dto::PairStage::Commit).write(),
                    );
                    releases.push((wire::read_pair(&get::<dto::PairId>(row, 1)?)?, get::<u64>(row, 5)?));
                }
            }
            for (pair, decision) in releases {
                release(raw, pair, decision)?;
            }
            Ok(())
        })
    }
    pub fn decision(&self, requester: DeviceId, pair: PairId, intent: [u8; 32], decision: u64) -> Result<Decision> {
        let row = transaction(&self.raw, pair, decision)?.ok_or(Error::Missing)?;
        let source = wire::read_device(&get::<dto::DeviceId>(row, 2)?)?;
        let target = wire::read_device(&get::<dto::DeviceId>(row, 3)?)?;
        if requester != source && requester != target {
            return Err(Error::Unauthorized);
        }
        if group_merge::digest(&get::<dto::MergePlan>(row, 4)?.intent()?)? != intent {
            return Err(Error::Conflict);
        }
        match get::<u8>(row, 6)? {
            0 => Ok(Decision::Pending),
            1 | 4 => Ok(Decision::Committed(get(row, 8)?)),
            2 => Ok(Decision::Aborted(get(row, 12)?)),
            3 => Ok(Decision::Unconfirmed),
            _ => Err(Error::Conflict),
        }
    }
    /// A query can establish a new durable abort fence for a known target
    /// preparation whose permit delivery was interrupted. Absence alone is not
    /// returned as Aborted, and an unknown PairId remains unavailable.
    pub fn query(&mut self, requester: DeviceId, pair: PairId, intent: [u8; 32], decision: u64) -> Result<Decision> {
        match self.decision(requester, pair, intent, decision) {
            Err(Error::Missing) => {}
            result => return result,
        }
        let plan = array(&self.raw, 3)?
            .iter()
            .find_map(|v| {
                let row = v.as_map().ok()?;
                let plan = get::<dto::MergePlan>(row, 4).ok()?;
                (row.get(&1) == Some(&wire::pair(pair).write())
                    && get::<u8>(row, 7).ok() == Some(1)
                    && wire::read_device(&plan.intent().ok()?.source_issuer().ok()?).ok() == Some(requester)
                    && group_merge::digest(&plan.intent().ok()?).ok() == Some(intent))
                .then_some(plan)
            })
            .ok_or(Error::Missing)?;
        self.prepare(&plan, decision, Role::Target, None)?;
        self.abort(
            pair,
            decision,
            crate::pairing::problem(dto::PairCode::PeerClosed, dto::PairStage::Commit),
        )?;
        self.decision(requester, pair, intent, decision)
    }
    pub fn receipt(&mut self, pair: PairId, decision: u64, event: RawMap) -> Result<u64> {
        self.edit(|raw| {
            let row = transaction(raw, pair, decision)?.ok_or(Error::Missing)?;
            if get::<u8>(row, 6)? != 1 {
                return Err(Error::Conflict);
            }
            if let Some(id) = row.get(&11) {
                return Ok(u64::read(id)?);
            }
            let id = push_event(raw, event)?;
            let row = transaction_mut(raw, pair, decision)?.expect("existing transaction");
            row.insert(9, true.write());
            row.insert(11, id.write());
            release(raw, pair, decision)?;
            Ok(id)
        })
    }
    pub fn event(&mut self, body: RawMap) -> Result<u64> {
        self.edit(|raw| push_event(raw, body))
    }
    pub fn events(&self, after: u64) -> Result<Vec<Outcome>> {
        let floor = after.max(get::<u64>(&self.raw, 6)?);
        array(&self.raw, 7)?
            .iter()
            .filter_map(|v| {
                let row = match v.as_map() {
                    Ok(r) => r,
                    Err(e) => return Some(Err(e.into())),
                };
                let id = match get::<u64>(row, 1) {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                (id > floor).then(|| Ok(Outcome { id, body: get(row, 2)? }))
            })
            .collect()
    }
    pub fn acknowledge(&mut self, id: u64) -> Result<()> {
        self.edit(|raw| {
            if id >= get::<u64>(raw, 5)? {
                return Err(Error::Conflict);
            }
            let old = get::<u64>(raw, 6)?;
            if id > old {
                raw.insert(6, id.write());
                array_mut(raw, 7)?.retain(|v| {
                    v.as_map()
                        .ok()
                        .and_then(|r| get::<u64>(r, 1).ok())
                        .is_some_and(|n| n > id)
                });
            }
            Ok(())
        })
    }
    /// Historical adoption stores the same proof and source archive. It does not
    /// create an I/R completion event or overwrite an already-current target.
    pub fn adopt(&mut self, bundle: &dto::MergeTransferBundle, current: &State, apply: bool) -> Result<()> {
        let cert = bundle.certificate()?;
        let intent = cert.intent()?;
        let source = group_merge::anchor(&intent.source()?)?;
        let result = if apply {
            group_merge::adopt(bundle, current, self.local)?
        } else {
            group_merge::verify_bundle(bundle, source)?
        };
        let pair = wire::read_pair(&intent.pair_id()?)?;
        let decision = cert.permit_decision()?;
        let bundle = bundle.clone();
        let archive = current.wire().clone();
        self.edit(|raw| {
            if let Some(existing) = transaction(raw, pair, decision)? {
                return if get::<dto::MergeTransferBundle>(existing, 8)? == bundle {
                    Ok(())
                } else {
                    Err(Error::Conflict)
                };
            }
            if apply && raw.contains_key(&9) {
                return Err(Error::Conflict);
            }
            // A conflicting source branch is retained as evidence but must be
            // checked with history_conflict before the caller chooses to apply.
            let row: RawMap = [
                (1, wire::pair(pair).write()),
                (2, intent.source_issuer()?.write()),
                (3, intent.target_issuer()?.write()),
                (4, bundle.plan()?.write()),
                (5, decision.write()),
                (6, 4u8.write()),
                (7, 2u8.write()),
                (8, bundle.write()),
                (9, false.write()),
                (10, archive.write()),
                (13, false.write()),
            ]
            .into();
            array_mut(raw, 3)?.push(RawValue::Map(row));
            if apply {
                raw.insert(
                    9,
                    RawValue::Map(
                        [
                            (1, wire::pair(pair).write()),
                            (2, decision.write()),
                            (3, result.wire().write()),
                        ]
                        .into(),
                    ),
                );
            }
            Ok(())
        })
    }
    pub fn history_conflict(&self, bundle: &dto::MergeTransferBundle) -> Result<bool> {
        use std::collections::{BTreeMap, BTreeSet};
        type Key = ([u8; 16], [u8; 32]);
        let key = |reference: dto::GroupRef| -> Result<Key> { Ok((reference.group()?.0, reference.root()?.0)) };
        let intent = bundle.certificate()?.intent()?;
        // Every retained edge was validated before journal insertion. Include the
        // candidate without adopting it; branches may have subsequently converged.
        let mut graph = BTreeMap::<Key, BTreeSet<Key>>::new();
        for value in array(&self.raw, 3)? {
            let row = value.as_map()?;
            if matches!(get::<u8>(row, 6)?, 1 | 4) {
                let prior = get::<dto::MergePlan>(row, 4)?.intent()?;
                graph
                    .entry(key(prior.source()?)?)
                    .or_default()
                    .insert(key(prior.target()?)?);
            }
        }
        let source = key(intent.source()?)?;
        graph.entry(source).or_default().insert(key(intent.target()?)?);
        fn terminal(
            node: Key,
            graph: &BTreeMap<Key, BTreeSet<Key>>,
            visiting: &mut BTreeSet<Key>,
            cache: &mut BTreeMap<Key, Option<Key>>,
        ) -> Option<Key> {
            if let Some(result) = cache.get(&node) {
                return *result;
            }
            if !visiting.insert(node) {
                return None;
            }
            let result = if let Some(next) = graph.get(&node) {
                let mut terminal_root = None;
                for child in next {
                    let Some(root) = terminal(*child, graph, visiting, cache) else {
                        visiting.remove(&node);
                        cache.insert(node, None);
                        return None;
                    };
                    if terminal_root.is_some_and(|old| old != root) {
                        visiting.remove(&node);
                        cache.insert(node, None);
                        return None;
                    }
                    terminal_root = Some(root);
                }
                terminal_root
            } else {
                Some(node)
            };
            visiting.remove(&node);
            cache.insert(node, result);
            result
        }
        let mut ancestors = BTreeSet::from([source]);
        loop {
            let before = ancestors.len();
            for (parent, children) in &graph {
                if children.iter().any(|child| ancestors.contains(child)) {
                    ancestors.insert(*parent);
                }
            }
            if ancestors.len() == before {
                break;
            }
        }
        let mut cache = BTreeMap::new();
        Ok(ancestors
            .into_iter()
            .any(|node| terminal(node, &graph, &mut BTreeSet::new(), &mut cache).is_none()))
    }
    /// Retained, non-conflicting certificates ending at this group. Optional
    /// companion config can import its own archive after the signed transition
    /// without changing the frozen expected_result_registers in the plan.
    pub fn committed_into(&self, target: Anchor) -> Result<Vec<dto::MergeTransferBundle>> {
        let mut bundles = Vec::new();
        for value in array(&self.raw, 3)? {
            let row = value.as_map()?;
            if !matches!(get::<u8>(row, 6)?, 1 | 4) {
                continue;
            }
            let bundle = get::<dto::MergeTransferBundle>(row, 8)?;
            let intent = bundle.certificate()?.intent()?;
            if group_merge::anchor(&intent.target()?)? != target || self.history_conflict(&bundle)? {
                continue;
            }
            group_merge::verify_bundle(&bundle, group_merge::anchor(&intent.source()?)?)?;
            bundles.push(bundle);
        }
        bundles.sort_by_key(|b| {
            b.certificate()
                .and_then(|c| c.intent())
                .and_then(|i| i.merge_id())
                .expect("verified merge")
                .0
        });
        Ok(bundles)
    }
    pub fn lookup(
        &self,
        requester: DeviceId,
        source: Anchor,
        after: Option<[u8; 32]>,
    ) -> Result<Option<dto::MergeTransferBundle>> {
        let mut proofs = Vec::new();
        for value in array(&self.raw, 3)? {
            let row = value.as_map()?;
            if !matches!(get::<u8>(row, 6)?, 1 | 4) {
                continue;
            }
            let bundle = get::<dto::MergeTransferBundle>(row, 8)?;
            let intent = bundle.certificate()?.intent()?;
            if group_merge::anchor(&intent.source()?)? != source {
                continue;
            }
            let roster = crate::roster::Roster::verify(bundle.source_roster()?, source)?;
            if !roster.contains(requester) {
                continue;
            }
            proofs.push((intent.source_transition_sequence()?, intent.merge_id()?.0, bundle));
        }
        proofs.sort_by_key(|(sequence, id, _)| (*sequence, *id));
        let start = if let Some(after) = after {
            proofs
                .iter()
                .position(|(_, id, _)| *id == after)
                .ok_or(Error::Missing)?
                + 1
        } else {
            0
        };
        let result = proofs.into_iter().nth(start).map(|(_, _, bundle)| bundle);
        if result
            .as_ref()
            .map(|bundle| self.history_conflict(bundle))
            .transpose()?
            .unwrap_or(false)
        {
            return Err(Error::Conflict);
        }
        Ok(result)
    }
}
fn push_event(raw: &mut RawMap, event: RawMap) -> Result<u64> {
    let id = get::<u64>(raw, 5)?;
    let next = id.checked_add(1).ok_or(Error::Conflict)?;
    raw.insert(5, next.write());
    array_mut(raw, 7)?.push(RawValue::Map([(1, id.write()), (2, RawValue::Map(event))].into()));
    Ok(id)
}
fn release(raw: &mut RawMap, pair: PairId, decision: u64) -> Result<()> {
    for value in array_mut(raw, 4)? {
        let row = map_mut(value)?;
        if row.get(&4) == Some(&wire::pair(pair).write()) && get::<u64>(row, 3)? == decision {
            for key in 4..=6 {
                row.remove(&key);
            }
        }
    }
    Ok(())
}
fn get<T: WireValue>(raw: &RawMap, key: u32) -> Result<T> {
    Ok(T::read(raw.get(&key).ok_or(Error::Conflict)?)?)
}
fn array(raw: &RawMap, key: u32) -> Result<&Vec<RawValue>> {
    match raw.get(&key) {
        Some(RawValue::Array(v)) => Ok(v),
        _ => Err(Error::Conflict),
    }
}
fn array_mut(raw: &mut RawMap, key: u32) -> Result<&mut Vec<RawValue>> {
    match raw.get_mut(&key) {
        Some(RawValue::Array(v)) => Ok(v),
        _ => Err(Error::Conflict),
    }
}
fn map_mut(value: &mut RawValue) -> Result<&mut RawMap> {
    match value {
        RawValue::Map(v) => Ok(v),
        _ => Err(Error::Conflict),
    }
}
fn transaction(raw: &RawMap, pair: PairId, decision: u64) -> Result<Option<&RawMap>> {
    for value in array(raw, 3)? {
        let row = value.as_map()?;
        if row.get(&1) == Some(&wire::pair(pair).write()) && get::<u64>(row, 5)? == decision {
            return Ok(Some(row));
        }
    }
    Ok(None)
}
fn transaction_mut(raw: &mut RawMap, pair: PairId, decision: u64) -> Result<Option<&mut RawMap>> {
    for value in array_mut(raw, 3)? {
        let row = map_mut(value)?;
        if row.get(&1) == Some(&wire::pair(pair).write()) && get::<u64>(row, 5)? == decision {
            return Ok(Some(row));
        }
    }
    Ok(None)
}
fn known(raw: &RawMap, max: u32) -> Result<()> {
    if raw.keys().any(|k| *k == 0 || *k > max) {
        Err(Error::Conflict)
    } else {
        Ok(())
    }
}
fn validate(raw: &RawMap, local: DeviceId) -> Result<()> {
    known(raw, 9)?;
    if get::<u8>(raw, 1)? != 1 || wire::read_device(&get::<dto::DeviceId>(raw, 2)?)? != local {
        return Err(Error::Unauthorized);
    }
    let next = get::<u64>(raw, 5)?;
    if next == 0 || get::<u64>(raw, 6)? >= next {
        return Err(Error::Conflict);
    }
    let mut seen = std::collections::BTreeSet::new();
    for value in array(raw, 3)? {
        let row = value.as_map()?;
        known(row, 13)?;
        let pair = get::<dto::PairId>(row, 1)?;
        let decision = get::<u64>(row, 5)?;
        if !seen.insert((pair.encode(512)?, decision)) {
            return Err(Error::Conflict);
        }
        let plan = get::<dto::MergePlan>(row, 4)?;
        let intent = plan.intent()?;
        if pair != intent.pair_id()?
            || get::<dto::DeviceId>(row, 2)? != intent.source_issuer()?
            || get::<dto::DeviceId>(row, 3)? != intent.target_issuer()?
        {
            return Err(Error::Conflict);
        }
        let role = get::<u8>(row, 7)?;
        let status = get::<u8>(row, 6)?;
        if role > 2
            || status > 4
            || (role == 0 && wire::read_device(&intent.source_issuer()?)? != local)
            || (role == 1 && wire::read_device(&intent.target_issuer()?)? != local)
        {
            return Err(Error::Unauthorized);
        }
        get::<bool>(row, 9)?;
        get::<bool>(row, 13)?;
        if let Some(archive) = row.get(&10) {
            dto::GroupState::read(archive)?.encode(v1::limits::CONFIG_STATE)?;
        }
        if matches!(status, 1 | 4) {
            let bundle = get::<dto::MergeTransferBundle>(row, 8)?;
            if bundle.plan()? != plan || bundle.certificate()?.permit_decision()? != decision {
                return Err(Error::Conflict);
            }
            group_merge::verify_bundle(&bundle, group_merge::anchor(&intent.source()?)?)?;
        }
        if status == 2 {
            get::<dto::PairProblem>(row, 12)?;
        }
        if let Some(id) = row.get(&11)
            && (u64::read(id)? == 0 || u64::read(id)? >= next)
        {
            return Err(Error::Conflict);
        }
    }
    for value in array(raw, 4)? {
        let row = value.as_map()?;
        known(row, 6)?;
        let low = wire::read_device(&get::<dto::DeviceId>(row, 1)?)?;
        let high = wire::read_device(&get::<dto::DeviceId>(row, 2)?)?;
        if low >= high || low != local {
            return Err(Error::Unauthorized);
        }
        get::<u64>(row, 3)?;
        if row.contains_key(&4) {
            get::<dto::PairId>(row, 4)?;
            get::<dto::Hash32>(row, 5)?;
            get::<dto::Hash32>(row, 6)?;
        }
    }
    let mut previous = get::<u64>(raw, 6)?;
    for value in array(raw, 7)? {
        let row = value.as_map()?;
        known(row, 2)?;
        let id = get::<u64>(row, 1)?;
        if id <= previous || id >= next {
            return Err(Error::Conflict);
        }
        previous = id;
        get::<RawMap>(row, 2)?;
    }
    for value in array(raw, 8)? {
        let row = value.as_map()?;
        known(row, 2)?;
        get::<dto::GroupRef>(row, 1)?;
        get::<u64>(row, 2)?;
    }
    if let Some(projection) = raw.get(&9) {
        let row = projection.as_map()?;
        known(row, 3)?;
        let pair = wire::read_pair(&get::<dto::PairId>(row, 1)?)?;
        let decision = get::<u64>(row, 2)?;
        let tx = transaction(raw, pair, decision)?.ok_or(Error::Conflict)?;
        if !matches!(get::<u8>(tx, 6)?, 1 | 4)
            || get::<dto::GroupState>(row, 3)? != get::<dto::MergeTransferBundle>(tx, 8)?.target_state()?
        {
            return Err(Error::Conflict);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use oink_foundation::session::Boot;
    fn fixture() -> (Identity, Identity, State, State, dto::MergePlan) {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let source = State::create(&a, "a", "test", 1).unwrap();
        let target = State::create(&b, "b", "test", 1).unwrap();
        let initiator = Boot {
            device_id: a.id(),
            nonce: [1; 16],
        };
        let responder = Boot {
            device_id: b.id(),
            nonce: [2; 16],
        };
        let pair = PairId {
            initiator_boot: initiator,
            random: [3; 16],
        };
        let context = dto::PairContext::from_raw(
            [
                (1, wire::pair(pair).write()),
                (2, wire::boot(initiator).write()),
                (3, wire::boot(responder).write()),
                (4, source.wire().group().unwrap().write()),
                (5, target.wire().group().unwrap().write()),
                (6, 1u8.write()),
                (7, 1u8.write()),
                (8, target.wire().group().unwrap().write()),
                (9, RawValue::Bytes(vec![0; 32])),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(target.roster().anchor().root.to_vec())),
                (12, RawValue::Bytes(source.roster().anchor().root.to_vec())),
            ]
            .into(),
        )
        .unwrap();
        let seed = group_merge::seed(&source, target.roster().anchor(), 1).unwrap();
        let plan = group_merge::prepare(&source, &target, &context, &seed, 2).unwrap();
        (a, b, source, target, plan)
    }
    #[test]
    fn committed_projection_and_receipt_survive_restart_without_duplicate_outcome() {
        let (a, b, source, target, plan) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut journal = Journal::load(&path, b.id()).unwrap();
        if b.id() < a.id() {
            journal.permit(&plan).unwrap();
        }
        journal.prepare(&plan, 1, Role::Target, None).unwrap();
        let proof = group_merge::commit(
            &b,
            source.roster(),
            &target,
            &plan,
            group_merge::authorization(&a, &plan, 1).unwrap(),
            1,
        )
        .unwrap();
        journal.commit(&proof, &target).unwrap();
        drop(journal);
        let mut journal = Journal::load(&path, b.id()).unwrap();
        journal.recover().unwrap();
        let projected = journal.pending_projection().unwrap().unwrap();
        assert_eq!(projected, proof.target_state().unwrap());
        journal.projected(group_merge::digest(&projected).unwrap()).unwrap();
        let pair = wire::read_pair(&plan.intent().unwrap().pair_id().unwrap()).unwrap();
        let event = [(1, RawValue::from("completed"))].into();
        let first = journal.receipt(pair, 1, event).unwrap();
        assert_eq!(
            journal
                .receipt(pair, 1, [(1, RawValue::from("duplicate"))].into())
                .unwrap(),
            first
        );
        assert_eq!(journal.events(0).unwrap().len(), 1);
        journal.acknowledge(first).unwrap();
        drop(journal);
        let journal = Journal::load(&path, b.id()).unwrap();
        assert!(journal.events(0).unwrap().is_empty());
        assert!(journal.pending_projection().unwrap().is_none());
        assert!(matches!(
            journal
                .decision(a.id(), pair, group_merge::digest(&plan.intent().unwrap()).unwrap(), 1)
                .unwrap(),
            Decision::Committed(_)
        ));
        assert!(matches!(
            journal.decision(
                DeviceId([7; 32]),
                pair,
                group_merge::digest(&plan.intent().unwrap()).unwrap(),
                1
            ),
            Err(Error::Unauthorized)
        ));
    }
    #[test]
    fn restart_fences_target_but_source_commit_stays_uncertain_until_authenticated_decision() {
        let (a, b, source, _, plan) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let pair = wire::read_pair(&plan.intent().unwrap().pair_id().unwrap()).unwrap();
        let digest = group_merge::digest(&plan.intent().unwrap()).unwrap();
        let ap = dir.path().join("source.cbor");
        let bp = dir.path().join("target.cbor");
        let mut aj = Journal::load(&ap, a.id()).unwrap();
        let mut bj = Journal::load(&bp, b.id()).unwrap();
        aj.prepare(&plan, 1, Role::Source, Some(source.wire())).unwrap();
        aj.commit_sent(pair, 1).unwrap();
        bj.prepare(&plan, 1, Role::Target, None).unwrap();
        drop((aj, bj));
        let mut aj = Journal::load(&ap, a.id()).unwrap();
        let mut bj = Journal::load(&bp, b.id()).unwrap();
        aj.recover().unwrap();
        bj.recover().unwrap();
        assert!(matches!(
            aj.decision(b.id(), pair, digest, 1).unwrap(),
            Decision::Unconfirmed
        ));
        let Decision::Aborted(reason) = bj.decision(a.id(), pair, digest, 1).unwrap() else {
            panic!()
        };
        assert!(aj.abort(pair, 1, reason.clone()).is_err());
        aj.resolved_abort(pair, 1, b.id(), reason).unwrap();
        assert!(matches!(
            aj.decision(b.id(), pair, digest, 1).unwrap(),
            Decision::Aborted(_)
        ));
        assert!(matches!(bj.decision(a.id(), pair, digest, 2), Err(Error::Missing)));
        assert!(matches!(
            bj.query(a.id(), pair, digest, 2).unwrap(),
            Decision::Aborted(_)
        ));
    }
    #[test]
    fn failed_atomic_save_never_updates_memory_and_unknown_executable_schema_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let local = DeviceId([1; 32]);
        let mut journal = Journal::load(&path, local).unwrap();
        journal.event([(1, true.write())].into()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(journal.event([(1, false.write())].into()).is_err());
        assert_eq!(journal.events(0).unwrap().len(), 1);
        std::fs::remove_dir(&path).unwrap();
        let mut raw = journal.raw.clone();
        raw.insert(99, 1u8.write());
        let bytes = v1::cbor::encode_map(&raw, MAX_FILE).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(Journal::load(&path, local).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
