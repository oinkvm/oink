//! The pointer base computer (protocol §14.3): a retained group register
//! holding one DeviceId. A phone's pointer units are logical points on that
//! computer's primary screen. Written only when the register is empty or its
//! device has left the group; then the member with the smallest DeviceId that
//! has desktop screens in the layout register is chosen. A new member never changes
//! it, a join keeps the target group's register (`group_merge` copies the
//! target's registers), and concurrent edits within a group converge by the
//! register's `(counter, writer)` order. An offline or hidden base is used as is.
use crate::{group_state::State, wire_identity as wire};
use oink_foundation::DeviceId;
use oink_protocol::v2::{
    self, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};

/// 0x1302 is DeviceOrder (shipped in 0.3.6) and 0x1303 was DeviceOrder in a
/// packaged 0.4.0 in-house build, so the pointer base uses the next free kind.
pub const REGISTER_KIND: u32 = 0x1304;

impl State {
    fn base_register(&self) -> v2::Result<Option<dto::Register>> {
        Ok(self
            .wire()
            .registers()?
            .0
            .into_iter()
            .find(|r| r.kind() == Ok(REGISTER_KIND)))
    }

    /// The recorded base computer, if any (it may have left the group).
    pub fn pointer_base(&self) -> v2::Result<Option<DeviceId>> {
        let Some(register) = self.base_register()? else {
            return Ok(None);
        };
        if register.deleted()? {
            return Ok(None);
        }
        let value = dto::PointerBase::from_raw(register.value()?)?;
        Ok(Some(wire::read_device(&value.device()?)?))
    }

    /// Members with a desktop screen (Android now has layout geometry too).
    fn screen_members(&self) -> v2::Result<Vec<DeviceId>> {
        let layout = dto::LayoutValue::from_raw(self.layout()?.value()?)?;
        let mut devices = Vec::new();
        for group in layout.devices()?.0 {
            let device = wire::read_device(&group.device()?)?;
            // The Android coordinate-space key is a screen identity, independent
            // of stale roster OS metadata. A phone never defines PC pointer units.
            let desktop = group.screens()?.0.iter().any(|s| {
                s.id()
                    .and_then(|id| id.display())
                    .is_ok_and(|key| key.0 != oink_foundation::display::ANDROID_INPUT_DISPLAY)
            });
            if desktop && self.roster().contains(device) {
                devices.push(device);
            }
        }
        devices.sort();
        Ok(devices)
    }

