//! Group-wide device list order: one optional retained register whose whole
//! value is last-writer-wins by (counter, writer). A move rewrites the list in
//! one edit, so a concurrent move or join allocation never interleaves with it.
use crate::{group_state::State, wire_identity as wire};
use oink_foundation::DeviceId;
use oink_protocol::v2::{
    self, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::collections::{BTreeMap, BTreeSet};

/// Optional retained register kind 0x1302. Public 0.3.6 shipped it with the
/// same id and value shape, so upgraded data and groups keep their order.
pub const REGISTER_KIND: u32 = 0x1302;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Directly above this device: its order − 1. When that value is taken,
    /// every other device at or below it moves down by one.
    Above(DeviceId),
    /// After this device, the previous end of the list: its order + 1.
    After(DeviceId),
}

/// Lists sort by (order, device id); devices without an entry go last.
pub fn sort_key(order: &BTreeMap<DeviceId, i64>, device: DeviceId) -> (bool, i64, DeviceId) {
    let value = order.get(&device).copied();
    (value.is_none(), value.unwrap_or_default(), device)
}

/// Apply one move. Returns false when the order is already as requested.
pub fn place(order: &mut BTreeMap<DeviceId, i64>, device: DeviceId, placement: Placement) -> v2::Result<bool> {
    let (Placement::Above(anchor) | Placement::After(anchor)) = placement;
    if anchor == device || !order.contains_key(&device) {
        return Err(v2::ErrorCode::StateConflict.into());
    }
    let base = *order.get(&anchor).ok_or(v2::ErrorCode::StateConflict)?;
    let target = match placement {
        Placement::Above(_) => base.checked_sub(1),
        Placement::After(_) => base.checked_add(1),
    }
    .ok_or(v2::ErrorCode::LimitExceeded)?;
    let taken = matches!(placement, Placement::Above(_)) && order.iter().any(|(d, o)| *d != device && *o == target);
    if !taken && order[&device] == target {
        return Ok(false);
    }
    if taken {
        for (_, value) in order.iter_mut().filter(|(d, o)| **d != device && **o <= target) {
            *value = value.checked_sub(1).ok_or(v2::ErrorCode::LimitExceeded)?;
        }
    }
    order.insert(device, target);
    Ok(true)
}

