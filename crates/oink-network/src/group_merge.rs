//! Directional signed handover. Plans are inert; only a durable transaction owner
//! may commit them, publish the resulting Adds, or activate the returned state.
use crate::{
    group_state::{self, State},
    identity::Identity,
    roster::{Anchor, Roster},
    wire_identity as wire,
};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};
use std::collections::{BTreeMap, BTreeSet};
type Result<T> = v1::Result<T>;
pub fn group_ref(anchor: Anchor) -> dto::GroupRef {
    dto::GroupRef::from_raw(
        [
            (1, RawValue::Bytes(anchor.group.0.to_vec())),
            (2, RawValue::Bytes(anchor.root.to_vec())),
        ]
        .into(),
    )
    .expect("anchor")
}
pub fn anchor(reference: &dto::GroupRef) -> Result<Anchor> {
    Ok(Anchor {
        group: GroupId(reference.group()?.0),
        root: reference.root()?.0,
    })
}
pub fn digest<T: WireDto>(value: &T) -> Result<[u8; 32]> {
    Ok(retained::sha256(&value.encode(v1::limits::PAIR_OBJECT)?).0)
}
pub fn seed(source: &State, target: Anchor, sequence: u64) -> Result<dto::MergeSeed> {
    if source.roster().anchor() == target {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    dto::MergeSeed::from_raw(
        [
            (1, RawValue::Bytes(rand::random::<[u8; 32]>().to_vec())),
            (2, group_ref(source.roster().anchor()).write()),
            (3, group_ref(target).write()),
            (4, sequence.write()),
            (5, RawValue::Bytes(source.digest()?.to_vec())),
        ]
        .into(),
    )
}
pub fn prepare(
    source: &State,
    target: &State,
    context: &dto::PairContext,
    seed: &dto::MergeSeed,
    now: u64,
) -> Result<dto::MergePlan> {
    let source_issuer = wire::read_boot(&context.initiator()?)?.device_id;
    let target_issuer = wire::read_boot(&context.responder()?)?.device_id;
    let source_anchor = source.roster().anchor();
    let target_anchor = target.roster().anchor();
    if source_anchor.group == target_anchor.group
        || source_anchor != anchor(&seed.source()?)?
        || target_anchor != anchor(&seed.target()?)?
        || context.source_group()?.0 != source_anchor.group.0
        || context.source_root_id()?.0 != source_anchor.root
        || context.target_group()?.0 != target_anchor.group.0
        || context.target_root_id()?.0 != target_anchor.root
        || seed.source_state_digest()?.0 != source.digest()?
    {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    let source_member = source
        .roster()
        .member(source_issuer)
        .ok_or(v1::ErrorCode::AuthenticationFailed)?;
    let target_member = target
        .roster()
        .member(target_issuer)
        .ok_or(v1::ErrorCode::AuthenticationFailed)?;
    let mut members = Vec::new();
    let mut proposals = Vec::new();
    let mut union = BTreeSet::new();
    union.extend(target.roster().members().map(|m| m.device));
    for member in source.roster().members() {
        union.insert(member.device);
        let epoch = if let Some(existing) = target.roster().member(member.device) {
            existing.epoch
        } else {
            let proposal = target.roster().propose_add(
                target_issuer,
                member.device,
                member.name.as_deref(),
                member.os.as_deref(),
                now,
            )?;
            let epoch = proposal.epoch()?;
            proposals.push(proposal);
            epoch
        };
        members.push(dto::MemberImport::from_raw(
            [
                (1, wire::device(member.device).write()),
                (2, RawValue::Bytes(member.add.to_vec())),
                (3, epoch.write()),
            ]
            .into(),
        )?);
    }
    if union.len() > crate::roster::MAX_MEMBERS
        || target.roster().wire().records()?.0.len() + proposals.len() > crate::roster::MAX_RECORDS
    {
        return Err(v1::ErrorCode::LimitExceeded.into());
    }
    let intent = dto::MergeIntent::from_raw(
        [
            (1, seed.merge_id()?.write()),
            (2, context.pair_id()?.write()),
            (3, seed.source()?.write()),
            (4, seed.target()?.write()),
            (5, wire::device(source_issuer).write()),
            (6, RawValue::Bytes(source_member.add.to_vec())),
            (7, wire::device(target_issuer).write()),
            (8, RawValue::Bytes(target_member.add.to_vec())),
            (9, RawValue::Bytes(source.roster().digest()?.to_vec())),
            (10, seed.source_state_digest()?.write()),
            (11, RawValue::Bytes(target.digest()?.to_vec())),
            (12, RawValue::Array(members.iter().map(WireValue::write).collect())),
            (13, seed.source_transition_sequence()?.write()),
        ]
        .into(),
    )?;
    let patch = geometry_patch(source, target)?;
    let registers = apply_geometry(target, &patch, target_issuer)?;
    let plan = dto::MergePlan::from_raw(
        [
            (1, intent.write()),
            (3, patch.write()),
            (4, RawValue::Array(proposals.iter().map(WireValue::write).collect())),
            (5, target.wire().write()),
            (6, RawValue::Array(registers.iter().map(WireValue::write).collect())),
        ]
        .into(),
    )?;
    plan.encode(v1::limits::PAIR_OBJECT)?;
    Ok(plan)
}
pub fn verify_plan(
    source: &State,
    context: &dto::PairContext,
    seed: &dto::MergeSeed,
    plan: &dto::MergePlan,
) -> Result<()> {
    let intent = plan.intent()?;
    let target = State::verify(plan.target_base()?, anchor(&intent.target()?)?)?;
    let proposed = plan.unsigned_adds()?.0;
    let time = proposed.first().map(|p| p.issued_ms()).transpose()?.unwrap_or(0);
    let expected = prepare(source, &target, context, seed, time)?;
    // The complete source snapshot is available to the authorizing source.
    // Every planned coordinate and retained register must match deterministic policy.
    if intent != expected.intent()?
        || plan.geometry_patch() != expected.geometry_patch()
        || plan.expected_result_registers() != expected.expected_result_registers()
    {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    let bodies = expected.unsigned_adds()?.0;
    if proposed.len() != bodies.len() {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    for (actual, expected) in proposed.iter().zip(bodies) {
        if (1..=9).any(|k| actual.raw().get(&k) != expected.raw().get(&k)) {
            return Err(v1::ErrorCode::StateConflict.into());
        }
    }
    validate_plan(&source.roster().clone(), plan)?;
    Ok(())
}
fn validate_plan(source: &Roster, plan: &dto::MergePlan) -> Result<State> {
    let intent = plan.intent()?;
    if source.anchor() != anchor(&intent.source()?)? || source.digest()? != intent.source_roster_digest()?.0 {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let source_issuer = wire::read_device(&intent.source_issuer()?)?;
    let target_issuer = wire::read_device(&intent.target_issuer()?)?;
    if wire::read_pair(&intent.pair_id()?)?.initiator_boot.device_id != source_issuer
        || intent.source()?.group()? == intent.target()?.group()?
    {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    if source.member(source_issuer).map(|m| m.add) != Some(intent.source_issuer_ref()?.0) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let target = State::verify(plan.target_base()?, anchor(&intent.target()?)?)?;
    if target.digest()? != intent.target_base_digest()?.0
        || target.roster().member(target_issuer).map(|m| m.add) != Some(intent.target_issuer_ref()?.0)
    {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let imports = intent.members()?.0;
    let actual = source.members().collect::<Vec<_>>();
    if imports.len() != actual.len() {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    let mut expected = BTreeMap::new();
    let mut union = target.roster().members().map(|m| m.device).collect::<BTreeSet<_>>();
    for (import, member) in imports.iter().zip(actual) {
        let device = wire::read_device(&import.device()?)?;
        let epoch = target
            .roster()
            .member(device)
            .map(|m| Ok(m.epoch))
            .unwrap_or_else(|| target.roster().next_epoch(device))?;
        if device != member.device || import.source_add()?.0 != member.add || import.target_epoch()? != epoch {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        union.insert(device);
        if !target.roster().contains(device) {
            expected.insert(device, epoch);
        }
    }
    if union.len() > crate::roster::MAX_MEMBERS {
        return Err(v1::ErrorCode::LimitExceeded.into());
    }
    let proposals = plan.unsigned_adds()?.0;
    if proposals.len() != expected.len()
        || proposals.len() + target.roster().wire().records()?.0.len() > crate::roster::MAX_RECORDS
    {
        return Err(v1::ErrorCode::LimitExceeded.into());
    }
    let mut seen = BTreeSet::new();
    for proposal in proposals {
        let subject = wire::read_device(&proposal.subject()?)?;
        if !seen.insert(subject)
            || expected.get(&subject) != Some(&proposal.epoch()?)
            || proposal.kind()? != dto::RecordKind::Add
            || proposal.group()?.0 != target.roster().anchor().group.0
            || wire::read_device(&proposal.issuer()?)? != target_issuer
            || proposal.issuer_ref()?.map(|r| r.0) != Some(intent.target_issuer_ref()?.0)
        {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        let member = source.member(subject).ok_or(v1::ErrorCode::AuthenticationFailed)?;
        if proposal.name()?.map(|s| s.0) != member.name || proposal.os()?.map(|s| s.0) != member.os {
            return Err(v1::ErrorCode::StateConflict.into());
        }
    }
    let patch = plan.geometry_patch()?;
    for screen in patch.appended_devices()?.0 {
        if !source.contains(screen_key(&screen)?) {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
    }
    if apply_geometry(&target, &plan.geometry_patch()?, target_issuer)? != plan.expected_result_registers()?.0 {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    Ok(target)
}
pub fn authorization(identity: &Identity, plan: &dto::MergePlan, decision: u64) -> Result<dto::Signature> {
    if identity.id() != wire::read_device(&plan.intent()?.source_issuer()?)? {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    signature(identity, &authorization_input(plan, decision)?)
}
fn authorization_input(plan: &dto::MergePlan, decision: u64) -> Result<Vec<u8>> {
    if decision == 0 {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    let mut input = v1::labels::MERGE_AUTHORIZE.to_vec();
    input.extend_from_slice(&digest(&plan.intent()?)?);
    input.extend_from_slice(&digest(plan)?);
    input.extend_from_slice(&decision.to_le_bytes());
    Ok(input)
}
pub fn commit(
    identity: &Identity,
    source: &Roster,
    current: &State,
    plan: &dto::MergePlan,
    source_signature: dto::Signature,
    decision: u64,
) -> Result<dto::MergeTransferBundle> {
    let target = validate_plan(source, plan)?;
    let intent = plan.intent()?;
    if current.digest()? != target.digest()? || identity.id() != wire::read_device(&intent.target_issuer()?)? {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    if decision == 0 {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    check_signature_format(&source_signature)?;
    let roster = target.roster().sign_adds(identity, &plan.unsigned_adds()?.0)?;
    let mut raw = target.wire().raw().clone();
    raw.insert(3, roster.wire().write());
    raw.insert(
        2,
        RawValue::Array(
            plan.expected_result_registers()?
                .0
                .iter()
                .map(WireValue::write)
                .collect(),
        ),
    );
    let result = State::verify(dto::GroupState::from_raw(raw)?, roster.anchor())?;
    let coordinator = identity.id().min(wire::read_device(&intent.source_issuer()?)?);
    let mut certificate: RawMap = [
        (1, intent.write()),
        (2, RawValue::Bytes(digest(plan)?.to_vec())),
        (3, RawValue::Bytes(result.digest()?.to_vec())),
        (4, source_signature.write()),
        (6, wire::device(coordinator).write()),
        (7, decision.write()),
    ]
    .into();
    let mut input = v1::labels::MERGE_COMMITTED.to_vec();
    input.extend(v1::cbor::encode_map(&certificate, v1::limits::PAIR_OBJECT)?);
    certificate.insert(5, signature(identity, &input)?.write());
    let bundle = dto::MergeTransferBundle::from_raw(
        [
            (1, RawValue::Map(certificate)),
            (2, source.wire().write()),
            (3, result.wire().write()),
            (4, plan.write()),
        ]
        .into(),
    )?;
    bundle.encode(v1::limits::PAIR_OBJECT)?;
    Ok(bundle)
}
/// Validates the exact target result against the signed plan. For automatic
/// adoption the caller additionally compares the source roster with local removal
/// knowledge and requires its own active import before installing this result.
pub fn verify_bundle(bundle: &dto::MergeTransferBundle, trusted_source: Anchor) -> Result<State> {
    bundle.encode(v1::limits::PAIR_OBJECT)?;
    let certificate = bundle.certificate()?;
    let intent = certificate.intent()?;
    let plan = bundle.plan()?;
    if anchor(&intent.source()?)? != trusted_source
        || plan.intent()? != intent
        || digest(&plan)? != certificate.plan_digest()?.0
    {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let source = Roster::verify(bundle.source_roster()?, trusted_source)?;
    let target = validate_plan(&source, &plan)?;
    let source_issuer = wire::read_device(&intent.source_issuer()?)?;
    let target_issuer = wire::read_device(&intent.target_issuer()?)?;
    if wire::read_device(&certificate.permit_coordinator()?)? != source_issuer.min(target_issuer) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    if certificate.permit_decision()? == 0 {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    check_signature_format(&certificate.source_signature()?)?;
    check_signature_format(&certificate.target_signature()?)?;
    let result = State::verify(bundle.target_state()?, target.roster().anchor())?;
    if result.digest()? != certificate.result_digest()?.0
        || !target
            .roster()
            .accepts_exact_adds(result.roster(), &plan.unsigned_adds()?.0)?
    {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let mut expected = target.wire().raw().clone();
    expected.insert(3, result.roster().wire().write());
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
    if &expected != result.wire().raw() {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    // Roster envelope extensions are target-owned and must not change at commit.
    if target
        .roster()
        .wire()
        .raw()
        .iter()
        .any(|(k, v)| *k != 3 && result.roster().wire().raw().get(k) != Some(v))
        || result
            .roster()
            .wire()
            .raw()
            .keys()
            .any(|k| *k != 3 && !target.roster().wire().raw().contains_key(k))
    {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    Ok(result)
}
pub fn adopt(bundle: &dto::MergeTransferBundle, current: &State, local: DeviceId) -> Result<State> {
    let result = verify_bundle(bundle, current.roster().anchor())?;
    let source = current.roster().merge(&bundle.source_roster()?)?;
    let imports = bundle.certificate()?.intent()?.members()?.0;
    if !source.contains(wire::read_device(&bundle.certificate()?.intent()?.source_issuer()?)?)
        || !source.contains(local)
        || !imports
            .iter()
            .any(|m| m.device().and_then(|d| wire::read_device(&d)) == Ok(local))
        || !result.roster().contains(local)
    {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    Ok(result)
}
fn signature(identity: &Identity, input: &[u8]) -> Result<dto::Signature> {
    dto::Signature::from_raw(
        [
            (1, RawValue::from(dto::ED25519)),
            (2, RawValue::Bytes(identity.sign(input).to_vec())),
        ]
        .into(),
    )
}
// Authentication belongs to TLS/member admission or the PAKE transaction.
// Continue emitting v1 signatures, and retain their structural validation.
fn check_signature_format(signature: &dto::Signature) -> Result<()> {
    if signature.algorithm()?.0 != dto::ED25519 {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    Ok(())
}
fn screens(state: &State) -> Result<Vec<dto::DevicePlacement>> {
    Ok(dto::LayoutValue::from_raw(state.layout()?.value()?)?.devices()?.0)
}
fn screen_key(group: &dto::DevicePlacement) -> Result<DeviceId> {
    wire::read_device(&group.device()?)
}
fn bounds(groups: &[dto::DevicePlacement]) -> Result<Option<(f64, f64, f64)>> {
    let mut bound: Option<(f64, f64, f64)> = None;
    for group in groups {
        let origin = group.origin()?;
        let k = group.scale()?;
        for screen in group.screens()?.0 {
            let r = screen.local()?;
            group_state::validate_rect(&r)?;
            let (x, y, right) = (
                origin.x()? + r.x()? * k,
                origin.y()? + r.y()? * k,
                origin.x()? + (r.x()? + r.w()?) * k,
            );
            bound = Some(bound.map_or((x, y, right), |(left, top, edge)| {
                (left.min(x), top.min(y), edge.max(right))
            }));
        }
    }
    Ok(bound)
}
fn geometry_patch(source: &State, target: &State) -> Result<dto::GeometryPatch> {
    for state in [source, target] {
        v1::layout::require_supported_geometry(&dto::LayoutValue::from_raw(state.layout()?.value()?)?)?;
    }
    let keep = screens(target)?;
    let keys = keep.iter().map(screen_key).collect::<Result<BTreeSet<_>>>()?;
    let mut arriving = screens(source)?
        .into_iter()
        .filter(|s| screen_key(s).is_ok_and(|key| source.roster().contains(key) && !keys.contains(&key)))
        .collect::<Vec<_>>();
    arriving.sort_by_key(|s| screen_key(s).expect("validated screen"));
    let (dx, dy) = if let Some((left, top, _)) = bounds(&arriving)? {
        if let Some((_, target_top, right)) = bounds(&keep)? {
            let snap = dto::LayoutValue::from_raw(target.layout()?.value()?)?
                .topology()?
                .portal_snap()?;
            (right + (10.0 / 686.0_f64).max(snap) - left, target_top - top)
        } else {
            (-left, -top)
        }
    } else {
        (0.0, 0.0)
    };
    if !dx.is_finite() || !dy.is_finite() {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    for group in &mut arriving {
        let mut raw = group.raw().clone();
        let mut origin = group.origin()?.into_raw();
        origin.insert(1, (group.origin()?.x()? + dx).write());
        origin.insert(2, (group.origin()?.y()? + dy).write());
        raw.insert(2, RawValue::Map(origin));
        *group = dto::DevicePlacement::from_raw(raw)?;
    }
    dto::GeometryPatch::from_raw(
        [
            (1, RawValue::Array(arriving.iter().map(WireValue::write).collect())),
            (3, RawValue::Map([(1, dx.write()), (2, dy.write())].into())),
            (4, dto::LayoutSource::Manual.write()),
        ]
        .into(),
    )
}
fn apply_geometry(target: &State, patch: &dto::GeometryPatch, writer: DeviceId) -> Result<Vec<dto::Register>> {
    if patch.raw().keys().any(|key| !matches!(key, 1 | 3 | 4)) || patch.source()? != dto::LayoutSource::Manual {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    let appended = patch.appended_devices()?.0;
    let mut registers = target.wire().registers()?.0;
    let current = target.layout()?;
    let mut layout = dto::LayoutValue::from_raw(current.value()?)?;
    v1::layout::require_supported_geometry(&layout)?;
    if appended.is_empty() {
        return Ok(registers);
    }
    let mut keep = layout.devices()?.0;
    let mut keys = keep.iter().map(screen_key).collect::<Result<BTreeSet<_>>>()?;
    let mut previous = None;
    for screen in &appended {
        let key = screen_key(screen)?;
        if !keys.insert(key) || previous.as_ref().is_some_and(|p| p >= &key) {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        previous = Some(key);
    }
    if keep.len() + appended.len() > 256 {
        return Err(v1::ErrorCode::LimitExceeded.into());
    }
    keep.extend(appended);
    let mut raw = layout.into_raw();
    raw.insert(1, RawValue::Array(keep.iter().map(WireValue::write).collect()));
    raw.insert(2, dto::LayoutSource::Manual.write());
    layout = dto::LayoutValue::from_raw(raw)?;
    v1::layout::require_supported_geometry(&layout)?;
    let edited = retained::edit_register(&current, &current.version()?, wire::device(writer), |value| {
        *value = layout.into_raw();
        Ok(())
    })?;
    *registers
        .iter_mut()
        .find(|r| r.kind() == Ok(2))
        .ok_or(v1::ErrorCode::MalformedFrame)? = edited;
    Ok(registers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::session::{Boot, PairId};
    fn context(source: &State, target: &State, a: &Identity, b: &Identity) -> dto::PairContext {
        let a = Boot {
            device_id: a.id(),
            nonce: [1; 16],
        };
        let b = Boot {
            device_id: b.id(),
            nonce: [2; 16],
        };
        dto::PairContext::from_raw(
            [
                (
                    1,
                    wire::pair(PairId {
                        initiator_boot: a,
                        random: [3; 16],
                    })
                    .write(),
                ),
                (2, wire::boot(a).write()),
                (3, wire::boot(b).write()),
                (4, RawValue::Bytes(source.roster().anchor().group.0.to_vec())),
                (5, RawValue::Bytes(target.roster().anchor().group.0.to_vec())),
                (6, 1u8.write()),
                (7, 1u8.write()),
                (8, RawValue::Bytes(target.roster().anchor().group.0.to_vec())),
                (9, RawValue::Bytes(vec![0; 32])),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(target.roster().anchor().root.to_vec())),
                (12, RawValue::Bytes(source.roster().anchor().root.to_vec())),
            ]
            .into(),
        )
        .unwrap()
    }
    #[test]
    fn full_trust_commit_and_reread_do_not_verify_embedded_signatures() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let source = State::create(&a, "a", "test", 1).unwrap();
        let target = State::create(&b, "b", "test", 1).unwrap();
        let ctx = context(&source, &target, &a, &b);
        let seed = seed(&source, target.roster().anchor(), 1).unwrap();
        let plan = prepare(&source, &target, &ctx, &seed, 4).unwrap();
        let auth = authorization(&a, &plan, 1).unwrap();
        assert!(crate::identity::verify_strict(
            &a.id(),
            &authorization_input(&plan, 1).unwrap(),
            &auth.value().unwrap().0
        ));
        crate::identity::STRICT_VERIFICATIONS.with(|count| count.set(0));
        let proof = commit(&b, source.roster(), &target, &plan, auth, 1).unwrap();
        verify_bundle(&proof, source.roster().anchor()).unwrap();
        adopt(&proof, &source, a.id()).unwrap();
        crate::identity::STRICT_VERIFICATIONS.with(|count| assert_eq!(count.get(), 0));
        // v1 signatures are still produced for older peers.
        let mut raw = proof.certificate().unwrap().into_raw();
        let signature = dto::Signature::from_raw(raw.remove(&5).unwrap().as_map().unwrap().clone()).unwrap();
        let mut input = v1::labels::MERGE_COMMITTED.to_vec();
        input.extend(v1::cbor::encode_map(&raw, v1::limits::PAIR_OBJECT).unwrap());
        assert!(crate::identity::verify_strict(
            &b.id(),
            &input,
            &signature.value().unwrap().0
        ));
    }
    fn screen(device: DeviceId, name: &str, x: f64, y: f64, w: f64, h: f64) -> RawValue {
        let rect = RawValue::Map([(1, x.write()), (2, y.write()), (3, w.write()), (4, h.write())].into());
        RawValue::Map(
            [
                (
                    1,
                    RawValue::Map(
                        [
                            (1, wire::device(device).write()),
                            (2, RawValue::from(name)),
                            (99, RawValue::Map([(1, RawValue::from("retained"))].into())),
                        ]
                        .into(),
                    ),
                ),
                (2, RawValue::from(name)),
                (3, rect.clone()),
                (5, dto::SizeSource::Reported.write()),
                (9, true.write()),
                (10, false.write()),
            ]
            .into(),
        )
    }
    fn add_screens(state: &State, identity: &Identity, screens: Vec<RawValue>) -> State {
        let current = state.layout().unwrap();
        let layout = retained::edit_register(
            &current,
            &current.version().unwrap(),
            wire::device(identity.id()),
            |raw| {
                let mut groups: BTreeMap<DeviceId, Vec<RawValue>> = BTreeMap::new();
                for screen in screens {
                    let item = dto::ScreenPlacement::from_raw(screen.as_map()?.clone())?;
                    groups
                        .entry(wire::read_device(&item.id()?.device()?)?)
                        .or_default()
                        .push(screen);
                }
                let groups = groups
                    .into_iter()
                    .map(|(device, screens)| {
                        RawValue::Map(
                            [
                                (1, wire::device(device).write()),
                                (2, RawValue::Map([(1, 0.0f64.write()), (2, 0.0f64.write())].into())),
                                (3, 1.0f64.write()),
                                (4, RawValue::Array(screens)),
                            ]
                            .into(),
                        )
                    })
                    .collect();
                raw.insert(1, RawValue::Array(groups));
                Ok(())
            },
        )
        .unwrap();
        let mut raw = state.wire().raw().clone();
        raw.insert(2, RawValue::Array(vec![layout.write()]));
        raw.insert(99, RawValue::Text("target-extension".into()));
        State::verify(dto::GroupState::from_raw(raw).unwrap(), state.roster().anchor()).unwrap()
    }
    #[test]
    fn directional_merge_imports_offline_members_and_target_tombstones_require_higher_epoch() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let c = Identity::from_seed([3; 32]);
        let source = State::create(&a, "a", "test", 1).unwrap();
        let source = source
            .with_roster(&source.roster().add(&a, c.id(), "offline", "test", 2).unwrap())
            .unwrap();
        let target = State::create(&b, "b", "test", 1).unwrap();
        let target = target
            .with_roster(
                &target
                    .roster()
                    .add(&b, c.id(), "c", "test", 2)
                    .unwrap()
                    .remove(&b, c.id(), None, 3)
                    .unwrap(),
            )
            .unwrap();
        let ctx = context(&source, &target, &a, &b);
        let seed = seed(&source, target.roster().anchor(), 1).unwrap();
        let plan = prepare(&source, &target, &ctx, &seed, 4).unwrap();
        verify_plan(&source, &ctx, &seed, &plan).unwrap();
        assert!(!target.roster().contains(a.id()));
        assert!(!target.roster().contains(c.id()));
        let proof = commit(
            &b,
            source.roster(),
            &target,
            &plan,
            authorization(&a, &plan, 1).unwrap(),
            1,
        )
        .unwrap();
        let result = verify_bundle(&proof, source.roster().anchor()).unwrap();
        assert_eq!(result.roster().anchor(), target.roster().anchor());
        assert!(result.roster().contains(a.id()));
        assert!(result.roster().contains(c.id()));
        assert_eq!(result.roster().member(c.id()).unwrap().epoch, 3);
        let reconciled = result.reconcile_pointer_slots(b.id()).unwrap();
        let adopted = adopt(&proof, &source, a.id())
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        assert_eq!(reconciled.pointer_slots().unwrap(), adopted.pointer_slots().unwrap());
        assert_eq!(reconciled.pointer_slots().unwrap()[&b.id()], Some(0));
        let slots = reconciled.pointer_slots().unwrap();
        assert_ne!(slots[&a.id()], slots[&b.id()]);
        assert_ne!(slots[&a.id()], slots[&c.id()]);

        assert_eq!(
            adopt(&proof, &source, c.id()).unwrap().digest().unwrap(),
            result.digest().unwrap()
        );
        let removed = source
            .with_roster(&source.roster().remove(&a, c.id(), None, 5).unwrap())
            .unwrap();
        assert!(adopt(&proof, &removed, c.id()).is_err());
    }
    #[test]
    fn target_geometry_and_unknown_fields_survive_and_source_block_moves_as_one() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let source = add_screens(
            &State::create(&a, "a", "test", 1).unwrap(),
            &a,
            vec![
                screen(a.id(), "one", -3.0, 2.0, 1.0, 1.0),
                screen(a.id(), "two", -1.0, 3.0, 1.0, 1.0),
            ],
        );
        let target = add_screens(
            &State::create(&b, "b", "test", 1).unwrap(),
            &b,
            vec![screen(b.id(), "target", 1.0, 1.0, 2.0, 1.0)],
        );
        let ctx = context(&source, &target, &a, &b);
        let seed = seed(&source, target.roster().anchor(), 1).unwrap();
        let plan = prepare(&source, &target, &ctx, &seed, 4).unwrap();
        verify_plan(&source, &ctx, &seed, &plan).unwrap();
        let proof = commit(
            &b,
            source.roster(),
            &target,
            &plan,
            authorization(&a, &plan, 1).unwrap(),
            1,
        )
        .unwrap();
        let result = verify_bundle(&proof, source.roster().anchor()).unwrap();
        let actual = screens(&result).unwrap();
        assert_eq!(actual[0], screens(&target).unwrap()[0]);
        let arrived = actual[1].screens().unwrap().0;
        assert_eq!(
            arrived[1].local().unwrap().x().unwrap() - arrived[0].local().unwrap().x().unwrap(),
            2.0
        );
        assert_eq!(
            arrived[1].local().unwrap().y().unwrap() - arrived[0].local().unwrap().y().unwrap(),
            1.0
        );
        assert!(bounds(&[actual[1].clone()]).unwrap().unwrap().0 > 3.0);
        assert_eq!(
            arrived[0].id().unwrap().raw().get(&99),
            screens(&source).unwrap()[0].screens().unwrap().0[0]
                .id()
                .unwrap()
                .raw()
                .get(&99)
        );
        assert_eq!(result.wire().raw().get(&99), target.wire().raw().get(&99));
    }
    #[test]
    fn unsupported_geometry_is_retained_but_cannot_be_prepared_or_applied() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let target = State::create(&b, "b", "test", 1).unwrap();
        let mut future = screen(a.id(), "future", 0.0, 0.0, 1.0, 1.0);
        let RawValue::Map(raw) = &mut future else {
            unreachable!()
        };
        raw.insert(99, RawValue::from("unknown geometry"));
        let source = add_screens(&State::create(&a, "a", "test", 1).unwrap(), &a, vec![future.clone()]);
        let before = source.wire().encode(v1::limits::CONFIG_STATE).unwrap();
        assert_eq!(
            geometry_patch(&source, &target).unwrap_err().code,
            v1::ErrorCode::UnsupportedValue
        );
        let patch = dto::GeometryPatch::from_raw(
            [
                (
                    1,
                    RawValue::Array(screens(&source).unwrap().iter().map(WireValue::write).collect()),
                ),
                (
                    3,
                    dto::Point::from_raw([(1, 0.0f64.write()), (2, 0.0f64.write())].into())
                        .unwrap()
                        .write(),
                ),
            ]
            .into(),
        )
        .unwrap();
        assert_eq!(
            apply_geometry(&target, &patch, b.id()).unwrap_err().code,
            v1::ErrorCode::UnsupportedValue
        );
        let empty = dto::GeometryPatch::from_raw(
            [(1, RawValue::Array(vec![])), (3, patch.translation().unwrap().write())].into(),
        )
        .unwrap();
        assert_eq!(
            apply_geometry(&source, &empty, a.id()).unwrap_err().code,
            v1::ErrorCode::UnsupportedValue
        );
        assert_eq!(source.wire().encode(v1::limits::CONFIG_STATE).unwrap(), before);
    }
    #[test]
    fn wrong_signer_zero_permit_changed_base_and_extra_result_are_rejected() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let x = Identity::from_seed([9; 32]);
        let source = State::create(&a, "a", "test", 1).unwrap();
        let target = State::create(&b, "b", "test", 1).unwrap();
        let ctx = context(&source, &target, &a, &b);
        let seed = seed(&source, target.roster().anchor(), 1).unwrap();
        let plan = prepare(&source, &target, &ctx, &seed, 4).unwrap();
        let sig = authorization(&a, &plan, 1).unwrap();
        assert!(authorization(&x, &plan, 1).is_err());
        assert!(commit(&b, source.roster(), &target, &plan, sig.clone(), 0).is_err());
        let changed = target
            .with_roster(&target.roster().add(&b, x.id(), "x", "test", 5).unwrap())
            .unwrap();
        assert!(commit(&b, source.roster(), &changed, &plan, sig.clone(), 1).is_err());
        let proof = commit(&b, source.roster(), &target, &plan, sig, 1).unwrap();
        let mut raw = proof.raw().clone();
        let mut state = proof.target_state().unwrap().into_raw();
        state.insert(77, true.write());
        raw.insert(3, RawValue::Map(state));
        assert!(
            verify_bundle(
                &dto::MergeTransferBundle::from_raw(raw).unwrap(),
                source.roster().anchor()
            )
            .is_err()
        );
    }
}
