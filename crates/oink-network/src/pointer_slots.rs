use crate::{group_state::State, wire_identity as wire};
use oink_foundation::DeviceId;
use oink_protocol::v1::{
    self, RawValue, dto, retained,
    schema::{WireDto, WireValue},
};
use std::collections::{BTreeMap, BTreeSet};

/// Optional retained register. Baseline peers preserve it opaquely in ConfigState.
pub const REGISTER_KIND: u32 = 0x1301;

impl State {
    fn slot_register(&self) -> v1::Result<Option<dto::Register>> {
        Ok(self
            .wire()
            .registers()?
            .0
            .into_iter()
            .find(|r| r.kind() == Ok(REGISTER_KIND)))
    }
    /// Only current membership incarnations appear; offline members retain slots.
    pub fn pointer_slots(&self) -> v1::Result<BTreeMap<DeviceId, Option<u8>>> {
        let Some(register) = self.slot_register()? else {
            return Ok(BTreeMap::new());
        };
        if register.deleted()? {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        let value = dto::PointerShortcutSlots::from_raw(register.value()?)?;
        let mut result = BTreeMap::new();
        let mut used = BTreeSet::new();
        let mut devices = BTreeSet::new();
        for entry in value.entries()?.0 {
            let device = wire::read_device(&entry.device()?)?;
            let slot = entry.slot()?;
            if slot.is_some_and(|s| s >= 10 || !used.insert(s)) || !devices.insert(device) {
                return Err(v1::ErrorCode::StateConflict.into());
            }
            if self
                .roster()
                .member(device)
                .is_some_and(|m| entry.membership_epoch() == Ok(m.epoch))
            {
                result.insert(device, slot);
            }
        }
        Ok(result)
    }
    /// Preserve assigned digits and OFF entries. Allocate only new incarnations,
    /// smallest free digit first; a removed device frees its digit immediately.
    /// Batch/legacy bootstrap joins tie-break by signed issued_ms then DeviceId.
    pub fn reconcile_pointer_slots(&self, writer: DeviceId) -> v1::Result<Self> {
        let mut slots = self.pointer_slots()?;
        let mut used = slots.values().flatten().copied().collect::<BTreeSet<_>>();
        let mut joining = self
            .roster()
            .members()
            .filter(|m| !slots.contains_key(&m.device))
            .collect::<Vec<_>>();
        joining.sort_by_key(|m| (m.issued_ms, m.device));
        for member in joining {
            let slot = (0..10).find(|s| !used.contains(s));
            if let Some(slot) = slot {
                used.insert(slot);
            }
            slots.insert(member.device, slot);
        }
        let current = self.slot_register()?;
        let retained = current
            .as_ref()
            .map(|r| dto::PointerShortcutSlots::from_raw(r.value()?)?.entries())
            .transpose()?
            .map(|v| v.0)
            .unwrap_or_default();
        let entries = slots
            .iter()
            .map(|(device, slot)| {
                let epoch = self.roster().member(*device).expect("current member").epoch;
                let mut raw = retained
                    .iter()
                    .find(|e| {
                        e.device().is_ok_and(|d| wire::read_device(&d) == Ok(*device))
                            && e.membership_epoch() == Ok(epoch)
                    })
                    .map(|e| e.raw().clone())
                    .unwrap_or_default();
                raw.insert(1, wire::device(*device).write());
                raw.insert(2, epoch.write());
                raw.insert(3, slot.write());
                dto::PointerShortcutSlot::from_raw(raw).map(|e| e.write())
            })
            .collect::<v1::Result<Vec<_>>>()?;
        let value: v1::RawMap = [(1, RawValue::Array(entries))].into();
        if current.as_ref().is_some_and(|r| r.value() == Ok(value.clone())) {
            return Ok(self.clone());
        }
        let register = if let Some(current) = current {
            retained::edit_register(&current, &current.version()?, wire::device(writer), |raw| {
                raw.insert(1, value[&1].clone());
                Ok(())
            })?
        } else {
            let version = dto::ConfigVersion::from_raw([(1, 1u64.write()), (3, wire::device(writer).write())].into())?;
            let mut raw = [
                (1, REGISTER_KIND.write()),
                (3, version.write()),
                (4, RawValue::Map(value)),
            ]
            .into();
            let digest = retained::register_digest(&raw)?;
            raw.insert(6, digest.write());
            dto::Register::from_raw(raw)?
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
        Self::verify(dto::GroupState::from_raw(raw)?, self.roster().anchor())
    }
}

#[cfg(test)]
mod tests {
    use crate::{group_state::State, identity::Identity};
    use oink_protocol::v1::schema::WireDto;

    fn add(state: &State, owner: &Identity, seed: u8, time: u64) -> State {
        let device = Identity::from_seed([seed; 32]).id();
        state
            .with_roster(&state.roster().add(owner, device, "peer", "test", time).unwrap())
            .unwrap()
            .reconcile_pointer_slots(owner.id())
            .unwrap()
    }
    #[test]
    fn defaults_follow_joins_not_device_sort_name_clock_or_local_view() {
        let a = Identity::from_seed([1; 32]);
        let mut state = State::create(&a, "local", "test", 100)
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        assert_eq!(state.pointer_slots().unwrap()[&a.id()], Some(0));
        for (index, seed) in [9, 3, 7].into_iter().enumerate() {
            state = add(&state, &a, seed, 1); // clock moves backwards
            assert_eq!(
                state.pointer_slots().unwrap()[&Identity::from_seed([seed; 32]).id()],
                Some(index as u8 + 1)
            );
        }
        let restarted = State::verify(state.wire().clone(), state.roster().anchor()).unwrap();
        assert_eq!(state.pointer_slots().unwrap(), restarted.pointer_slots().unwrap());
        assert_eq!(
            state.wire().encode(1 << 20).unwrap(),
            restarted
                .reconcile_pointer_slots(a.id())
                .unwrap()
                .wire()
                .encode(1 << 20)
                .unwrap()
        );
    }
    #[test]
    fn eleventh_is_off_and_leave_frees_only_departing_slot() {
        let a = Identity::from_seed([1; 32]);
        let mut state = State::create(&a, "a", "test", 1)
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        for seed in 2..=11 {
            state = add(&state, &a, seed, seed as u64);
        }
        let off = Identity::from_seed([11; 32]).id();
        assert_eq!(state.pointer_slots().unwrap()[&off], None);
        let b = Identity::from_seed([2; 32]).id();
        let removed = state.roster().remove(&a, b, None, 99).unwrap();
        state = state
            .with_roster(&removed)
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        assert!(!state.pointer_slots().unwrap().contains_key(&b));
        assert_eq!(state.pointer_slots().unwrap()[&off], None);
        state = add(&state, &a, 12, 100);
        assert_eq!(
            state.pointer_slots().unwrap()[&Identity::from_seed([12; 32]).id()],
            Some(1)
        );
        state = add(&state, &a, 2, 101);
        assert_eq!(state.pointer_slots().unwrap()[&b], None);
    }
    #[test]
    fn rejoin_is_a_fresh_allocation_and_does_not_renumber_survivors() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]).id();
        let c = Identity::from_seed([3; 32]).id();
        let base = State::create(&a, "a", "test", 1).unwrap();
        let base = add(&add(&base, &a, 2, 2), &a, 3, 3);
        let removed = base
            .with_roster(&base.roster().remove(&a, b, None, 4).unwrap())
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        assert_eq!(removed.pointer_slots().unwrap()[&c], Some(2));
        let rejoined = add(&removed, &a, 2, 1);
        assert_eq!(rejoined.pointer_slots().unwrap()[&b], Some(1));
        assert_eq!(rejoined.pointer_slots().unwrap()[&c], Some(2));
    }
    #[test]
    fn batch_bootstrap_order_is_independent_of_roster_delivery_order() {
        let a = Identity::from_seed([1; 32]);
        let base = State::create(&a, "a", "test", 1).unwrap();
        let b = Identity::from_seed([2; 32]).id();
        let c = Identity::from_seed([3; 32]).id();
        let ab = base.roster().add(&a, b, "b", "test", 2).unwrap();
        let ac = base.roster().add(&a, c, "c", "test", 2).unwrap();
        let l = base
            .with_roster(&ab.merge(ac.wire()).unwrap())
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        let r = base
            .with_roster(&ac.merge(ab.wire()).unwrap())
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        assert_eq!(l.pointer_slots().unwrap(), r.pointer_slots().unwrap());
        assert_eq!(l.digest().unwrap(), r.digest().unwrap());
    }
    #[test]
    fn concurrent_roster_merges_converge_independent_of_delivery_order() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let base = add(
            &State::create(&a, "a", "test", 1)
                .unwrap()
                .reconcile_pointer_slots(a.id())
                .unwrap(),
            &a,
            2,
            2,
        );
        let left = add(&base, &a, 3, 3);
        let right = add(&base, &b, 4, 3);
        let l = left
            .merge(right.wire(), a.id(), a.id())
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        let r = right
            .merge(left.wire(), a.id(), b.id())
            .unwrap()
            .reconcile_pointer_slots(b.id())
            .unwrap();
        assert_eq!(l.pointer_slots().unwrap(), r.pointer_slots().unwrap());
        let lm = l
            .merge(r.wire(), b.id(), a.id())
            .unwrap()
            .reconcile_pointer_slots(a.id())
            .unwrap();
        let rm = r
            .merge(l.wire(), a.id(), b.id())
            .unwrap()
            .reconcile_pointer_slots(b.id())
            .unwrap();
        assert_eq!(lm.digest().unwrap(), rm.digest().unwrap());
        assert_eq!(
            lm.digest().unwrap(),
            lm.reconcile_pointer_slots(a.id()).unwrap().digest().unwrap()
        );
    }
}
