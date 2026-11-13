//! Committed merge proofs written by 0.3.x into the pairing journal.
//!
//! 0.3.x committed every merge under the format-1 object definitions: SHA-256
//! object digests, roster record ids `SHA-256(ROSTER_RECORD || record)` over the
//! full signed record, a roster root id (Roster key 2), issuer references
//! (RecordBody key 6) and register digests (Register key 6). v2 deliberately
//! replaced all of them (BLAKE3 object digests, projected record ids, no
//! register digest), so recomputing an old certificate under v2 can never
//! reproduce its `plan_digest`, `result_digest` or the intent's roster/state
//! digests and member references. An upgraded journal still holds those
//! proofs, and `pair_journal::validate` must keep checking them; this module
//! checks them exactly as 0.3.x did, under the definitions they were committed
//! with. It is used only for retained journal rows; network-received bundles
//! are always verified by `group_merge::verify_bundle`.
use crate::{group_merge, group_state, roster::Anchor, wire_identity as wire};
use oink_foundation::DeviceId;
use oink_protocol::v2::{
    self, RawMap, RawValue, dto,
    schema::{WireDto, WireValue, defaulted, required},
};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = v2::Result<T>;

/// Format-1 fields that v2 retired; 0.3.x always wrote each of them.
const ROSTER_ROOT_ID: u32 = 2;
const RECORD_SIGNATURE: u32 = 2;
const RECORD_ISSUER_REF: u32 = 6;
const REGISTER_DIGEST: u32 = 6;
const LAYOUT_REGISTER: u32 = 2;

/// The v2 projection of a register stored in the 0.3.x shape (register digest
/// key 6, visibility envelope key 7). Reading only; nothing is migrated.
fn visibility_ids<T: WireValue>(metadata: &RawMap, key: u32) -> v2::Result<Vec<T>> {
    if metadata.contains_key(&key) {
        Ok(required::<v2::schema::List<T, 256>>(metadata, key)?.0)
    } else {
        Ok(Vec::new())
    }
}

pub fn project_register(register: dto::Register) -> v2::Result<dto::Register> {
    if !register.raw().contains_key(&6) && !register.raw().contains_key(&7) {
        return Ok(register);
    }
    let mut raw = register.raw().clone();
    raw.remove(&6);
    let metadata = raw.remove(&7);
    if register.kind()? != 2 {
        return dto::Register::from_raw(raw);
    }
    let metadata = match metadata.as_ref() {
        None | Some(RawValue::Null) => return dto::Register::from_raw(raw),
        Some(metadata) => metadata.as_map()?,
    };
    let hidden_devices = visibility_ids::<dto::DeviceId>(metadata, 1)?;
    let hidden_screens = visibility_ids::<dto::ScreenId>(metadata, 2)?;
    let known_devices = visibility_ids::<dto::DeviceId>(metadata, 3)?;
    let known_screens = visibility_ids::<dto::ScreenId>(metadata, 4)?;

    let mut layout = register.value()?;
    let mut devices = Vec::new();
    for device in dto::LayoutValue::from_raw(layout.clone())?.devices()?.0 {
        let id = device.device()?;
        let hidden = hidden_devices.contains(&id);
        let mut fields = device.raw().clone();
        if hidden || known_devices.contains(&id) {
            fields.entry(5).or_insert_with(|| hidden.write());
        }
        let mut screens = Vec::new();
        for screen in device.screens()?.0 {
            let id = screen.id()?;
            let hidden = hidden_screens.contains(&id);
            let mut fields = screen.into_raw();
            if hidden || known_screens.contains(&id) {
                fields.entry(11).or_insert_with(|| hidden.write());
            }
            screens.push(RawValue::Map(fields));
        }
        fields.insert(4, RawValue::Array(screens));
        devices.push(RawValue::Map(fields));
    }
    layout.insert(1, RawValue::Array(devices));
    raw.insert(4, RawValue::Map(layout));
    dto::Register::from_raw(raw)
}

/// The v2 projection of a whole GroupState stored in the 0.3.x shape, the
/// same projection the stores use when they read 0.3.x data.
pub fn project_state(state: &dto::GroupState) -> Result<dto::GroupState> {
    let roster = state.roster()?;
    let registers = state.registers()?.0;
    if !roster.raw().contains_key(&ROSTER_ROOT_ID)
        && !registers
            .iter()
            .any(|register| register.raw().contains_key(&REGISTER_DIGEST) || register.raw().contains_key(&7))
    {
        return Ok(state.clone());
    }
    let anchor = Anchor {
        group: oink_foundation::GroupId(state.group()?.0),
        root: crate::roster::root_id(&roster)?,
    };
    let roster = crate::roster::Roster::verify(roster, anchor)?;
    let registers = registers
        .into_iter()
        .map(project_register)
        .map(|register| register.map(|register| register.write()))
        .collect::<Result<Vec<_>>>()?;
    let mut raw = state.raw().clone();
    raw.insert(2, RawValue::Array(registers));
    raw.insert(3, roster.wire().write());
    dto::GroupState::from_raw(raw)
}

