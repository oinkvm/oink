//! Size-dependent profile composition with synthetic identities and no I/O.
use super::{model::*, types::*};
use crate::api::{DeviceKey, GroupKey, Placement, ScreenKey};
use oink_foundation::{DeviceId, GroupId, Point, Size};
use std::collections::{BTreeMap, BTreeSet};

fn device(n: u8) -> DeviceKey {
    DeviceKey(DeviceId([n; 32]))
}
fn screen(n: u8) -> ScreenKey {
    ScreenKey {
        device: device(n),
        display: "main".into(),
    }
}
fn roster(first: u8, count: u8) -> BTreeSet<DeviceKey> {
    (first..first + count).map(device).collect()
}
fn layout(first: u8, count: u8) -> ArrangementSnapshot {
    ArrangementSnapshot {
        placements: (first..first + count)
            .map(|n| Placement {
                device: device(n),
                origin: Point::new(f64::from(n - first) * 300.0, 0.0),
                scale: 1.0,
            })
            .collect(),
        disabled: (first..first + count)
            .map(|n| Disabled {
                screen: screen(n),
                disabled: false,
            })
            .collect(),
        screen_shared: (first..first + count)
            .map(|n| DeviceSharing {
                device: device(n),
                shared: true,
            })
            .collect(),
        native: (first..first + count)
            .map(|n| NativeState {
                device: device(n),
                token: String::new(),
                primary: screen(n),
                screens: vec![NativeScreen {
                    screen: screen(n),
                    origin: Point::default(),
                    size: Size::new(100.0, 100.0),
                }],
            })
            .collect(),
        topology: Default::default(),
        active_profile_id: String::new(),
    }
}
fn state(s: &ArrangementSnapshot) -> DeviceState {
    DeviceState {
        online_devices: BTreeSet::new(),
        connected_screens: s
            .native
            .iter()
            .map(|n| {
                (
                    n.device,
                    Some(n.screens.iter().map(|s| s.screen.display.clone()).collect()),
                )
            })
            .collect(),
    }
}
fn catalog(group: u8, first: u8, count: u8, profiles: usize) -> Catalog {
    let mut s = layout(first, count);
    let mut c = Catalog::initial(GroupKey(GroupId([group; 16])), device(first), &s, state(&s), 1).unwrap();
    for n in 1..profiles {
        s.placements[0].origin.y += 20.0;
        c = c
            .save(
                device(first),
                &s,
                state(&s),
                Some(&format!("Desk {group}-{n}")),
                n as u64 + 1,
            )
            .unwrap();
    }
    c
}
fn merge(
    target: &Catalog,
    source: &Catalog,
    source_members: BTreeSet<DeviceKey>,
    target_members: BTreeSet<DeviceKey>,
    live: &ArrangementSnapshot,
    id: &str,
    writer: DeviceKey,
) -> Result<Catalog> {
    target.merge_group_with_context(
        source,
        id,
        writer,
        &super::composition::MergeContext {
            source_members: &source_members,
            target_members: &target_members,
            current_members: &source_members
                .union(&target_members)
                .copied()
                .chain(live.placements.iter().map(|p| p.device))
                .collect(),
            live,
        },
    )
}
fn live(a: &Catalog, b: &Catalog) -> ArrangementSnapshot {
    let mut s = a.active().content.clone();
    let other = &b.active().content;
    s.placements.extend(other.placements.clone());
    s.native.extend(other.native.clone());
    s.disabled.extend(other.disabled.clone());
    s.screen_shared.extend(other.screen_shared.clone());
    s
}
fn assert_own_content(before: &Profile, after: &Profile, own: &BTreeSet<DeviceKey>) {
    for p in before.content.placements.iter().filter(|p| own.contains(&p.device)) {
        assert!(after.content.placements.contains(p));
    }
    for n in before.content.native.iter().filter(|n| own.contains(&n.device)) {
        assert!(after.content.native.contains(n));
    }
    for d in before
        .content
        .disabled
        .iter()
        .filter(|d| own.contains(&d.screen.device))
    {
        assert!(after.content.disabled.contains(d));
    }
    assert_eq!(after.content.topology, before.content.topology);
    assert_eq!(after.last_used, before.last_used);
}
#[test]
fn singleton_pairing_keeps_only_acceptors_profiles_and_appends_initiator_to_each() {
    for profiles in [2, 16] {
        for reverse in [false, true] {
            for custom in [false, true] {
                let a = catalog(4, 1, 1, profiles);
                let b = catalog(5, 10, 1, 3);
                let (mut target, source, tm, sm) = if reverse {
                    (b, a, roster(10, 1), roster(1, 1))
                } else {
                    (a, b, roster(1, 1), roster(10, 1))
                };
                if !custom {
                    target.active_profile_id = target.default_profile().unwrap().id.clone();
                }
                let id = "a0".repeat(32);
                let result = merge(
                    &target,
                    &source,
                    sm.clone(),
                    tm.clone(),
                    &live(&target, &source),
                    &id,
                    *tm.first().unwrap(),
                )
                .expect("two singleton catalogs must complete without adding profile slots");
                assert_eq!(result.profiles.len(), target.profiles.len());
                assert_eq!(result.active_profile_id, target.active_profile_id);
                assert_eq!(
                    result.default_profile().unwrap().id,
                    target.default_profile().unwrap().id
                );
                assert!(
                    source.profiles.iter().all(|p| result.profile(&p.id).is_err()),
                    "all initiating profiles must be discarded"
                );
                for old in &target.profiles {
                    let p = result.profile(&old.id).unwrap();
                    assert_own_content(old, p, &tm);
                    assert_eq!(p.content.placements.len(), 2);
                    let arriving = *sm.first().unwrap();
                    assert!(p.content.placements.iter().any(|p| p.device == arriving));
                    assert!(
                        p.content
                            .disabled
                            .iter()
                            .any(|d| d.screen.device == arriving && !d.disabled)
                    );
                }
                assert!(result.completed_joins.contains_key(&id));
            }
        }
    }
}
#[test]
fn singleton_multi_keeps_all_sixteen_multi_profiles_in_both_pairing_directions() {
    for reverse in [false, true] {
        for custom in [false, true] {
            let mut multi = catalog(4, 1, 3, 16);
            multi.extensions = oink_protocol::v1::cbor::encode_map(
                &[(1000, oink_protocol::v1::RawValue::Bool(true))].into(),
                MAX_BYTES,
            )
            .unwrap();
            if !custom {
                multi.active_profile_id = multi.default_profile().unwrap().id.clone();
            }
            let singleton = catalog(5, 10, 1, 3);
            let expected = multi.clone();
            let current = live(&multi, &singleton);
            let (target, source, tm, sm) = if reverse {
                (&singleton, &multi, roster(10, 1), roster(1, 3))
            } else {
                (&multi, &singleton, roster(1, 3), roster(10, 1))
            };
            let result = merge(
                target,
                source,
                sm,
                tm,
                &current,
                &"a1".repeat(32),
                device(if reverse { 10 } else { 1 }),
            )
            .unwrap();
            assert_eq!(result.profiles.len(), 16);
            assert_eq!(result.extensions, expected.extensions);
            assert_eq!(
                result.active_profile_id, expected.active_profile_id,
                "the multi-device selection must survive either direction"
            );
            assert_eq!(
                result.default_profile().unwrap().id,
                expected.default_profile().unwrap().id
            );
            for old in &expected.profiles {
                let p = result.profile(&old.id).expect("multi-device profile ID lost");
                assert_own_content(old, p, &roster(1, 3));
                assert!(
                    p.content.placements.iter().any(|p| p.device == device(10)),
                    "singleton missing from a retained profile"
                );
                assert!(p.content.disabled.iter().any(|d| d.screen == screen(10) && !d.disabled));
            }
        }
    }
}
#[test]
fn every_group_profile_appends_the_other_active_cluster_and_visibility() {
    for (tc, sc) in [(1, 1), (2, 3), (3, 2)] {
        for hidden in 0..4 {
            let mut target = catalog(4, 1, tc, 2);
            let mut source = catalog(5, 10, sc, 2);
            target
                .profiles
                .iter_mut()
                .for_each(|p| p.content.disabled[0].disabled = hidden & 1 != 0);
            source
                .profiles
                .iter_mut()
                .for_each(|p| p.content.disabled[0].disabled = hidden & 2 != 0);
            let current = live(&target, &source);
            let result = merge(
                &target,
                &source,
                roster(10, sc),
                roster(1, tc),
                &current,
                &"a2".repeat(32),
                device(1),
            )
            .unwrap();
            for p in &result.profiles {
                assert_eq!(
                    p.content.placements.len(),
                    usize::from(tc + sc),
                    "every merged profile must include both original rosters"
                );
                assert!(
                    p.content
                        .disabled
                        .iter()
                        .any(|d| d.screen == screen(1) && d.disabled == (hidden & 1 != 0))
                );
                assert!(
                    p.content
                        .disabled
                        .iter()
                        .any(|d| d.screen == screen(10) && d.disabled == (hidden & 2 != 0))
                );
                let other = if source.profiles.iter().any(|old| old.id == p.id) {
                    &target
                } else {
                    &source
                };
                if other.active().content.placements.len() > 1 {
                    let a = &other.active().content.placements[0];
                    let b = &other.active().content.placements[1];
                    let pa = p.content.placements.iter().find(|p| p.device == a.device).unwrap();
                    let pb = p.content.placements.iter().find(|p| p.device == b.device).unwrap();
                    assert_eq!(pa.origin.x - pb.origin.x, a.origin.x - b.origin.x);
                    assert_eq!(pa.origin.y - pb.origin.y, a.origin.y - b.origin.y);
                }
            }
        }
    }
}
#[test]
fn empty_singleton_default_uses_live_facts_and_stale_foreign_hidden_bits_do_not_win() {
    let mut target = catalog(4, 1, 3, 2);
    let mut source = catalog(5, 10, 1, 1);
    let current = live(&target, &source);
    source.profiles[0].content = layout(10, 0);
    source.profiles[0].match_state.connected_screens = BTreeMap::new();
    // A profile can retain a former member; its stale hidden bit is not this
    // joining singleton's original visibility.
    for p in &mut target.profiles {
        let s = layout(10, 1);
        p.content.placements.extend(s.placements);
        p.content.native.extend(s.native);
        p.content.disabled.push(Disabled {
            screen: screen(10),
            disabled: true,
        });
    }
    let result = merge(
        &target,
        &source,
        roster(10, 1),
        roster(1, 3),
        &current,
        &"a3".repeat(32),
        device(1),
    )
    .unwrap();
    assert_eq!(
        result.default_profile().unwrap().id,
        target.default_profile().unwrap().id
    );
    for p in &result.profiles {
        assert!(
            p.content.disabled.iter().any(|d| d.screen == screen(10) && !d.disabled),
            "stale foreign hidden state must be replaced by the arriving owner's visible state"
        );
    }
}
#[test]
fn capacity_overflow_evicts_lru_but_protects_default_and_resolved_active_and_completes() {
    for active_default in [false, true] {
        let mut target = catalog(4, 1, 2, 16);
        let mut source = catalog(5, 10, 2, 16);
        // Force both protected entries below every ordinary profile in recency.
        let active_id = if active_default {
            target.default_profile().unwrap().id.clone()
        } else {
            target.active_profile_id.clone()
        };
        target.active_profile_id = active_id.clone();
        target
            .profiles
            .iter_mut()
            .find(|p| p.id == active_id)
            .unwrap()
            .last_used
            .logical_ms = 0;
        let default_id = source.default_profile().unwrap().id.clone();
        source
            .profiles
            .iter_mut()
            .find(|p| p.id == default_id)
            .unwrap()
            .last_used
            .logical_ms = 0;
        let resolved_active = if active_default { default_id.clone() } else { active_id };
        let mut ordinary = target
            .profiles
            .iter()
            .chain(&source.profiles)
            .filter(|p| p.default_group.is_none() && p.id != resolved_active)
            .collect::<Vec<_>>();
        ordinary.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
        let slots = if resolved_active == default_id { 15 } else { 14 };
        let mut expected = ordinary
            .iter()
            .take(slots)
            .map(|p| p.id.clone())
            .collect::<BTreeSet<_>>();
        expected.insert(default_id.clone());
        expected.insert(resolved_active.clone());
        let id = "a4".repeat(32);
        let result = merge(
            &target,
            &source,
            roster(10, 2),
            roster(1, 2),
            &live(&target, &source),
            &id,
            device(1),
        )
        .expect("capacity eviction must finish the join");
        assert_eq!(result.profiles.len(), 16);
        assert_eq!(
            result.profiles.iter().map(|p| p.id.clone()).collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(result.active_profile_id, resolved_active);
        assert_eq!(result.default_profile().unwrap().id, default_id);
        assert!(result.completed_joins.contains_key(&id));
        assert_eq!(
            merge(
                &result,
                &source,
                roster(10, 2),
                roster(1, 2),
                &live(&target, &source),
                &id,
                device(1)
            )
            .unwrap(),
            result,
            "a retry must not restore evicted profiles"
        );
    }
}
#[test]
fn concurrent_singletons_extend_every_multi_profile_without_losing_either_device() {
    let target = catalog(4, 1, 2, 2);
    let a = catalog(5, 10, 1, 1);
    let b = catalog(6, 11, 1, 1);
    let left = merge(
        &target,
        &a,
        roster(10, 1),
        roster(1, 2),
        &live(&target, &a),
        &"aa".repeat(32),
        device(1),
    )
    .unwrap();
    let right = merge(
        &target,
        &b,
        roster(11, 1),
        roster(1, 2),
        &live(&target, &b),
        &"bb".repeat(32),
        device(2),
    )
    .unwrap();
    let joined = left.receive(&right, device(1)).unwrap();
    let other = right.receive(&left, device(2)).unwrap();
    assert_eq!(joined.profiles.len(), 2);
    assert_eq!(joined.active_profile_id, target.active_profile_id);
    for p in &joined.profiles {
        for n in [1, 2, 10, 11] {
            assert!(
                p.content.placements.iter().any(|p| p.device == device(n)),
                "concurrent append lost a device"
            );
        }
    }
    assert_eq!(joined.profiles, other.profiles);
}

#[test]
fn concurrent_multi_group_overflow_evicts_lru_and_converges_with_protected_selection() {
    let mut target = catalog(4, 1, 2, 6);
    let active = target.active_profile_id.clone();
    target
        .profiles
        .iter_mut()
        .find(|p| p.id == active)
        .unwrap()
        .last_used
        .logical_ms = 0;
    let a = catalog(5, 10, 2, 6);
    let b = catalog(6, 12, 2, 6);
    let left = merge(
        &target,
        &a,
        roster(10, 2),
        roster(1, 2),
        &live(&target, &a),
        &"da".repeat(32),
        device(1),
    )
    .unwrap();
    let right = merge(
        &target,
        &b,
        roster(12, 2),
        roster(1, 2),
        &live(&target, &b),
        &"db".repeat(32),
        device(2),
    )
    .unwrap();
    let default = right.default_profile().unwrap().id.clone();
    let mut candidates = BTreeMap::new();
    for p in left.profiles.iter().chain(&right.profiles) {
        candidates.insert(p.id.clone(), p);
    }
    assert_eq!(candidates.len(), 18);
    let mut ordinary = candidates
        .values()
        .filter(|p| p.id != active && p.id != default)
        .collect::<Vec<_>>();
    ordinary.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
    let mut expected = ordinary.iter().take(14).map(|p| p.id.clone()).collect::<BTreeSet<_>>();
    expected.extend([active.clone(), default.clone()]);
    let joined = left
        .receive(&right, device(1))
        .expect("concurrent overflow must complete");
    let other = right.receive(&left, device(2)).unwrap();
    assert_eq!(joined.profiles, other.profiles);
    assert_eq!(
        joined.profiles.iter().map(|p| p.id.clone()).collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(joined.active_profile_id, active);
    assert_eq!(joined.default_profile().unwrap().id, default);
    assert_eq!(joined.completed_joins.len(), 2);
    assert!(joined.profiles.iter().all(|p| p.content.placements.len() == 6));
    assert_eq!(joined.receive(&right, device(1)).unwrap(), joined);
}

#[test]
fn concurrent_multi_group_joins_complete_all_profiles_and_survive_wire_roundtrip() {
    let target = catalog(4, 1, 2, 2);
    let a = catalog(5, 10, 2, 2);
    let b = catalog(6, 12, 2, 2);
    let left = merge(
        &target,
        &a,
        roster(10, 2),
        roster(1, 2),
        &live(&target, &a),
        &"ca".repeat(32),
        device(1),
    )
    .unwrap();
    let right = merge(
        &target,
        &b,
        roster(12, 2),
        roster(1, 2),
        &live(&target, &b),
        &"cb".repeat(32),
        device(2),
    )
    .unwrap();
    let left = super::wire::read_catalog(super::wire::catalog(&left).unwrap()).unwrap();
    let right = super::wire::read_catalog(super::wire::catalog(&right).unwrap()).unwrap();
    let joined = left.receive(&right, device(1)).unwrap();
    let other = right.receive(&left, device(2)).unwrap();
    assert_eq!(joined.profiles, other.profiles);
    assert_eq!(joined.profiles.len(), 6);
    for p in &joined.profiles {
        assert_eq!(p.content.placements.len(), 6);
        for n in [1, 2, 10, 11, 12, 13] {
            assert!(p.content.disabled.iter().any(|d| d.screen == screen(n) && !d.disabled));
        }
    }
    assert_eq!(
        super::wire::read_catalog(super::wire::catalog(&joined).unwrap()).unwrap(),
        joined
    );
}

#[test]
fn sixteen_profile_multi_group_union_composes_without_evicting_any_original_content() {
    let target = catalog(4, 1, 2, 8);
    let source = catalog(5, 10, 2, 8);
    let result = merge(
        &target,
        &source,
        roster(10, 2),
        roster(1, 2),
        &live(&target, &source),
        &"d1".repeat(32),
        device(1),
    )
    .unwrap();
    assert_eq!(result.profiles.len(), 16);
    for old in target.profiles.iter().chain(&source.profiles) {
        let own = if target.profiles.iter().any(|p| p.id == old.id) {
            roster(1, 2)
        } else {
            roster(10, 2)
        };
        let preserved = result
            .profiles
            .iter()
            .find(|p| {
                p.id == old.id
                    || (old.default_group == Some(target.group)
                        && p.content
                            .placements
                            .iter()
                            .filter(|p| own.contains(&p.device))
                            .all(|p| old.content.placements.contains(p)))
            })
            .unwrap();
        assert_own_content(old, preserved, &own);
        assert_eq!(preserved.content.placements.len(), 4);
    }
}

#[test]
fn late_handoff_keeps_devices_of_a_join_that_completed_first() {
    let target = catalog(4, 1, 2, 2);
    let a = catalog(5, 10, 1, 1);
    let b = catalog(6, 11, 1, 1);
    let completed = merge(
        &target,
        &b,
        roster(11, 1),
        roster(1, 2),
        &live(&target, &b),
        &"eb".repeat(32),
        device(2),
    )
    .unwrap();
    // A's membership commit preceded B, but its retained profile list arrives
    // after B's list was merged. Its bound original target roster still has two.
    let after = merge(
        &completed,
        &a,
        roster(10, 1),
        roster(1, 2),
        &live(&completed, &a),
        &"ea".repeat(32),
        device(1),
    )
    .unwrap();
    assert_eq!(after.profiles.len(), target.profiles.len());
    for old in &completed.profiles {
        let p = after.profile(&old.id).unwrap();
        assert!(p.content.placements.iter().any(|p| p.device == device(10)));
        for previous in &old.content.placements {
            assert!(
                p.content.placements.contains(previous),
                "a late handoff must not erase or reposition an already merged computer"
            );
        }
    }
}

#[test]
fn imported_active_snapshot_cannot_override_current_owner_visibility() {
    for currently_hidden in [false, true] {
        let target = catalog(4, 1, 2, 2);
        let mut source = catalog(5, 10, 1, 2);
        let mut current = live(&target, &source);
        // Selecting another profile without loading it (for example deleting
        // the active one) can leave saved visibility different from live owner bits.
        let active = source.active_profile_id.clone();
        source
            .profiles
            .iter_mut()
            .find(|p| p.id == active)
            .unwrap()
            .content
            .screen_shared[0]
            .shared = currently_hidden;
        current
            .screen_shared
            .iter_mut()
            .find(|p| p.device == device(10))
            .unwrap()
            .shared = !currently_hidden;
        source
            .profiles
            .iter_mut()
            .find(|p| p.id == active)
            .unwrap()
            .content
            .disabled[0]
            .disabled = !currently_hidden;
        current
            .disabled
            .iter_mut()
            .find(|d| d.screen == screen(10))
            .unwrap()
            .disabled = currently_hidden;
        let result = merge(
            &target,
            &source,
            roster(10, 1),
            roster(1, 2),
            &current,
            &"f1".repeat(32),
            device(1),
        )
        .unwrap();
        for p in &result.profiles {
            assert_eq!(screen_shared(&p.content, device(10)), !currently_hidden);
            assert!(
                p.content
                    .disabled
                    .iter()
                    .any(|d| d.screen == screen(10) && d.disabled == currently_hidden),
                "an imported stale active snapshot must not change the arriving owner's visibility"
            );
        }
    }
}
