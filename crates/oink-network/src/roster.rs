//! Signed immutable membership records. Trust is pinned by the caller; received
//! self-signed roots never establish authority for an existing group.
use crate::{identity::Identity, wire_identity as wire};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = v1::Result<T>;
pub const MAX_MEMBERS: usize = 16;
pub const MAX_RECORDS: usize = 256;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub group: GroupId,
    pub root: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub device: DeviceId,
    pub epoch: u64,
    pub add: [u8; 32],
    pub name: Option<String>,
    pub os: Option<String>,
    pub issuer: DeviceId,
    pub issued_ms: u64,
}
#[derive(Clone, Debug)]
struct Record {
    wire: dto::SignedRecord,
    body: dto::RecordBody,
    subject: DeviceId,
    issuer: DeviceId,
}
#[derive(Clone, Debug)]
pub struct Roster {
    wire: dto::Roster,
    anchor: Anchor,
    records: BTreeMap<[u8; 32], Record>,
    members: BTreeMap<DeviceId, Member>,
}
impl Roster {
    pub fn create(identity: &Identity, group: GroupId, name: &str, os: &str, now_ms: u64) -> Result<Self> {
        let record = sign(
            identity,
            dto::RecordKind::Create,
            group,
            identity.id(),
            0,
            None,
            Some(name),
            Some(os),
            now_ms,
        )?;
        let root = retained::record_id(&record)?.0;
        let roster = dto::Roster::from_raw(
            [
                (1, RawValue::Bytes(group.0.to_vec())),
                (2, RawValue::Bytes(root.to_vec())),
                (3, RawValue::Array(vec![record.write()])),
            ]
            .into(),
        )?;
        Self::verify(roster, Anchor { group, root })
    }
    /// Checks structure against a locally trusted anchor, not authenticity.
    /// Callers must authenticate network senders before accepting their content.
    pub fn verify(roster: dto::Roster, anchor: Anchor) -> Result<Self> {
        if roster.group()?.0 != anchor.group.0 || roster.root_id()?.0 != anchor.root {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        roster.encode(v1::limits::CONFIG_STATE)?;
        let mut records = BTreeMap::new();
        let mut previous = None;
        for record in roster.records()?.0 {
            let record_id = retained::record_id(&record)?.0;
            if previous.is_some_and(|previous| previous > record_id) {
                return Err(v1::ErrorCode::MalformedFrame.into());
            }
            previous = Some(record_id);
            let body = dto::RecordBody::from_raw(record.body()?)?;
            if body.group()?.0 != anchor.group.0 {
                return Err(v1::ErrorCode::AuthenticationFailed.into());
            }
            let subject = wire::read_device(&body.subject()?)?;
            let issuer = wire::read_device(&body.issuer()?)?;
            let signature = record.signature()?;
            // TLS/member admission or PAKE authenticates the sender; persisted
            // and in-group content is trusted. Keep the v1 signature schema.
            if signature.algorithm()?.0 != dto::ED25519 {
                return Err(v1::ErrorCode::AuthenticationFailed.into());
            }
            match body.kind()? {
                dto::RecordKind::Create => {
                    if record_id != anchor.root
                        || subject != issuer
                        || body.epoch()? != 0
                        || body.issuer_ref()?.is_some()
                    {
                        return Err(v1::ErrorCode::AuthenticationFailed.into());
                    }
                }
                dto::RecordKind::Remove if body.name()?.is_some() || body.os()?.is_some() => {
                    return Err(v1::ErrorCode::MalformedFrame.into());
                }
                _ => {}
            }
            records.insert(
                record_id,
                Record {
                    wire: record,
                    body,
                    subject,
                    issuer,
                },
            );
        }
        let root = records.get(&anchor.root).ok_or(v1::ErrorCode::AuthenticationFailed)?;
        if root.body.kind()? != dto::RecordKind::Create {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        let mut verified = BTreeSet::new();
        for record in records.keys() {
            verify_chain(*record, anchor.root, &records, &mut verified, &mut BTreeSet::new())?;
        }
        let mut adds = BTreeMap::<DeviceId, (u64, [u8; 32])>::new();
        let mut removes = BTreeMap::<DeviceId, u64>::new();
        for (&id, record) in &records {
            match record.body.kind()? {
                dto::RecordKind::Create | dto::RecordKind::Add => {
                    let candidate = (record.body.epoch()?, id);
                    let prior = adds.entry(record.subject).or_insert(candidate);
                    *prior = (*prior).max(candidate);
                }
                dto::RecordKind::Remove => {
                    let epoch = record.body.epoch()?;
                    let prior = removes.entry(record.subject).or_insert(epoch);
                    *prior = (*prior).max(epoch);
                }
                _ => {}
            }
        }
        let mut members = BTreeMap::new();
        for (device, (epoch, add)) in adds {
            if removes.get(&device).is_some_and(|removed| epoch <= *removed) {
                continue;
            }
            let record = &records[&add];
            members.insert(
                device,
                Member {
                    device,
                    epoch,
                    add,
                    name: record.body.name()?.map(|n| n.0),
                    os: record.body.os()?.map(|n| n.0),
                    issuer: record.issuer,
                    issued_ms: record.body.issued_ms()?,
                },
            );
        }
        if members.len() > MAX_MEMBERS || records.len() > MAX_RECORDS {
            return Err(v1::ErrorCode::LimitExceeded.into());
        }
        let mut raw = roster.into_raw();
        raw.insert(3, RawValue::Array(records.values().map(|r| r.wire.write()).collect()));
        Ok(Self {
            wire: dto::Roster::from_raw(raw)?,
            anchor,
            records,
            members,
        })
    }
    pub fn anchor(&self) -> Anchor {
        self.anchor
    }
    pub fn wire(&self) -> &dto::Roster {
        &self.wire
    }
    pub fn digest(&self) -> Result<[u8; 32]> {
        Ok(retained::sha256(&self.wire.encode(v1::limits::CONFIG_STATE)?).0)
    }
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.values()
    }
    pub fn member(&self, id: DeviceId) -> Option<&Member> {
        self.members.get(&id)
    }
    pub fn contains(&self, id: DeviceId) -> bool {
        self.members.contains_key(&id)
    }
    pub fn merge(&self, incoming: &dto::Roster) -> Result<Self> {
        let other = Self::verify(incoming.clone(), self.anchor)?;
        let mut records = self.records.clone();
        records.extend(other.records);
        let mut raw = retained::merge_extensions(self.wire.raw(), incoming.raw(), &[1, 2, 3])?;
        raw.insert(
            3,
            RawValue::Array(records.values().map(|record| record.wire.write()).collect()),
        );
        Self::verify(dto::Roster::from_raw(raw)?, self.anchor)
    }
    pub fn add(&self, signer: &Identity, subject: DeviceId, name: &str, os: &str, now_ms: u64) -> Result<Self> {
        self.edit(signer, dto::RecordKind::Add, subject, Some(name), Some(os), now_ms)
    }
    pub fn remove(
        &self,
        signer: &Identity,
        subject: DeviceId,
        expected_add: Option<[u8; 32]>,
        now_ms: u64,
    ) -> Result<Self> {
        if let Some(expected) = expected_add
            && self.member(subject).map(|m| m.add) != Some(expected)
        {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        self.edit(signer, dto::RecordKind::Remove, subject, None, None, now_ms)
    }
    pub fn next_epoch(&self, subject: DeviceId) -> Result<u64> {
        self.records
            .values()
            .filter(|record| {
                record.subject == subject
                    && matches!(
                        record.body.kind(),
                        Ok(dto::RecordKind::Create | dto::RecordKind::Add | dto::RecordKind::Remove)
                    )
            })
            .map(|record| record.body.epoch())
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .map_or(Ok(1), |epoch| {
                epoch.checked_add(1).ok_or(v1::ErrorCode::LimitExceeded.into())
            })
    }
    pub fn propose_add(
        &self,
        issuer: DeviceId,
        subject: DeviceId,
        name: Option<&str>,
        os: Option<&str>,
        now: u64,
    ) -> Result<dto::RecordBody> {
        let reference = self.member(issuer).ok_or(v1::ErrorCode::AuthenticationFailed)?.add;
        body(
            dto::RecordKind::Add,
            self.anchor.group,
            subject,
            self.next_epoch(subject)?,
            issuer,
            Some(reference),
            name,
            os,
            now,
        )
    }
    pub fn sign_adds(&self, identity: &Identity, proposals: &[dto::RecordBody]) -> Result<Self> {
        let mut result = self.clone();
        for proposal in proposals {
            let subject = wire::read_device(&proposal.subject()?)?;
            let expected = result.propose_add(
                identity.id(),
                subject,
                proposal.name()?.as_ref().map(|s| s.0.as_str()),
                proposal.os()?.as_ref().map(|s| s.0.as_str()),
                proposal.issued_ms()?,
            )?;
            if result.contains(subject) || (1..=9).any(|key| proposal.raw().get(&key) != expected.raw().get(&key)) {
                return Err(v1::ErrorCode::StateConflict.into());
            }
            result = result.insert(sign_body(identity, proposal.clone())?)?;
        }
        Ok(result)
    }
    /// Validate exact proposed bodies and forbid unplanned new signed records.
    pub fn accepts_exact_adds(&self, result: &Roster, proposals: &[dto::RecordBody]) -> Result<bool> {
        if self.anchor != result.anchor
            || self
                .records
                .iter()
                .any(|(id, old)| result.records.get(id).is_none_or(|new| new.wire != old.wire))
        {
            return Ok(false);
        }
        let added = result
            .records
            .iter()
            .filter(|(id, _)| !self.records.contains_key(*id))
            .map(|(_, r)| r.body.clone())
            .collect::<Vec<_>>();
        Ok(added.len() == proposals.len() && proposals.iter().all(|p| added.contains(p)))
    }
    fn edit(
        &self,
        signer: &Identity,
        kind: dto::RecordKind,
        subject: DeviceId,
        name: Option<&str>,
        os: Option<&str>,
        now_ms: u64,
    ) -> Result<Self> {
        let authority = self.member(signer.id()).ok_or(v1::ErrorCode::AuthenticationFailed)?;
        let epoch = self.next_epoch(subject)?;
        let record = sign(
            signer,
            kind,
            self.anchor.group,
            subject,
            epoch,
            Some(authority.add),
            name,
            os,
            now_ms,
        )?;
        self.insert(record)
    }
    fn insert(&self, record: dto::SignedRecord) -> Result<Self> {
        let mut records = self
            .records
            .iter()
            .map(|(id, record)| (*id, record.wire.clone()))
            .collect::<BTreeMap<_, _>>();
        records.insert(retained::record_id(&record)?.0, record);
        let mut raw = self.wire.raw().clone();
        raw.insert(3, RawValue::Array(records.values().map(WireValue::write).collect()));
        Self::verify(dto::Roster::from_raw(raw)?, self.anchor)
    }
}
fn verify_chain(
    id: [u8; 32],
    root: [u8; 32],
    records: &BTreeMap<[u8; 32], Record>,
    verified: &mut BTreeSet<[u8; 32]>,
    visiting: &mut BTreeSet<[u8; 32]>,
) -> Result<()> {
    if verified.contains(&id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let record = records.get(&id).ok_or(v1::ErrorCode::AuthenticationFailed)?;
    if id != root {
        let parent = record.body.issuer_ref()?.ok_or(v1::ErrorCode::AuthenticationFailed)?.0;
        let authority = records.get(&parent).ok_or(v1::ErrorCode::AuthenticationFailed)?;
        if authority.subject != record.issuer
            || !matches!(authority.body.kind()?, dto::RecordKind::Create | dto::RecordKind::Add)
        {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        verify_chain(parent, root, records, verified, visiting)?;
    }
    visiting.remove(&id);
    verified.insert(id);
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn sign(
    identity: &Identity,
    kind: dto::RecordKind,
    group: GroupId,
    subject: DeviceId,
    epoch: u64,
    issuer_ref: Option<[u8; 32]>,
    name: Option<&str>,
    os: Option<&str>,
    now_ms: u64,
) -> Result<dto::SignedRecord> {
    sign_body(
        identity,
        body(kind, group, subject, epoch, identity.id(), issuer_ref, name, os, now_ms)?,
    )
}
#[allow(clippy::too_many_arguments)]
fn body(
    kind: dto::RecordKind,
    group: GroupId,
    subject: DeviceId,
    epoch: u64,
    issuer: DeviceId,
    issuer_ref: Option<[u8; 32]>,
    name: Option<&str>,
    os: Option<&str>,
    now: u64,
) -> Result<dto::RecordBody> {
    let mut raw: RawMap = [
        (1, kind.write()),
        (2, RawValue::Bytes(group.0.to_vec())),
        (3, wire::device(subject).write()),
        (4, epoch.write()),
        (5, wire::device(issuer).write()),
        (9, now.write()),
    ]
    .into();
    if let Some(reference) = issuer_ref {
        raw.insert(6, RawValue::Bytes(reference.to_vec()));
    }
    if let Some(name) = name {
        raw.insert(7, RawValue::from(name));
    }
    if let Some(os) = os {
        raw.insert(8, RawValue::from(os));
    }
    dto::RecordBody::from_raw(raw)
}
fn sign_body(identity: &Identity, body: dto::RecordBody) -> Result<dto::SignedRecord> {
    let signature =
        dto::Signature::from_raw([(1, RawValue::from(dto::ED25519)), (2, RawValue::Bytes(vec![0; 64]))].into())?;
    let mut record = dto::SignedRecord::from_raw([(1, body.write()), (2, signature.write())].into())?;
    let input = retained::record_signature_input(&record)?;
    let signature = dto::Signature::from_raw(
        [
            (1, RawValue::from(dto::ED25519)),
            (2, RawValue::Bytes(identity.sign(&input).to_vec())),
        ]
        .into(),
    )?;
    let mut raw = record.into_raw();
    raw.insert(2, signature.write());
    record = dto::SignedRecord::from_raw(raw)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removal_wins_and_historical_issuer_does_not_revoke_descendants() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let c = Identity::from_seed([3; 32]);
        let root = Roster::create(&a, GroupId([1; 16]), "a", "test", 0).unwrap();
        let ab = root.add(&a, b.id(), "b", "test", 1).unwrap();
        let abc = ab.add(&b, c.id(), "c", "test", 2).unwrap();
        let removed = abc.remove(&a, b.id(), None, 3).unwrap();
        assert!(!removed.contains(b.id()));
        assert!(removed.contains(c.id()));
        assert!(!removed.merge(abc.wire()).unwrap().contains(b.id()));
        assert!(removed.add(&b, DeviceId([4; 32]), "d", "test", 4).is_err());
        let rejoined = removed.add(&a, b.id(), "b", "test", 4).unwrap();
        assert!(rejoined.contains(b.id()));
    }
    #[test]
    fn foreign_roots_changed_root_digests_and_unanchored_adds_are_rejected() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let root = Roster::create(&a, GroupId([1; 16]), "a", "test", 0).unwrap();
        let foreign = Roster::create(&b, GroupId([1; 16]), "b", "test", 0).unwrap();
        assert!(root.merge(foreign.wire()).is_err());
        let mut raw = root.wire().raw().clone();
        let RawValue::Array(records) = raw.get_mut(&3).unwrap() else {
            panic!()
        };
        let RawValue::Map(record) = &mut records[0] else {
            panic!()
        };
        let RawValue::Map(body) = record.get_mut(&1).unwrap() else {
            panic!()
        };
        body.insert(7, RawValue::from("tampered"));
        assert!(Roster::verify(dto::Roster::from_raw(raw).unwrap(), root.anchor()).is_err());
        let unanchored = sign(
            &b,
            dto::RecordKind::Add,
            root.anchor.group,
            b.id(),
            1,
            Some([0; 32]),
            Some("b"),
            Some("test"),
            0,
        )
        .unwrap();
        assert!(root.insert(unanchored).is_err());
    }
    #[test]
    fn full_trust_roster_keeps_signature_format_and_record_digests_without_verifying_signatures() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let root = Roster::create(&a, GroupId([1; 16]), "a", "test", 0).unwrap();
        let added = root.add(&a, b.id(), "b", "test", 1).unwrap();
        let record = &added.records[&added.member(b.id()).unwrap().add].wire;
        let mut raw = record.raw().clone();
        raw.insert(
            2,
            RawValue::Map([(1, RawValue::from(dto::ED25519)), (2, RawValue::Bytes(vec![0; 64]))].into()),
        );
        let record = dto::SignedRecord::from_raw(raw).unwrap();
        crate::identity::STRICT_VERIFICATIONS.with(|count| count.set(0));
        let accepted = root.insert(record.clone()).unwrap();
        assert!(accepted.contains(b.id()));
        assert_eq!(
            accepted.member(b.id()).unwrap().add,
            retained::record_id(&record).unwrap().0
        );
        crate::identity::STRICT_VERIFICATIONS.with(|count| assert_eq!(count.get(), 0));
        let mut raw = record.into_raw();
        raw.insert(
            2,
            RawValue::Map([(1, RawValue::from(dto::ED25519)), (2, RawValue::Bytes(vec![0; 63]))].into()),
        );
        assert!(dto::SignedRecord::from_raw(raw).is_err());
    }
    #[test]
    fn equal_epoch_removal_wins_and_signed_unknown_data_is_retained() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let root = Roster::create(&a, GroupId([1; 16]), "a", "test", 0).unwrap();
        let add = root.add(&a, b.id(), "b", "test", 1).unwrap();
        let remove = sign(
            &a,
            dto::RecordKind::Remove,
            root.anchor.group,
            b.id(),
            1,
            Some(root.anchor.root),
            None,
            None,
            2,
        )
        .unwrap();
        let removed = add.insert(remove).unwrap();
        assert!(!removed.contains(b.id()));
        let unknown = sign(
            &a,
            dto::RecordKind::Unknown(100),
            root.anchor.group,
            b.id(),
            99,
            Some(root.anchor.root),
            None,
            None,
            3,
        )
        .unwrap();
        let unknown_id = retained::record_id(&unknown).unwrap().0;
        let kept = removed.insert(unknown.clone()).unwrap();
        assert!(!kept.contains(b.id()));
        assert_eq!(kept.records[&unknown_id].wire, unknown);
    }
}