/// The v2 anchors (group id, v2 root of the embedded roster) of a bundle's
/// source and target. A format-1 certificate names 0.3.x roots; members that
/// upgraded know their group by the v2 root of the same roster (M33).
pub fn anchors(bundle: &dto::MergeTransferBundle) -> Result<(Anchor, Anchor)> {
    let intent = bundle.certificate()?.intent()?;
    if !is_format1(bundle)? {
        return Ok((
            group_merge::anchor(&intent.source()?)?,
            group_merge::anchor(&intent.target()?)?,
        ));
    }
    let source = Anchor {
        group: oink_foundation::GroupId(intent.source()?.group()?.0),
        root: crate::roster::root_id(&bundle.source_roster()?)?,
    };
    let target = Anchor {
        group: oink_foundation::GroupId(intent.target()?.group()?.0),
        root: crate::roster::root_id(&bundle.target_state()?.roster()?)?,
    };
    Ok((source, target))
}

/// Verify a merge proof under the definitions it was certified with.
pub fn verify_any(bundle: &dto::MergeTransferBundle) -> Result<()> {
    let certified = group_merge::anchor(&bundle.certificate()?.intent()?.source()?)?;
    if is_format1(bundle)? {
        verify_bundle(bundle, certified)
    } else {
        group_merge::verify_bundle(bundle, certified).map(|_| ())
    }
}