    /// Fill an empty register, or replace a base that left the group.
    /// `None` when nothing has to be written.
    pub fn reconcile_pointer_base(&self, writer: DeviceId) -> v2::Result<Option<Self>> {
        let current = self.pointer_base()?;
        if current.is_some_and(|base| self.roster().contains(base)) {
            return Ok(None);
        }
        let Some(base) = self.screen_members()?.first().copied() else {
            return Ok(None);
        };
        let value: v2::RawMap = [(1, wire::device(base).write())].into();
        let register = match self.base_register()? {
            Some(current) => retained::edit_register(&current, &current.version()?, wire::device(writer), |raw| {
                *raw = value.clone();
                Ok(())
            })?,
            None => {
                let version =
                    dto::ConfigVersion::from_raw([(1, 1u64.write()), (3, wire::device(writer).write())].into())?;
                dto::Register::from_raw(
                    [
                        (1, REGISTER_KIND.write()),
                        (3, version.write()),
                        (4, RawValue::Map(value)),
                    ]
                    .into(),
                )?
            }
        };
        let mut registers = self
            .wire()
            .registers()?
            .0
            .into_iter()
            .filter(|r| r.kind() != Ok(REGISTER_KIND))
            .collect::<Vec<_>>();
        registers.push(register);
        registers.sort_by_key(|r| r.kind().expect("validated kind"));
        let mut raw = self.wire().raw().clone();
        raw.insert(2, RawValue::Array(registers.iter().map(WireValue::write).collect()));
        Self::verify(dto::GroupState::from_raw(raw)?, self.roster().anchor()).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use crate::{group_state::State, identity::Identity, wire_identity as wire};
    use oink_foundation::DeviceId;
    use oink_protocol::v2::{
        self, RawValue, dto,
        schema::{WireDto, WireValue},
    };

    fn map<const N: usize>(fields: [(u32, RawValue); N]) -> v2::RawMap {
        fields.into()
    }

    /// Replace the layout with one screen for each of `devices`.
    fn with_screens(state: &State, writer: DeviceId, devices: &[DeviceId]) -> State {
        with_named_screens(
            state,
            writer,
            &devices.iter().map(|id| (*id, "main")).collect::<Vec<_>>(),
        )
    }
    fn with_named_screens(state: &State, writer: DeviceId, devices: &[(DeviceId, &str)]) -> State {
        let groups: Vec<RawValue> = devices
            .iter()
            .map(|(device, key)| {
                let id = map([(1, wire::device(*device).write()), (2, RawValue::Text((*key).into()))]);
                let rect = map([
                    (1, 0f64.write()),
                    (2, 0f64.write()),
                    (3, 1000f64.write()),
                    (4, 800f64.write()),
                ]);
                let screen = dto::ScreenPlacement::from_raw(map([
                    (1, RawValue::Map(id)),
                    (2, RawValue::Text("main".into())),
                    (3, RawValue::Map(rect)),
                    (5, dto::SizeSource::Reported.write()),
                    (9, true.write()),
                    (10, false.write()),
                ]))
                .unwrap();
                dto::DevicePlacement::from_raw(map([
                    (1, wire::device(*device).write()),
                    (2, RawValue::Map(map([(1, 0f64.write()), (2, 0f64.write())]))),
                    (3, 1f64.write()),
                    (4, RawValue::Array(vec![screen.write()])),
                ]))
                .unwrap()
                .write()
            })
            .collect();
        let current = state.layout().unwrap();
        let layout = v2::layout::edit_register(&current, &current.version().unwrap(), wire::device(writer), |value| {
            value.insert(1, RawValue::Array(groups));
            Ok(())
        })
        .unwrap();
        let registers: Vec<RawValue> = state
            .wire()
            .registers()
            .unwrap()
            .0
            .into_iter()
            .map(|r| if r.kind() == Ok(2) { layout.clone() } else { r })
            .map(|r| r.write())
            .collect();
        let mut raw = state.wire().raw().clone();
        raw.insert(2, RawValue::Array(registers));
        State::verify(dto::GroupState::from_raw(raw).unwrap(), state.roster().anchor()).unwrap()
    }

    fn add(state: &State, owner: &Identity, seed: u8) -> (State, DeviceId) {
        let device = Identity::from_seed([seed; 32]).id();
        let roster = state.roster().add(owner, device, "peer", "test", seed as u64).unwrap();
        (state.with_roster(&roster).unwrap(), device)
    }

    #[test]
    fn base_is_the_smallest_member_with_screens_and_stays_until_it_leaves() {
        let owner = Identity::from_seed([5; 32]);
        let state = State::create(&owner, "owner", "test", 1).unwrap();
        // No screens in the layout yet: nothing to choose, nothing written.
        assert!(state.reconcile_pointer_base(owner.id()).unwrap().is_none());
        assert_eq!(state.pointer_base().unwrap(), None);
        let (state, phone) = add(&state, &owner, 1);
        // A desktop whose id is smaller than the owner's, so it becomes the base.
        let seed = (6..=200u8)
            .find(|seed| Identity::from_seed([*seed; 32]).id() < owner.id())
            .unwrap();
        let (state, desk) = add(&state, &owner, seed);
        // The phone has no screens; of the two desktops the smaller id wins.
        let state = with_screens(&state, owner.id(), &[owner.id(), desk]);
        let state = state.reconcile_pointer_base(owner.id()).unwrap().unwrap();
        assert_eq!(state.pointer_base().unwrap(), Some(desk));
        assert_ne!(state.pointer_base().unwrap(), Some(phone));
        // A newly added member with screens and a smaller id changes nothing.
        let newcomer_seed = (201..=255u8)
            .find(|seed| Identity::from_seed([*seed; 32]).id() < desk)
            .unwrap_or(2);
        let (state, newcomer) = add(&state, &owner, newcomer_seed);
        let state = with_screens(&state, owner.id(), &[owner.id(), desk, newcomer]);
        assert!(state.reconcile_pointer_base(owner.id()).unwrap().is_none());
        let again = state;
        assert_eq!(again.pointer_base().unwrap(), Some(desk));
        // The base leaves: the smallest remaining member with screens replaces it.
        let roster = again.roster().remove(&owner, desk, None, 99).unwrap();
        let left = again
            .with_roster(&roster)
            .unwrap()
            .reconcile_pointer_base(owner.id())
            .unwrap()
            .unwrap();
        assert_eq!(left.pointer_base().unwrap(), Some(owner.id().min(newcomer)));
    }
    #[test]
    fn controllable_phone_in_layout_is_never_the_pointer_base() {
        let owner = Identity::from_seed([5; 32]);
        let state = State::create(&owner, "owner", "test", 1).unwrap();
        let seed = (6..=200u8)
            .find(|seed| Identity::from_seed([*seed; 32]).id() < owner.id())
            .unwrap();
        let (state, phone) = add(&state, &owner, seed);
        let only_phone = with_named_screens(
            &state,
            owner.id(),
            &[(phone, oink_foundation::display::ANDROID_INPUT_DISPLAY)],
        );
        assert!(only_phone.reconcile_pointer_base(owner.id()).unwrap().is_none());
        let both = with_named_screens(
            &state,
            owner.id(),
            &[
                (owner.id(), "main"),
                (phone, oink_foundation::display::ANDROID_INPUT_DISPLAY),
            ],
        );
        assert_eq!(
            both.reconcile_pointer_base(owner.id())
                .unwrap()
                .unwrap()
                .pointer_base()
                .unwrap(),
            Some(owner.id())
        );
    }
}
