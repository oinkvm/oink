//! Retained shared-state validation and reconciliation after roster admission.
use crate::{
    identity::Identity,
    roster::{Anchor, Roster},
    wire_identity as wire,
};
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    self, RawMap, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub struct State {
    wire: dto::GroupState,
    roster: Roster,
}
impl State {
    pub fn create(identity: &Identity, name: &str, os: &str, now: u64) -> v1::Result<Self> {
        let group = GroupId(rand::random());
        let roster = Roster::create(identity, group, name, os, now)?;
        let layout = dto::LayoutValue::from_raw(
            [
                (1, RawValue::Array(Vec::new())),
                (2, dto::LayoutSource::Default.write()),
                (4, RawValue::Map(RawMap::new())),
            ]
            .into(),
        )?;
        let version =
            dto::ConfigVersion::from_raw([(1, 1u64.write()), (3, wire::device(identity.id()).write())].into())?;
        let mut register: RawMap = [(1, 2u32.write()), (3, version.write()), (4, layout.write())].into();
        register.insert(6, retained::register_digest(&register)?.write());
        Self::verify(
            dto::GroupState::from_raw(
                [
                    (1, RawValue::Bytes(group.0.to_vec())),
                    (2, RawValue::Array(vec![RawValue::Map(register)])),
                    (3, roster.wire().write()),
                ]
                .into(),
            )?,
            roster.anchor(),
        )
    }
    pub fn verify(state: dto::GroupState, anchor: Anchor) -> v1::Result<Self> {
        if state.group()?.0 != anchor.group.0 {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        state.encode(v1::limits::CONFIG_STATE)?;
        let roster = Roster::verify(state.roster()?, anchor)?;
        let mut layout = false;
        for register in state.registers()?.0 {
            retained::verify_register(&register)?;
            if register.kind()? == 2 {
                validate_layout(&dto::LayoutValue::from_raw(register.value()?)?)?;
                layout = true;
            }
        }
        if !layout {
            return Err(v1::ErrorCode::MalformedFrame.into());
        }
        Ok(Self { wire: state, roster })
    }
    pub fn wire(&self) -> &dto::GroupState {
        &self.wire
    }
    pub fn roster(&self) -> &Roster {
        &self.roster
    }
    pub fn digest(&self) -> v1::Result<[u8; 32]> {
        Ok(retained::sha256(&self.wire.encode(v1::limits::CONFIG_STATE)?).0)
    }
    pub fn layout(&self) -> v1::Result<dto::Register> {
        self.wire
            .registers()?
            .0
            .into_iter()
            .find(|r| r.kind() == Ok(2))
            .ok_or(v1::ErrorCode::MalformedFrame.into())
    }
    pub fn with_roster(&self, roster: &Roster) -> v1::Result<Self> {
        let mut raw = self.wire.raw().clone();
        raw.insert(3, roster.wire().write());
        Self::verify(dto::GroupState::from_raw(raw)?, self.roster.anchor())
    }
    pub fn merge(&self, incoming: &dto::GroupState, peer: DeviceId, local: DeviceId) -> v1::Result<Self> {
        let other = Self::verify(incoming.clone(), self.roster.anchor())?;
        let roster = self.roster.merge(other.roster.wire())?;
        if !roster.contains(peer) || !roster.contains(local) {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        self.merge_verified(incoming, other, roster)
    }
    /// An already admitted peer may deliver a signed removal of itself or this
    /// node. Retain that removal before the runtime revokes application authority.
    pub fn receive_update(&self, incoming: &dto::GroupState, peer: DeviceId, local: DeviceId) -> v1::Result<Self> {
        if !self.roster.contains(peer) || !self.roster.contains(local) {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        let other = Self::verify(incoming.clone(), self.roster.anchor())?;
        let roster = self.roster.merge(other.roster.wire())?;
        self.merge_verified(incoming, other, roster)
    }
    fn merge_verified(&self, incoming: &dto::GroupState, other: Self, roster: Roster) -> v1::Result<Self> {
        let mut registers = self
            .wire
            .registers()?
            .0
            .into_iter()
            .map(|r| Ok((r.kind()?, r)))
            .collect::<v1::Result<BTreeMap<_, _>>>()?;
        for incoming in other.wire.registers()?.0 {
            let kind = incoming.kind()?;
            let merged = if let Some(current) = registers.get(&kind) {
                retained::merge_register(current, &incoming)?
            } else {
                incoming
            };
            registers.insert(kind, merged);
        }
        let mut raw = retained::merge_extensions(self.wire.raw(), incoming.raw(), &[1, 2, 3])?;
        raw.insert(2, RawValue::Array(registers.values().map(WireValue::write).collect()));
        raw.insert(3, roster.wire().write());
        Self::verify(dto::GroupState::from_raw(raw)?, self.roster.anchor())
    }
}
pub fn validate_layout(layout: &dto::LayoutValue) -> v1::Result<()> {
    let mut devices = std::collections::BTreeSet::new();
    for group in layout.devices()?.0 {
        let device = wire::read_device(&group.device()?)?;
        if !devices.insert(device) || group.scale()? <= 0.0 {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        group.origin()?;
        let mut ids = std::collections::BTreeSet::new();
        for screen in group.screens()?.0 {
            let id = screen.id()?;
            if wire::read_device(&id.device()?)? != device || !ids.insert(id.display()?.0) {
                return Err(v1::ErrorCode::StateConflict.into());
            }
            validate_rect(&screen.local()?)?;
        }
    }
    Ok(())
}
pub fn validate_rect(rect: &dto::Rect) -> v1::Result<()> {
    let (x, y, w, h) = (rect.x()?, rect.y()?, rect.w()?, rect.h()?);
    if w <= 0.0 || h <= 0.0 || !(x + w).is_finite() || !(y + h).is_finite() {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admitted_signed_removals_are_retained_before_authority_is_revoked() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let initial = State::create(&a, "source", "test", 1).unwrap();
        let state = initial
            .with_roster(&initial.roster().add(&a, b.id(), "peer", "test", 2).unwrap())
            .unwrap();
        for removed in [a.id(), b.id()] {
            let updated = state
                .with_roster(&state.roster().remove(&b, removed, None, 3).unwrap())
                .unwrap();
            assert!(state.merge(updated.wire(), b.id(), a.id()).is_err());
            let retained = state.receive_update(updated.wire(), b.id(), a.id()).unwrap();
            assert!(!retained.roster().contains(removed));
            assert!(retained.receive_update(state.wire(), b.id(), a.id()).is_err());
        }
    }
}