impl State {
    /// Only current membership incarnations appear; offline members keep their entry.
    pub fn device_order(&self) -> v2::Result<BTreeMap<DeviceId, i64>> {
        let Some(register) = self.register(REGISTER_KIND)? else {
            return Ok(BTreeMap::new());
        };
        if register.deleted()? {
            return Err(v2::ErrorCode::StateConflict.into());
        }
        let mut result = BTreeMap::new();
        let mut devices = BTreeSet::new();
        for entry in dto::DeviceOrder::from_raw(register.value()?)?.entries()?.0 {
            let device = wire::read_device(&entry.device()?)?;
            let order = entry.order()?;
            if !devices.insert(device) {
                return Err(v2::ErrorCode::StateConflict.into());
            }
            if self
                .roster()
                .member(device)
                .is_some_and(|m| entry.membership_epoch() == Ok(m.epoch))
            {
                result.insert(device, order);
            }
        }
        Ok(result)
    }
    /// Keep existing entries and append each new incarnation at max + 1, in
    /// signed join order (issued_ms, then DeviceId). Departed devices drop out.
    pub fn reconcile_device_order(&self, writer: DeviceId) -> v2::Result<Self> {
        let mut order = self.device_order()?;
        let mut joining = self
            .roster()
            .members()
            .filter(|m| !order.contains_key(&m.device))
            .collect::<Vec<_>>();
        joining.sort_by_key(|m| (m.issued_ms, m.device));
        for member in joining {
            let next = order
                .values()
                .max()
                .map_or(Ok(0), |max| max.checked_add(1).ok_or(v2::ErrorCode::LimitExceeded))?;
            order.insert(member.device, next);
        }
        self.write_device_order(writer, &order)
    }
    pub fn move_device(&self, writer: DeviceId, device: DeviceId, placement: Placement) -> v2::Result<Self> {
        let mut order = self.device_order()?;
        if !place(&mut order, device, placement)? {
            return Ok(self.clone());
        }
        self.write_device_order(writer, &order)
    }
    fn write_device_order(&self, writer: DeviceId, order: &BTreeMap<DeviceId, i64>) -> v2::Result<Self> {
        let retained = self
            .register(REGISTER_KIND)?
            .map(|r| dto::DeviceOrder::from_raw(r.value()?)?.entries())
            .transpose()?
            .map(|v| v.0)
            .unwrap_or_default();
        let entries = order
            .iter()
            .map(|(device, value)| {
                let epoch = self.roster().member(*device).ok_or(v2::ErrorCode::StateConflict)?.epoch;
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
                raw.insert(3, value.write());
                dto::DeviceOrderEntry::from_raw(raw).map(|e| e.write())
            })
            .collect::<v2::Result<Vec<_>>>()?;
        self.put_register(REGISTER_KIND, writer, [(1, RawValue::Array(entries))].into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use oink_protocol::v2::retained;

    fn id(seed: u8) -> DeviceId {
        Identity::from_seed([seed; 32]).id()
    }
    fn map(entries: &[(u8, i64)]) -> BTreeMap<DeviceId, i64> {
        entries.iter().map(|(s, o)| (id(*s), *o)).collect()
    }
    fn sorted(order: &BTreeMap<DeviceId, i64>) -> Vec<DeviceId> {
        let mut devices = order.keys().copied().collect::<Vec<_>>();
        devices.sort_by_key(|d| sort_key(order, *d));
        devices
    }
    fn add(state: &State, owner: &Identity, seed: u8, time: u64) -> State {
        state
            .with_roster(&state.roster().add(owner, id(seed), "peer", "test", time).unwrap())
            .unwrap()
            .reconcile_device_order(owner.id())
            .unwrap()
    }
    fn group(seeds: &[u8]) -> (Identity, State) {
        let a = Identity::from_seed([1; 32]);
        let mut state = State::create(&a, "a", "test", 1).unwrap();
        for (index, seed) in seeds.iter().enumerate() {
            state = add(&state, &a, *seed, 2 + index as u64);
        }
        (a, state)
    }

    #[test]
    fn move_above_uses_the_free_slot_below_the_anchor() {
        let mut order = map(&[(1, 0), (2, 2), (3, 5)]);
        assert!(place(&mut order, id(1), Placement::Above(id(3))).unwrap());
        assert_eq!(order, map(&[(1, 4), (2, 2), (3, 5)]));
        assert_eq!(sorted(&order), [id(2), id(1), id(3)]);
        assert!(!place(&mut order, id(1), Placement::Above(id(3))).unwrap());
    }

    #[test]
    fn move_above_a_taken_slot_shifts_every_lower_device_and_keeps_their_relative_order() {
        // 4 is offline in the viewer's list; it still shifts with the others.
        let mut order = map(&[(1, 0), (2, 1), (3, 2), (4, -3), (5, 3)]);
        assert!(place(&mut order, id(5), Placement::Above(id(3))).unwrap());
        assert_eq!(order, map(&[(1, -1), (2, 0), (3, 2), (4, -4), (5, 1)]));
        assert_eq!(sorted(&order), [id(4), id(1), id(2), id(5), id(3)]);
        // A already at B − 1 but tied with another device still separates them.
        let mut tied = map(&[(1, 1), (2, 1), (3, 2)]);
        assert!(place(&mut tied, id(2), Placement::Above(id(3))).unwrap());
        assert_eq!(tied, map(&[(1, 0), (2, 1), (3, 2)]));
    }

    #[test]
    fn move_to_end_is_the_previous_last_plus_one_and_ties_sort_by_device_id() {
        let mut order = map(&[(1, 0), (2, 1), (3, 2), (4, 3)]);
        assert!(place(&mut order, id(1), Placement::After(id(3))).unwrap());
        assert_eq!(order[&id(1)], 3);
        let tie = [id(1).min(id(4)), id(1).max(id(4))];
        assert_eq!(sorted(&order), [id(2), id(3), tie[0], tie[1]]);
        assert!(place(&mut order, id(1), Placement::After(id(1))).is_err());
        assert!(place(&mut order, id(9), Placement::After(id(1))).is_err());
        assert!(place(&mut order, id(1), Placement::Above(id(9))).is_err());
        assert_eq!(sorted(&map(&[(7, 1), (6, 1)])), [id(6).min(id(7)), id(6).max(id(7))]);
        let mut partial = map(&[(7, 1)]);
        partial.remove(&id(7));
        assert_eq!(sort_key(&partial, id(7)), (true, 0, id(7)));
    }

    #[test]
    fn new_members_join_at_the_end_and_departures_leave_the_others_unchanged() {
        let (a, state) = group(&[9, 3, 7]);
        assert_eq!(state.device_order().unwrap(), map(&[(1, 0), (9, 1), (3, 2), (7, 3)]));
        let moved = state.move_device(a.id(), id(9), Placement::After(id(7))).unwrap();
        assert_eq!(moved.device_order().unwrap()[&id(9)], 4);
        let joined = add(&moved, &a, 5, 10);
        assert_eq!(joined.device_order().unwrap()[&id(5)], 5);
        let removed = joined
            .with_roster(&joined.roster().remove(&a, id(3), None, 11).unwrap())
            .unwrap()
            .reconcile_device_order(a.id())
            .unwrap();
        assert_eq!(removed.device_order().unwrap(), map(&[(1, 0), (7, 3), (9, 4), (5, 5)]));
        // A rejoin is a fresh incarnation, placed at the end again.
        let rejoined = add(&removed, &a, 3, 12);
        assert_eq!(rejoined.device_order().unwrap()[&id(3)], 6);
        let restarted = State::verify(rejoined.wire().clone(), rejoined.roster().anchor()).unwrap();
        assert_eq!(restarted.device_order().unwrap(), rejoined.device_order().unwrap());
        assert_eq!(
            restarted.reconcile_device_order(a.id()).unwrap().digest().unwrap(),
            rejoined.digest().unwrap()
        );
    }

    #[test]
    fn concurrent_moves_are_whole_list_last_writer_wins_in_any_delivery_order() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let base = group(&[2, 3]).1;
        let left = base.move_device(a.id(), id(3), Placement::Above(id(1))).unwrap();
        let right = base.move_device(b.id(), id(1), Placement::After(id(3))).unwrap();
        let l = left.merge(right.wire(), b.id(), a.id()).unwrap();
        let r = right.merge(left.wire(), a.id(), b.id()).unwrap();
        assert_eq!(l.digest().unwrap(), r.digest().unwrap());
        // Equal counters: the frozen non-layout rule picks the larger writer key.
        let writer = if wire::device(a.id()).ordering_key().unwrap() > wire::device(b.id()).ordering_key().unwrap() {
            &left
        } else {
            &right
        };
        assert_eq!(l.device_order().unwrap(), writer.device_order().unwrap());
        // A later edit (higher counter) wins over both.
        let later = r.move_device(a.id(), id(2), Placement::Above(id(1))).unwrap();
        for stale in [&left, &right, &base] {
            assert_eq!(
                stale
                    .merge(later.wire(), a.id(), b.id())
                    .unwrap()
                    .device_order()
                    .unwrap(),
                later.device_order().unwrap()
            );
            assert_eq!(
                later.merge(stale.wire(), b.id(), a.id()).unwrap().digest().unwrap(),
                later.digest().unwrap()
            );
        }
    }

    #[test]
    fn baseline_peer_relays_the_unknown_register_and_a_state_without_it_gets_one() {
        let (a, state) = group(&[2]);
        let b = Identity::from_seed([2; 32]);
        let moved = state.move_device(a.id(), id(1), Placement::After(id(2))).unwrap();
        // A 0.3.x peer decodes GroupState generically: no schema for this kind
        // (it is not reserved), whole-register digest checks, and opaque merge.
        let register = moved.register(REGISTER_KIND).unwrap().unwrap();
        retained::verify_register(&register).unwrap();
        assert!(!v2::registry::RESERVED_REGISTERS.contains(&REGISTER_KIND));
        let relayed = dto::GroupState::from_raw(
            v2::cbor::decode_map(
                &moved.wire().encode(v2::limits::CONFIG_STATE).unwrap(),
                v2::limits::CONFIG_STATE,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(&relayed, moved.wire());
        // A state authored before this register existed merges without conflict
        // and keeps the newer order; reconciling it alone creates the default.
        let mut legacy = state.wire().raw().clone();
        let registers = state
            .wire()
            .registers()
            .unwrap()
            .0
            .into_iter()
            .filter(|r| r.kind() != Ok(REGISTER_KIND))
            .map(|r| r.write())
            .collect();
        legacy.insert(2, RawValue::Array(registers));
        let legacy = State::verify(dto::GroupState::from_raw(legacy).unwrap(), state.roster().anchor()).unwrap();
        assert!(legacy.device_order().unwrap().is_empty());
        assert_eq!(
            legacy.merge(&relayed, a.id(), b.id()).unwrap().device_order().unwrap(),
            moved.device_order().unwrap()
        );
        assert_eq!(
            moved.merge(legacy.wire(), b.id(), a.id()).unwrap().digest().unwrap(),
            moved.digest().unwrap()
        );
        assert_eq!(
            legacy.reconcile_device_order(b.id()).unwrap().device_order().unwrap(),
            map(&[(1, 0), (2, 1)])
        );
        // Unknown entry and value keys written by a future peer survive edits.
        let future = moved
            .put_register(REGISTER_KIND, b.id(), [(90, RawValue::from("future"))].into())
            .unwrap();
        let edited = future.move_device(a.id(), id(2), Placement::After(id(1))).unwrap();
        let value = edited.register(REGISTER_KIND).unwrap().unwrap().value().unwrap();
        assert_eq!(value.get(&90), Some(&RawValue::from("future")));
        assert_eq!(
            edited.reconcile_device_order(a.id()).unwrap().digest().unwrap(),
            edited.digest().unwrap()
        );
    }
}