/// The v2 result of adopting a format-1 proof as `local`, a member of its
/// source group that upgraded before it learned the merge.
pub fn adopt(
    bundle: &dto::MergeTransferBundle,
    current: &group_state::State,
    local: DeviceId,
) -> Result<group_state::State> {
    verify_any(bundle)?;
    let (source, target) = anchors(bundle)?;
    let intent = bundle.certificate()?.intent()?;
    let imported = intent
        .members()?
        .0
        .iter()
        .any(|m| m.device().and_then(|d| wire::read_device(&d)) == Ok(local));
    if current.roster().anchor() != source || !current.roster().contains(local) || !imported {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let result = group_state::State::verify(project_state(&bundle.target_state()?)?, target)?;
    if !result.roster().contains(local) {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    Ok(result)
}

/// A bundle is format 1 when its source roster carries the roster root id.
/// 0.3.x required that field in every roster; v2 never writes it (its roster
/// projection drops it), so the proof's own encoding names its definitions.
pub fn is_format1(bundle: &dto::MergeTransferBundle) -> Result<bool> {
    Ok(bundle.source_roster()?.raw().contains_key(&ROSTER_ROOT_ID))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn object_digest<T: WireDto>(value: &T, limit: usize) -> Result<[u8; 32]> {
    Ok(sha256(&value.encode(limit)?))
}
fn record_id(record: &dto::SignedRecord) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(v2::labels::ROSTER_RECORD);
    hash.update(record.encode(v2::limits::CONFIG_STATE)?);
    Ok(hash.finalize().into())
}
fn issuer_ref(body: &dto::RecordBody) -> Result<Option<[u8; 32]>> {
    Ok(defaulted::<Option<dto::Hash32>>(body.raw(), RECORD_ISSUER_REF, || None)?.map(|r| r.0))
}
fn register_digest(raw: &RawMap) -> Result<[u8; 32]> {
    let mut body = raw.clone();
    body.remove(&REGISTER_DIGEST);
    body.entry(5).or_insert(RawValue::Bool(false));
    Ok(sha256(&RawValue::Map(body).encode(v2::limits::CONFIG_STATE)?))
}
fn verify_register(register: &dto::Register) -> Result<()> {
    if register_digest(register.raw())? != required::<dto::Hash32>(register.raw(), REGISTER_DIGEST)?.0 {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    Ok(())
}
fn check_signature_format(signature: &dto::Signature) -> Result<()> {
    if signature.algorithm()?.0 != dto::ED25519 {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    Ok(())
}

struct Record {
    wire: dto::SignedRecord,
    body: dto::RecordBody,
    subject: DeviceId,
    issuer: DeviceId,
}
struct Member {
    device: DeviceId,
    epoch: u64,
    add: [u8; 32],
    name: Option<String>,
    os: Option<String>,
}
struct Roster {
    wire: dto::Roster,
    anchor: Anchor,
    records: BTreeMap<[u8; 32], Record>,
    members: BTreeMap<DeviceId, Member>,
}
impl Roster {
    fn verify(roster: dto::Roster, anchor: Anchor) -> Result<Self> {
        if roster.group()?.0 != anchor.group.0
            || required::<dto::Hash32>(roster.raw(), ROSTER_ROOT_ID)?.0 != anchor.root
        {
            return Err(v2::ErrorCode::AuthenticationFailed.into());
        }
        roster.encode(v2::limits::CONFIG_STATE)?;
        let mut records = BTreeMap::new();
        let mut previous = None;
        for record in roster.records()?.0 {
            let id = record_id(&record)?;
            if previous.is_some_and(|previous| previous > id) {
                return Err(v2::ErrorCode::MalformedFrame.into());
            }
            previous = Some(id);
            let body = dto::RecordBody::from_raw(record.body()?)?;
            if body.group()?.0 != anchor.group.0 {
                return Err(v2::ErrorCode::AuthenticationFailed.into());
            }
            let subject = wire::read_device(&body.subject()?)?;
            let issuer = wire::read_device(&body.issuer()?)?;
            check_signature_format(&required::<dto::Signature>(record.raw(), RECORD_SIGNATURE)?)?;
            match body.kind()? {
                dto::RecordKind::Create => {
                    if id != anchor.root || subject != issuer || body.epoch()? != 0 || issuer_ref(&body)?.is_some() {
                        return Err(v2::ErrorCode::AuthenticationFailed.into());
                    }
                }
                dto::RecordKind::Remove if body.name()?.is_some() || body.os()?.is_some() => {
                    return Err(v2::ErrorCode::MalformedFrame.into());
                }
                _ => {}
            }
            records.insert(
                id,
                Record {
                    wire: record,
                    body,
                    subject,
                    issuer,
                },
            );
        }
        let root = records.get(&anchor.root).ok_or(v2::ErrorCode::AuthenticationFailed)?;
        if root.body.kind()? != dto::RecordKind::Create {
            return Err(v2::ErrorCode::AuthenticationFailed.into());
        }
        let mut verified = BTreeSet::new();
        for id in records.keys() {
            verify_chain(*id, anchor.root, &records, &mut verified, &mut BTreeSet::new())?;
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
                },
            );
        }
        if members.len() > crate::roster::MAX_MEMBERS || records.len() > crate::roster::MAX_RECORDS {
            return Err(v2::ErrorCode::LimitExceeded.into());
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
    fn digest(&self) -> Result<[u8; 32]> {
        object_digest(&self.wire, v2::limits::CONFIG_STATE)
    }
    fn member(&self, id: DeviceId) -> Option<&Member> {
        self.members.get(&id)
    }
    fn contains(&self, id: DeviceId) -> bool {
        self.members.contains_key(&id)
    }
    fn next_epoch(&self, subject: DeviceId) -> Result<u64> {
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
                epoch.checked_add(1).ok_or(v2::ErrorCode::LimitExceeded.into())
            })
    }
    fn accepts_exact_adds(&self, result: &Roster, proposals: &[dto::RecordBody]) -> bool {
        if self.anchor != result.anchor
            || self
                .records
                .iter()
                .any(|(id, old)| result.records.get(id).is_none_or(|new| new.wire != old.wire))
        {
            return false;
        }
        let added = result
            .records
            .iter()
            .filter(|(id, _)| !self.records.contains_key(*id))
            .map(|(_, r)| &r.body)
            .collect::<Vec<_>>();
        added.len() == proposals.len() && proposals.iter().all(|p| added.contains(&p))
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
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let record = records.get(&id).ok_or(v2::ErrorCode::AuthenticationFailed)?;
    if id != root {
        let parent = issuer_ref(&record.body)?.ok_or(v2::ErrorCode::AuthenticationFailed)?;
        let authority = records.get(&parent).ok_or(v2::ErrorCode::AuthenticationFailed)?;
        if authority.subject != record.issuer
            || !matches!(authority.body.kind()?, dto::RecordKind::Create | dto::RecordKind::Add)
        {
            return Err(v2::ErrorCode::AuthenticationFailed.into());
        }
        verify_chain(parent, root, records, verified, visiting)?;
    }
    visiting.remove(&id);
    verified.insert(id);
    Ok(())
}

struct State {
    wire: dto::GroupState,
    roster: Roster,
}
impl State {
    fn verify(state: dto::GroupState, anchor: Anchor) -> Result<Self> {
        if state.group()?.0 != anchor.group.0 {
            return Err(v2::ErrorCode::AuthenticationFailed.into());
        }
        state.encode(v2::limits::CONFIG_STATE)?;
        let roster = Roster::verify(state.roster()?, anchor)?;
        let mut layout = false;
        for register in state.registers()?.0 {
            verify_register(&register)?;
            match register.kind()? {
                LAYOUT_REGISTER => {
                    group_state::validate_layout(&dto::LayoutValue::from_raw(register.value()?)?)?;
                    layout = true;
                }
                crate::pointer_slots::REGISTER_KIND => check_pointer_slots(&register)?,
                _ => {}
            }
        }
        if !layout {
            return Err(v2::ErrorCode::MalformedFrame.into());
        }
        Ok(Self { wire: state, roster })
    }
    fn digest(&self) -> Result<[u8; 32]> {
        object_digest(&self.wire, v2::limits::CONFIG_STATE)
    }
    fn layout(&self) -> Result<dto::Register> {
        self.wire
            .registers()?
            .0
            .into_iter()
            .find(|r| r.kind() == Ok(LAYOUT_REGISTER))
            .ok_or(v2::ErrorCode::MalformedFrame.into())
    }
}
/// The structural rejections of `State::pointer_slots`, which 0.3.x applied to
/// every verified state.
fn check_pointer_slots(register: &dto::Register) -> Result<()> {
    if register.deleted()? {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    let mut used = BTreeSet::new();
    let mut devices = BTreeSet::new();
    for entry in dto::PointerShortcutSlots::from_raw(register.value()?)?.entries()?.0 {
        let device = wire::read_device(&entry.device()?)?;
        if entry.slot()?.is_some_and(|s| s >= 10 || !used.insert(s)) || !devices.insert(device) {
            return Err(v2::ErrorCode::StateConflict.into());
        }
    }
    Ok(())
}

fn known_fields(raw: &RawMap, fields: &[u32]) -> Result<()> {
    if raw.keys().any(|key| !fields.contains(key)) {
        return Err(v2::ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
/// The 0.3.x executable-geometry boundary (narrower than v2's).
fn require_supported_geometry(layout: &dto::LayoutValue) -> Result<()> {
    known_fields(layout.raw(), &[1, 2, 4])?;
    if !matches!(layout.source()?, dto::LayoutSource::Default | dto::LayoutSource::Manual) {
        return Err(v2::ErrorCode::UnsupportedValue.into());
    }
    let mut devices = BTreeSet::new();
    let mut total = 0;
    for group in layout.devices()?.0 {
        known_fields(group.raw(), &[1, 2, 3, 4])?;
        group.device()?.require_supported()?;
        let device = group.device()?.ordering_key()?;
        if !devices.insert(device.clone()) {
            return Err(v2::ErrorCode::StateConflict.into());
        }
        let origin = group.origin()?;
        known_fields(origin.raw(), &[1, 2])?;
        let (x, y, k) = (origin.x()?, origin.y()?, group.scale()?);
        if !x.is_finite() || !y.is_finite() || !k.is_finite() || k <= 0.0 {
            return Err(v2::ErrorCode::UnsupportedValue.into());
        }
        let mut screens = BTreeSet::new();
        for screen in group.screens()?.0 {
            total += 1;
            known_fields(screen.raw(), &[1, 2, 3, 5, 9, 10])?;
            let id = screen.id()?;
            id.device()?.require_supported()?;
            if id.device()?.ordering_key()? != device || !screens.insert(id.display()?.0) {
                return Err(v2::ErrorCode::StateConflict.into());
            }
            if !matches!(
                screen.size_source()?,
                dto::SizeSource::Reported | dto::SizeSource::Estimated | dto::SizeSource::Manual
            ) {
                return Err(v2::ErrorCode::UnsupportedValue.into());
            }
            let r = screen.local()?;
            known_fields(r.raw(), &[1, 2, 3, 4])?;
            let (rx, ry, rw, rh) = (r.x()?, r.y()?, r.w()?, r.h()?);
            if rw <= 0.0
                || rh <= 0.0
                || ![
                    rx,
                    ry,
                    rw,
                    rh,
                    rx + rw,
                    ry + rh,
                    x + rx * k,
                    y + ry * k,
                    x + (rx + rw) * k,
                    y + (ry + rh) * k,
                ]
                .into_iter()
                .all(f64::is_finite)
            {
                return Err(v2::ErrorCode::UnsupportedValue.into());
            }
        }
    }
    if total > 256 {
        return Err(v2::ErrorCode::LimitExceeded.into());
    }
    known_fields(layout.topology()?.raw(), &[6, 7, 8, 9, 10, 12])
}
/// 0.3.x register edit: bumps the version and recomputes the register digest.
fn edit_register(current: &dto::Register, writer: DeviceId, value: RawMap) -> Result<dto::Register> {
    verify_register(current)?;
    let expected = current.version()?;
    let counter = expected.counter()?.checked_add(1).ok_or(v2::ErrorCode::LimitExceeded)?;
    let mut raw = current.raw().clone();
    let mut version = expected.raw().clone();
    version.insert(1, counter.write());
    version.insert(3, wire::device(writer).write());
    raw.insert(3, RawValue::Map(version));
    raw.insert(4, RawValue::Map(value));
    raw.entry(5).or_insert(false.write());
    raw.insert(REGISTER_DIGEST, RawValue::Bytes(register_digest(&raw)?.to_vec()));
    RawValue::Map(raw.clone()).encode(v2::limits::CONFIG_STATE)?;
    dto::Register::from_raw(raw)
}
fn screen_key(group: &dto::DevicePlacement) -> Result<DeviceId> {
    wire::read_device(&group.device()?)
}
fn apply_geometry(target: &State, patch: &dto::GeometryPatch, writer: DeviceId) -> Result<Vec<dto::Register>> {
    if patch.raw().keys().any(|key| !matches!(key, 1 | 3 | 4)) || patch.source()? != dto::LayoutSource::Manual {
        return Err(v2::ErrorCode::UnsupportedValue.into());
    }
    let appended = patch.appended_devices()?.0;
    let mut registers = target.wire.registers()?.0;
    let current = target.layout()?;
    let layout = dto::LayoutValue::from_raw(current.value()?)?;
    require_supported_geometry(&layout)?;
    if appended.is_empty() {
        return Ok(registers);
    }
    let mut keep = layout.devices()?.0;
    let mut keys = keep.iter().map(screen_key).collect::<Result<BTreeSet<_>>>()?;
    let mut previous = None;
    for screen in &appended {
        let key = screen_key(screen)?;
        if !keys.insert(key) || previous.as_ref().is_some_and(|p| p >= &key) {
            return Err(v2::ErrorCode::StateConflict.into());
        }
        previous = Some(key);
    }
    if keep.len() + appended.len() > 256 {
        return Err(v2::ErrorCode::LimitExceeded.into());
    }
    keep.extend(appended);
    let mut raw = layout.into_raw();
    raw.insert(1, RawValue::Array(keep.iter().map(WireValue::write).collect()));
    raw.insert(2, dto::LayoutSource::Manual.write());
    let layout = dto::LayoutValue::from_raw(raw)?;
    require_supported_geometry(&layout)?;
    let edited = edit_register(&current, writer, layout.into_raw())?;
    *registers
        .iter_mut()
        .find(|r| r.kind() == Ok(LAYOUT_REGISTER))
        .ok_or(v2::ErrorCode::MalformedFrame)? = edited;
    Ok(registers)
}
fn validate_plan(source: &Roster, plan: &dto::MergePlan) -> Result<State> {
    let intent = plan.intent()?;
    if source.anchor != group_merge::anchor(&intent.source()?)? || source.digest()? != intent.source_roster_digest()?.0
    {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let source_issuer = wire::read_device(&intent.source_issuer()?)?;
    let target_issuer = wire::read_device(&intent.target_issuer()?)?;
    if wire::read_pair(&intent.pair_id()?)?.initiator_boot.device_id != source_issuer
        || intent.source()?.group()? == intent.target()?.group()?
    {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    if source.member(source_issuer).map(|m| m.add) != Some(intent.source_issuer_ref()?.0) {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let target = State::verify(plan.target_base()?, group_merge::anchor(&intent.target()?)?)?;
    if target.digest()? != intent.target_base_digest()?.0
        || target.roster.member(target_issuer).map(|m| m.add) != Some(intent.target_issuer_ref()?.0)
    {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let imports = intent.members()?.0;
    let actual = source.members.values().collect::<Vec<_>>();
    if imports.len() != actual.len() {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    let mut expected = BTreeMap::new();
    let mut union = target.roster.members.keys().copied().collect::<BTreeSet<_>>();
    for (import, member) in imports.iter().zip(actual) {
        let device = wire::read_device(&import.device()?)?;
        let epoch = target
            .roster
            .member(device)
            .map(|m| Ok(m.epoch))
            .unwrap_or_else(|| target.roster.next_epoch(device))?;
        if device != member.device || import.source_add()?.0 != member.add || import.target_epoch()? != epoch {
            return Err(v2::ErrorCode::StateConflict.into());
        }
        union.insert(device);
        if !target.roster.contains(device) {
            expected.insert(device, epoch);
        }
    }
    if union.len() > crate::roster::MAX_MEMBERS {
        return Err(v2::ErrorCode::LimitExceeded.into());
    }
    let proposals = plan.unsigned_adds()?.0;
    if proposals.len() != expected.len()
        || proposals.len() + target.roster.wire.records()?.0.len() > crate::roster::MAX_RECORDS
    {
        return Err(v2::ErrorCode::LimitExceeded.into());
    }
    let mut seen = BTreeSet::new();
    for proposal in proposals {
        let subject = wire::read_device(&proposal.subject()?)?;
        if !seen.insert(subject)
            || expected.get(&subject) != Some(&proposal.epoch()?)
            || proposal.kind()? != dto::RecordKind::Add
            || proposal.group()?.0 != target.roster.anchor.group.0
            || wire::read_device(&proposal.issuer()?)? != target_issuer
            || issuer_ref(&proposal)? != Some(intent.target_issuer_ref()?.0)
        {
            return Err(v2::ErrorCode::AuthenticationFailed.into());
        }
        let member = source.member(subject).ok_or(v2::ErrorCode::AuthenticationFailed)?;
        if proposal.name()?.map(|s| s.0) != member.name || proposal.os()?.map(|s| s.0) != member.os {
            return Err(v2::ErrorCode::StateConflict.into());
        }
    }
    let patch = plan.geometry_patch()?;
    for screen in patch.appended_devices()?.0 {
        if !source.contains(screen_key(&screen)?) {
            return Err(v2::ErrorCode::AuthenticationFailed.into());
        }
    }
    if apply_geometry(&target, &patch, target_issuer)? != plan.expected_result_registers()?.0 {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    Ok(target)
}

/// `group_merge::verify_bundle` under the format-1 definitions. Full trust is
/// unchanged: like 0.3.x, embedded signatures are checked for format only.
pub fn verify_bundle(bundle: &dto::MergeTransferBundle, trusted_source: Anchor) -> Result<()> {
    bundle.encode(v2::limits::PAIR_OBJECT)?;
    let certificate = bundle.certificate()?;
    let intent = certificate.intent()?;
    let plan = bundle.plan()?;
    if group_merge::anchor(&intent.source()?)? != trusted_source
        || plan.intent()? != intent
        || object_digest(&plan, v2::limits::PAIR_OBJECT)? != certificate.plan_digest()?.0
    {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let source = Roster::verify(bundle.source_roster()?, trusted_source)?;
    let target = validate_plan(&source, &plan)?;
    let source_issuer = wire::read_device(&intent.source_issuer()?)?;
    let target_issuer = wire::read_device(&intent.target_issuer()?)?;
    if wire::read_device(&certificate.permit_coordinator()?)? != source_issuer.min(target_issuer) {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    if certificate.permit_decision()? == 0 {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    check_signature_format(&certificate.source_signature()?)?;
    check_signature_format(&certificate.target_signature()?)?;
    let result = State::verify(bundle.target_state()?, target.roster.anchor)?;
    if result.digest()? != certificate.result_digest()?.0
        || !target
            .roster
            .accepts_exact_adds(&result.roster, &plan.unsigned_adds()?.0)
    {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let mut expected = target.wire.raw().clone();
    expected.insert(3, result.roster.wire.write());
    expected.insert(
        2,
        RawValue::Array(
            plan.expected_result_registers()?
                .0
                .iter()
                .map(WireValue::write)
                .collect(),
        ),
    );
    if &expected != result.wire.raw() {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    // Roster envelope extensions are target-owned and must not change at commit.
    let (before, after) = (target.roster.wire.raw(), result.roster.wire.raw());
    if before.iter().any(|(k, v)| *k != 3 && after.get(k) != Some(v))
        || after.keys().any(|k| *k != 3 && !before.contains_key(k))
    {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_fixture {
    //! A committed merge built exactly as 0.3.x built it (format-1 records,
    //! roster root id, register digests, SHA-256 certificate digests).
    use super::*;
    use crate::identity::Identity;
    use oink_foundation::{
        GroupId,
        session::{Boot, PairId},
    };

    fn signature(identity: &Identity, label: &[u8], body: &[u8]) -> RawValue {
        let mut input = label.to_vec();
        input.extend_from_slice(body);
        RawValue::Map(
            [
                (1, RawValue::from(dto::ED25519)),
                (2, RawValue::Bytes(identity.sign(&input).to_vec())),
            ]
            .into(),
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
        name: &str,
        now: u64,
    ) -> RawMap {
        let mut raw: RawMap = [
            (1, kind.write()),
            (2, RawValue::Bytes(group.0.to_vec())),
            (3, wire::device(subject).write()),
            (4, epoch.write()),
            (5, wire::device(issuer).write()),
            (7, RawValue::from(name)),
            (8, RawValue::from("test")),
            (9, now.write()),
        ]
        .into();
        if let Some(reference) = issuer_ref {
            raw.insert(6, RawValue::Bytes(reference.to_vec()));
        }
        raw
    }
    fn signed(identity: &Identity, body: RawMap) -> dto::SignedRecord {
        let bytes = RawValue::Map(body.clone()).encode(v2::limits::CONFIG_STATE).unwrap();
        let signature = signature(identity, b"oink/v1/roster/signature\0", &bytes);
        dto::SignedRecord::from_raw([(1, RawValue::Map(body)), (2, signature)].into()).unwrap()
    }
    fn roster(group: GroupId, root: [u8; 32], records: Vec<dto::SignedRecord>) -> dto::Roster {
        let mut records = records
            .into_iter()
            .map(|r| (record_id(&r).unwrap(), r.write()))
            .collect::<Vec<_>>();
        records.sort_by_key(|(id, _)| *id);
        dto::Roster::from_raw(
            [
                (1, RawValue::Bytes(group.0.to_vec())),
                (2, RawValue::Bytes(root.to_vec())),
                (3, RawValue::Array(records.into_iter().map(|(_, r)| r).collect())),
            ]
            .into(),
        )
        .unwrap()
    }
    fn state(group: GroupId, roster: &dto::Roster, registers: Vec<RawValue>) -> dto::GroupState {
        dto::GroupState::from_raw(
            [
                (1, RawValue::Bytes(group.0.to_vec())),
                (2, RawValue::Array(registers)),
                (3, roster.write()),
            ]
            .into(),
        )
        .unwrap()
    }
    fn layout_register(writer: DeviceId) -> RawValue {
        let layout: RawMap = [
            (1, RawValue::Array(Vec::new())),
            (2, dto::LayoutSource::Default.write()),
            (4, RawValue::Map(RawMap::new())),
        ]
        .into();
        let version: RawMap = [(1, 1u64.write()), (3, wire::device(writer).write())].into();
        let mut register: RawMap = [
            (1, 2u32.write()),
            (3, RawValue::Map(version)),
            (4, RawValue::Map(layout)),
        ]
        .into();
        register.insert(6, RawValue::Bytes(register_digest(&register).unwrap().to_vec()));
        RawValue::Map(register)
    }
    fn hash(value: &impl WireDto, limit: usize) -> RawValue {
        RawValue::Bytes(object_digest(value, limit).unwrap().to_vec())
    }
    #[allow(missing_debug_implementations, reason = "test fixture with a signing identity")]
    pub struct Fixture {
        pub source: Identity,
        pub target: Identity,
        pub bundle: dto::MergeTransferBundle,
        /// A's 0.3.x group state before the merge (format 1).
        pub source_state: dto::GroupState,
        /// B's 0.3.x group state before the merge (format 1).
        pub target_base: dto::GroupState,
        /// B's committed journal: the target row, receipt delivered.
        pub journal: Vec<u8>,
        pub pair: PairId,
    }
    /// Source group {A} merges into target group {B}; B commits.
    pub fn fixture() -> Fixture {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let (source_group, target_group) = (GroupId([0xa1; 16]), GroupId([0xb2; 16]));
        let a_create = signed(
            &a,
            body(
                dto::RecordKind::Create,
                source_group,
                a.id(),
                0,
                a.id(),
                None,
                "alpha-source",
                1,
            ),
        );
        let a_root = record_id(&a_create).unwrap();
        let b_create = signed(
            &b,
            body(
                dto::RecordKind::Create,
                target_group,
                b.id(),
                0,
                b.id(),
                None,
                "bravo-target",
                1,
            ),
        );
        let b_root = record_id(&b_create).unwrap();
        let source_roster = roster(source_group, a_root, vec![a_create]);
        let source_state = state(source_group, &source_roster, vec![layout_register(a.id())]);
        let target_roster = roster(target_group, b_root, vec![b_create.clone()]);
        let target_registers = vec![layout_register(b.id())];
        let target_base = state(target_group, &target_roster, target_registers.clone());
        let group_ref = |group: GroupId, root: [u8; 32]| {
            RawValue::Map(
                [
                    (1, RawValue::Bytes(group.0.to_vec())),
                    (2, RawValue::Bytes(root.to_vec())),
                ]
                .into(),
            )
        };
        let pair = PairId {
            initiator_boot: Boot {
                device_id: a.id(),
                nonce: [3; 16],
            },
            random: [4; 16],
        };
        let import: RawMap = [
            (1, wire::device(a.id()).write()),
            (2, RawValue::Bytes(a_root.to_vec())),
            (3, 1u64.write()),
        ]
        .into();
        let intent = dto::MergeIntent::from_raw(
            [
                (1, RawValue::Bytes(vec![5; 32])),
                (2, wire::pair(pair).write()),
                (3, group_ref(source_group, a_root)),
                (4, group_ref(target_group, b_root)),
                (5, wire::device(a.id()).write()),
                (6, RawValue::Bytes(a_root.to_vec())),
                (7, wire::device(b.id()).write()),
                (8, RawValue::Bytes(b_root.to_vec())),
                (9, hash(&source_roster, v2::limits::CONFIG_STATE)),
                (10, hash(&source_state, v2::limits::CONFIG_STATE)),
                (11, hash(&target_base, v2::limits::CONFIG_STATE)),
                (12, RawValue::Array(vec![RawValue::Map(import)])),
                (13, 1u64.write()),
            ]
            .into(),
        )
        .unwrap();
        let add = body(
            dto::RecordKind::Add,
            target_group,
            a.id(),
            1,
            b.id(),
            Some(b_root),
            "alpha-source",
            2,
        );
        let patch: RawMap = [
            (1, RawValue::Array(Vec::new())),
            (3, RawValue::Map([(1, 0.0f64.write()), (2, 0.0f64.write())].into())),
            (4, dto::LayoutSource::Manual.write()),
        ]
        .into();
        let plan = dto::MergePlan::from_raw(
            [
                (1, intent.write()),
                (3, RawValue::Map(patch)),
                (4, RawValue::Array(vec![RawValue::Map(add.clone())])),
                (5, target_base.write()),
                (6, RawValue::Array(target_registers.clone())),
            ]
            .into(),
        )
        .unwrap();
        let result_roster = roster(target_group, b_root, vec![b_create, signed(&b, add)]);
        let result = state(target_group, &result_roster, target_registers);
        let mut authorize = object_digest(&intent, v2::limits::PAIR_OBJECT).unwrap().to_vec();
        authorize.extend_from_slice(&object_digest(&plan, v2::limits::PAIR_OBJECT).unwrap());
        authorize.extend_from_slice(&1u64.to_le_bytes());
        let mut certificate: RawMap = [
            (1, intent.write()),
            (2, hash(&plan, v2::limits::PAIR_OBJECT)),
            (3, hash(&result, v2::limits::CONFIG_STATE)),
            (4, signature(&a, v2::labels::MERGE_AUTHORIZE, &authorize)),
            (6, wire::device(a.id().min(b.id())).write()),
            (7, 1u64.write()),
        ]
        .into();
        let committed = v2::cbor::encode_map(&certificate, v2::limits::PAIR_OBJECT).unwrap();
        certificate.insert(5, signature(&b, v2::labels::MERGE_COMMITTED, &committed));
        let bundle = dto::MergeTransferBundle::from_raw(
            [
                (1, RawValue::Map(certificate)),
                (2, source_roster.write()),
                (3, result.write()),
                (4, plan.write()),
            ]
            .into(),
        )
        .unwrap();
        // Status 1 (committed), role 1 (target), receipt delivered.
        let row: RawMap = [
            (1, wire::pair(pair).write()),
            (2, wire::device(a.id()).write()),
            (3, wire::device(b.id()).write()),
            (4, plan.write()),
            (5, 1u64.write()),
            (6, 1u8.write()),
            (7, 1u8.write()),
            (8, bundle.write()),
            (9, true.write()),
            (13, false.write()),
        ]
        .into();
        let journal: RawMap = [
            (1, 1u8.write()),
            (2, wire::device(b.id()).write()),
            (3, RawValue::Array(vec![RawValue::Map(row)])),
            (4, RawValue::Array(Vec::new())),
            (5, 1u64.write()),
            (6, 0u64.write()),
            (7, RawValue::Array(Vec::new())),
            (8, RawValue::Array(Vec::new())),
        ]
        .into();
        Fixture {
            source: a,
            target: b,
            bundle,
            source_state,
            target_base,
            journal: v2::cbor::encode_map(&journal, usize::MAX).unwrap(),
            pair,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_fixture::fixture, *};
    use crate::pair_journal::Journal;

    fn positions(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
        haystack
            .windows(needle.len())
            .enumerate()
            .filter(|(_, w)| *w == needle)
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn format1_committed_merge_is_verified_under_its_own_definitions() {
        let fixture = fixture();
        let intent = fixture.bundle.certificate().unwrap().intent().unwrap();
        let source = group_merge::anchor(&intent.source().unwrap()).unwrap();
        assert!(is_format1(&fixture.bundle).unwrap());
        verify_bundle(&fixture.bundle, source).unwrap();
        // The v2 definitions cannot reproduce a 0.3.x certificate.
        assert_eq!(
            group_merge::verify_bundle(&fixture.bundle, source).unwrap_err().code,
            v2::ErrorCode::AuthenticationFailed
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::pair_journal::FILE);
        std::fs::write(&path, &fixture.journal).unwrap();
        let journal = Journal::load(&path, fixture.target.id()).unwrap();
        assert!(journal.pending_projection().unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), fixture.journal);
    }

    #[test]
    fn format1_committed_merge_with_one_changed_byte_is_rejected() {
        let fixture = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::pair_journal::FILE);
        let certificate = fixture.bundle.certificate().unwrap();
        let intent = certificate.intent().unwrap();
        // Member names occur in the source roster, the planned Add, the target
        // base and the committed result; the digests in the certificate.
        let needles: [&[u8]; 6] = [
            b"alpha-source",
            b"bravo-target",
            &certificate.plan_digest().unwrap().0,
            &certificate.result_digest().unwrap().0,
            &intent.source_roster_digest().unwrap().0,
            &intent.target_base_digest().unwrap().0,
        ];
        let mut targets = Vec::new();
        for needle in needles {
            let found = positions(&fixture.journal, needle);
            assert!(!found.is_empty());
            targets.extend(found);
        }
        for at in targets {
            let mut bytes = fixture.journal.clone();
            bytes[at] ^= 0x01;
            std::fs::write(&path, &bytes).unwrap();
            assert!(
                Journal::load(&path, fixture.target.id()).is_err(),
                "a change at byte {at} was accepted"
            );
        }
    }
}
