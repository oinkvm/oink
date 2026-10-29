use super::{model::*, store, types::*, wire};
use crate::api::{DeviceKey, GroupKey, Placement, ScreenKey};
use oink_display::layout::LayoutSettings;
use oink_foundation::{DeviceId, GroupId, Point, Size};
use oink_protocol::v1::schema::WireDto;
use std::collections::BTreeSet;
fn device(n: u8) -> DeviceKey {
    DeviceKey(DeviceId([n; 32]))
}
fn screen(n: u8, name: &str) -> ScreenKey {
    ScreenKey {
        device: device(n),
        display: name.into(),
    }
}
fn snapshot() -> ArrangementSnapshot {
    ArrangementSnapshot {
        screen_shared: Vec::new(),
        placements: vec![
            Placement {
                device: device(1),
                origin: Point::default(),
                scale: 1.0,
            },
            Placement {
                device: device(2),
                origin: Point::new(300.0, 0.0),
                scale: 1.0,
            },
        ],
        disabled: vec![
            Disabled {
                screen: screen(1, "a"),
                disabled: false,
            },
            Disabled {
                screen: screen(1, "b"),
                disabled: true,
            },
            Disabled {
                screen: screen(2, "a"),
                disabled: false,
            },
        ],
        native: vec![
            NativeState {
                device: device(1),
                token: "first".into(),
                primary: screen(1, "a"),
                screens: vec![
                    NativeScreen {
                        screen: screen(1, "a"),
                        origin: Point::default(),
                        size: Size::new(100.0, 100.0),
                    },
                    NativeScreen {
                        screen: screen(1, "b"),
                        origin: Point::new(100.0, 0.0),
                        size: Size::new(100.0, 100.0),
                    },
                ],
            },
            NativeState {
                device: device(2),
                token: "second".into(),
                primary: screen(2, "a"),
                screens: vec![NativeScreen {
                    screen: screen(2, "a"),
                    origin: Point::default(),
                    size: Size::new(100.0, 100.0),
                }],
            },
        ],
        topology: LayoutSettings::default(),
        active_profile_id: String::new(),
    }
}
fn state(s: &ArrangementSnapshot) -> DeviceState {
    DeviceState {
        online_devices: s.placements.iter().map(|p| p.device).collect(),
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
fn devices(s: &ArrangementSnapshot) -> Vec<DeviceArrangementState> {
    s.placements
        .iter()
        .map(|p| DeviceArrangementState {
            device: p.device,
            sync_system_arrangement: false,
            preference_token: "1".into(),
            native_token: Some("test".into()),
            availability: Availability::Ready,
            can_edit_screens: false,
        })
        .collect()
}
fn catalog(s: &ArrangementSnapshot) -> Catalog {
    Catalog::initial(GroupKey(GroupId([4; 16])), device(1), s, state(s), 10).unwrap()
}
#[test]
fn catalog_wire_and_atomic_store_roundtrip() {
    let s = snapshot();
    let mut c = catalog(&s);
    c = c.save(device(1), &s, state(&s), Some("Desk"), 20).unwrap();
    c = c.activate(device(2), &c.profiles[0].id, true, 21).unwrap();
    let raw = wire::catalog(&c).unwrap();
    let bytes = raw.encode(MAX_BYTES).unwrap();
    assert_eq!(
        wire::read_catalog(oink_protocol::v1::dto::LayoutProfileCatalogV3::decode(&bytes, MAX_BYTES).unwrap()).unwrap(),
        c
    );
    let root = tempfile::tempdir().unwrap();
    store::save(root.path(), &c).unwrap();
    assert_eq!(store::load(root.path(), c.group).unwrap(), Some(c.clone()));
    assert_eq!(c.receive(&wire::read_catalog(raw).unwrap(), device(1)).unwrap(), c);
}
#[test]
fn content_comparison_ignores_tokens_order_and_active_but_not_hidden_or_native() {
    let a = snapshot();
    let mut b = a.clone();
    b.native[0].token = "new boot".into();
    b.active_profile_id = "different".into();
    b.placements.reverse();
    b.disabled.reverse();
    assert!(same_content(&a, &b));
    assert!(history_changed(&a, &b));
    b.native[0].screens[1].origin.y = 10.0;
    assert!(!same_content(&a, &b));
    b = a.clone();
    b.disabled[1].disabled = false;
    assert!(!same_content(&a, &b));
}
#[test]
fn exact_restore_is_clean_and_restores_both_hidden_directions_without_compaction() {
    let saved = snapshot();
    let mut live = saved.clone();
    live.placements[1].origin.x = 1000.0;
    live.disabled[0].disabled = true;
    live.disabled[1].disabled = false;
    let result = restore(&saved, &live, &devices(&live)).unwrap();
    assert!(same_content(&saved, &result.snapshot));
    assert!(result.adjustments.is_empty());
    assert_eq!(result.snapshot.placements[1].origin.x, 300.0);
}
#[test]
fn native_off_keeps_actual_native_anchors_primary_and_repairs_other_device_overlap() {
    let saved = snapshot();
    let mut live = saved.clone();
    live.native[0].screens[0].size.w = 350.0;
    live.native[0].screens[1].origin.x = 350.0;
    let result = restore(&saved, &live, &devices(&live)).unwrap();
    assert_eq!(result.snapshot.placements[0].origin, Point::default());
    assert!(same_native(&result.snapshot.native[0], &live.native[0]));
    assert_eq!(result.snapshot.placements[1].origin.x, 350.0);
    assert!(!same_content(&saved, &result.snapshot));
    assert!(result.native_devices.is_empty());
}
#[test]
fn native_on_restores_system_origins_and_becomes_clean() {
    let saved = snapshot();
    let mut live = saved.clone();
    live.native[0].screens[1].origin = Point::new(0.0, 100.0);
    let mut owners = devices(&live);
    owners[0].sync_system_arrangement = true;
    let result = restore(&saved, &live, &owners).unwrap();
    assert!(same_content(&saved, &result.snapshot));
    assert_eq!(result.native_devices, vec![device(1)]);
}
#[test]
fn native_exact_restore_does_not_depend_on_display_key_order() {
    let mut saved = snapshot();
    saved.native[0].screens[1].screen = screen(1, "z");
    saved.disabled[1].screen = screen(1, "z");
    saved.native[0].screens.push(NativeScreen {
        screen: screen(1, "c"),
        origin: Point::new(200.0, 0.0),
        size: Size::new(100.0, 100.0),
    });
    saved.disabled.push(Disabled {
        screen: screen(1, "c"),
        disabled: false,
    });
    let mut owners = devices(&saved);
    owners[0].sync_system_arrangement = true;
    assert!(same_content(
        &saved,
        &restore(&saved, &saved, &owners).unwrap().snapshot
    ));
}
#[test]
fn offline_restores_position_but_never_queues_visibility_or_native_writes() {
    let saved = snapshot();
    let mut live = saved.clone();
    live.placements[0].origin = Point::new(900.0, 20.0);
    live.disabled[1].disabled = false;
    let mut owners = devices(&live);
    owners[0].availability = Availability::Offline;
    owners[0].sync_system_arrangement = true;
    let result = restore(&saved, &live, &owners).unwrap();
    assert_eq!(result.snapshot.placements[0], saved.placements[0]);
    assert!(!result.snapshot.disabled[1].disabled);
    assert!(result.native_devices.is_empty());
    assert_eq!(
        result
            .adjustments
            .iter()
            .filter(|a| a.code == AdjustmentCode::OwnerOffline)
            .count(),
        2
    );
    assert!(!same_content(&saved, &result.snapshot));
}
#[test]
fn new_device_is_placed_without_moving_saved_devices() {
    let mut saved = snapshot();
    saved.placements.pop();
    saved.native.pop();
    saved.disabled.retain(|s| s.screen.device == device(1));
    let mut live = snapshot();
    live.placements[1].origin = Point::default();
    let result = restore(&saved, &live, &devices(&live)).unwrap();
    assert_eq!(result.snapshot.placements[0], saved.placements[0]);
    assert_ne!(result.snapshot.placements[1].origin, Point::default());
    assert!(result.adjustments.iter().any(|a| a.code == AdjustmentCode::DeviceAdded));
}
#[test]
fn removed_device_and_disconnected_screen_are_dropped() {
    let saved = snapshot();
    let mut live = saved.clone();
    live.placements.pop();
    live.native.pop();
    live.native[0].screens.pop();
    live.disabled.truncate(1);
    let result = restore(&saved, &live, &devices(&live)).unwrap();
    assert_eq!(result.snapshot.native[0].screens.len(), 1);
    assert_eq!(result.snapshot.placements.len(), 1);
    assert!(
        result
            .adjustments
            .iter()
            .any(|a| a.code == AdjustmentCode::DeviceRemoved)
    );
    assert!(
        result
            .adjustments
            .iter()
            .any(|a| a.code == AdjustmentCode::ScreenRemoved)
    );
}
#[test]
fn mru_profiles_precede_older_matches_and_unknown_is_never_exact() {
    let s = snapshot();
    let initial = catalog(&s);
    let first = initial.active_profile_id.clone();
    let mut different = state(&s);
    different.online_devices.remove(&device(2));
    let c = initial.save(device(1), &s, different, Some("Other"), 100).unwrap();
    assert_ne!(c.summaries(&state(&s))[0].id, first);
    let mut unknown = state(&s);
    unknown.connected_screens.insert(device(2), None);
    assert!(c.summaries(&unknown).iter().all(|p| !p.exact_match));
}
#[test]
fn sixteenth_profile_blocks_create_and_group_merge_drops_least_recent() {
    let s = snapshot();
    let mut c = catalog(&s);
    let old = c.active_profile_id.clone();
    for n in 1..16 {
        c = c
            .save(device(1), &s, state(&s), Some(&format!("Desk {n}")), n + 10)
            .unwrap();
    }
    assert_eq!(
        c.save(device(1), &s, state(&s), Some("Seventeenth"), 100)
            .unwrap_err()
            .code,
        ErrorCode::ProfileLimit
    );
    let mut source = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 90).unwrap();
    source = source.save(device(2), &s, state(&s), Some("Imported"), 100).unwrap();
    let merged = c.merge_group(&source, &"ab".repeat(32), device(1)).unwrap();
    assert_eq!(merged.profiles.len(), 16);
    assert!(!merged.profiles.iter().any(|p| p.id == old));
    assert!(merged.profiles.iter().any(|p| p.id == source.active_profile_id));
    assert_eq!(
        merged.merge_group(&source, &"ab".repeat(32), device(1)).unwrap(),
        merged
    );
}

fn accepting_default_is_dropped(source_devices: usize) {
    let target_snapshot = snapshot();
    let mut source_snapshot = snapshot();
    source_snapshot.placements.truncate(source_devices);
    source_snapshot.native.truncate(source_devices);
    source_snapshot
        .disabled
        .retain(|s| source_snapshot.placements.iter().any(|p| p.device == s.screen.device));
    let target = catalog(&target_snapshot);
    let source = Catalog::initial(
        GroupKey(GroupId([5; 16])),
        device(2),
        &source_snapshot,
        state(&source_snapshot),
        5,
    )
    .unwrap();
    let target_default = target.active_profile_id.clone();
    let source_default = source.active_profile_id.clone();
    let target = target
        .save(
            device(1),
            &target_snapshot,
            state(&target_snapshot),
            Some("Target custom"),
            30,
        )
        .unwrap()
        .activate(device(1), &target_default, true, 40)
        .unwrap();
    let source = source
        .save(
            device(2),
            &source_snapshot,
            state(&source_snapshot),
            Some("Source custom"),
            20,
        )
        .unwrap();
    let merged = target.merge_group(&source, &"ef".repeat(32), device(1)).unwrap();
    assert!(!merged.profiles.iter().any(|p| p.id == target_default));
    assert_eq!(merged.profiles.len(), 3);
    assert_eq!(merged.active_profile_id, source_default);
    assert_eq!(
        merged
            .profiles
            .iter()
            .filter(|p| p.name == ProfileName::Default)
            .count(),
        1
    );
    for profile in &merged.profiles {
        let original = target
            .profiles
            .iter()
            .chain(&source.profiles)
            .find(|p| p.id == profile.id)
            .unwrap();
        assert_eq!(profile.content, original.content);
        assert_eq!(profile.name, original.name);
        assert_eq!(profile.content_revision, original.content_revision);
        assert_eq!(profile.last_used, original.last_used);
    }
}

#[test]
fn group_join_discards_accepting_group_default() {
    accepting_default_is_dropped(2);
}

#[test]
fn single_device_join_discards_accepting_group_default() {
    accepting_default_is_dropped(1);
}

#[test]
fn delete_last_is_refused_and_rename_preserves_recency_and_content() {
    let s = snapshot();
    let c = catalog(&s);
    let named = c.rename(device(1), &c.active_profile_id, "Work").unwrap();
    assert_eq!(named.active().last_used, c.active().last_used);
    assert!(same_content(&named.active().content, &s));
    assert_eq!(
        named
            .delete(device(1), &named.active_profile_id, &s, state(&s), 100)
            .unwrap_err()
            .code,
        ErrorCode::LastProfile
    );
}
#[test]
fn load_undo_redo_restores_disabled_state_and_active_profile_at_state_level() {
    let mut before = snapshot();
    before.active_profile_id = "first".into();
    let mut target = before.clone();
    target.active_profile_id = "second".into();
    target.disabled[0].disabled = true;
    target.disabled[1].disabled = false;
    target.placements[1].origin.x = 600.0;
    let mut loaded = restore(&target, &before, &devices(&before)).unwrap().snapshot;
    loaded.active_profile_id = target.active_profile_id.clone();
    assert!(history_changed(&before, &loaded));
    let mut undone = restore(&before, &loaded, &devices(&loaded)).unwrap().snapshot;
    undone.active_profile_id = before.active_profile_id.clone();
    assert!(!history_changed(&before, &undone));
    let redone = restore(&target, &undone, &devices(&undone)).unwrap().snapshot;
    assert!(same_content(&target, &redone));
}
#[test]
fn finite_integer_native_and_unique_visibility_are_required() {
    let mut s = snapshot();
    s.native[0].screens[0].origin.x = 0.5;
    assert!(validate_snapshot(&s).is_err());
    s = snapshot();
    s.disabled.push(s.disabled[0].clone());
    assert!(validate_snapshot(&s).is_err());
    assert_eq!(
        state(&snapshot()).online_devices,
        BTreeSet::from([device(1), device(2)])
    );
}

#[test]
fn future_catalog_fields_survive_rename_save_and_store() {
    use oink_protocol::v1::{RawValue as V, dto};
    let s = snapshot();
    let c = catalog(&s);
    let mut raw = wire::catalog(&c).unwrap().into_raw();
    raw.insert(77, V::Text("future catalog".into()));
    let V::Array(profiles) = raw.get_mut(&5).unwrap() else {
        panic!()
    };
    let V::Map(profile) = &mut profiles[0] else { panic!() };
    profile.insert(77, V::Text("future profile".into()));
    let V::Map(content) = profile.get_mut(&5).unwrap() else {
        panic!()
    };
    let V::Array(placements) = content.get_mut(&1).unwrap() else {
        panic!()
    };
    let V::Map(placement) = &mut placements[0] else {
        panic!()
    };
    placement.insert(77, V::Text("future placement".into()));
    let c = wire::read_catalog(dto::LayoutProfileCatalogV3::from_raw(raw).unwrap()).unwrap();
    let c = c.rename(device(1), &c.active_profile_id, "Renamed").unwrap();
    let mut changed = s.clone();
    changed.placements[0].origin.x = 50.0;
    let c = c.save(device(1), &changed, state(&changed), None, 30).unwrap();
    let root = tempfile::tempdir().unwrap();
    store::save(root.path(), &c).unwrap();
    let loaded = store::load(root.path(), c.group).unwrap().unwrap();
    assert_eq!(loaded, c);
    let raw = wire::catalog(&loaded).unwrap();
    assert_eq!(raw.raw().get(&77), Some(&V::Text("future catalog".into())));
    let p = &raw.profiles().unwrap().0[0];
    assert_eq!(p.raw().get(&77), Some(&V::Text("future profile".into())));
    let placements = p.content().unwrap().placements().unwrap();
    assert_eq!(
        placements.0[0].raw().get(&77),
        Some(&V::Text("future placement".into()))
    );
    assert_eq!(placements.0[0].origin().unwrap().x(), Ok(50.0));
}

#[test]
fn replay_reconciles_membership_but_refuses_native_dimension_changes() {
    let before = snapshot();
    let mut after = before.clone();
    after.placements[0].origin.x -= 20.0;
    let mut live = after.clone();
    live.placements.pop();
    live.native.pop();
    live.disabled.retain(|b| b.screen.device != device(2));
    let mut new = before.placements[1].clone();
    new.device = device(3);
    new.origin.x = 1000.0;
    live.placements.push(new.clone());
    let mut native = before.native[1].clone();
    native.device = device(3);
    native.primary.device = device(3);
    for s in &mut native.screens {
        s.screen.device = device(3);
        live.disabled.push(Disabled {
            screen: s.screen.clone(),
            disabled: false,
        });
    }
    live.native.push(native);
    let result = replay(&after, &before, &live, &devices(&live), &[]).unwrap();
    assert!(result.snapshot.placements.contains(&new));
    assert!(!result.snapshot.placements.iter().any(|p| p.device == device(2)));
    let mut live = after.clone();
    live.native[0].screens[0].size.w += 1.0;
    let mut owners = devices(&live);
    owners[0].sync_system_arrangement = true;
    assert_eq!(
        replay(&after, &before, &live, &owners, &[device(1)]).unwrap_err().code,
        ErrorCode::NativeChanged
    );
}

#[test]
fn offline_screen_churn_preserves_known_visibility_without_resurrecting_screens() {
    let saved = snapshot();
    let mut live = saved.clone();
    let removed = live.native[0].screens.pop().unwrap().screen;
    live.disabled.retain(|b| b.screen != removed);
    let mut owners = devices(&live);
    owners[0].availability = Availability::Offline;
    let result = restore(&saved, &live, &owners).unwrap();
    assert_eq!(result.snapshot.native[0].screens.len(), 1);
    assert_eq!(result.snapshot.disabled, live.disabled);
    assert!(result.native_devices.is_empty());
}

#[test]
fn shared_dto_retains_both_visibility_layers_and_default_sharing() {
    let original = snapshot();
    let mut changed = original.clone();
    set_screen_shared(&mut changed, device(1), false);
    changed.disabled[0].disabled = true;
    assert!(!same_content(&original, &changed));
    let raw = wire::snapshot(&changed).unwrap();
    let oink_protocol::v1::RawValue::Map(raw) = raw else {
        unreachable!()
    };
    let dto = oink_protocol::v1::dto::LayoutProfileSnapshot::from_raw(raw).unwrap();
    let decoded = wire::read_snapshot(dto, &changed.active_profile_id).unwrap();
    assert!(same_content(&changed, &decoded));
    assert!(!screen_shared(&decoded, device(1)));
    assert!(decoded.disabled[0].disabled);
    let mut explicit_default = original.clone();
    set_screen_shared(&mut explicit_default, device(1), true);
    assert!(same_content(&original, &explicit_default));
}

#[test]
fn redo_020_catalog_accepts_sixteen_and_blocks_seventeenth() {
    let s = snapshot();
    let mut c = catalog(&s);
    for n in 1..16 {
        c = c
            .save(device(1), &s, state(&s), Some(&format!("Desk {n}")), n + 10)
            .unwrap();
    }
    assert_eq!(c.profiles.len(), 16);
    assert_eq!(
        c.save(device(1), &s, state(&s), Some("Seventeenth"), 30)
            .unwrap_err()
            .code,
        ErrorCode::ProfileLimit
    );
}
#[test]
fn redo_020_catalog_rejects_duplicate_names_and_thirty_three_characters() {
    let s = snapshot();
    let c = catalog(&s).save(device(1), &s, state(&s), Some(" Desk "), 20).unwrap();
    assert!(c.save(device(1), &s, state(&s), Some("dEsK"), 30).is_err());
    assert!(valid_name(&"界".repeat(32)).is_ok());
    assert!(valid_name(&"a".repeat(33)).is_err());
}
#[test]
fn redo_020_catalog_keeps_last_profile_and_uses_pure_mru() {
    let s = snapshot();
    let c = catalog(&s);
    assert!(c.delete(device(1), &c.active_profile_id, &s, state(&s), 30).is_err());
    let mut different = state(&s);
    different.online_devices.remove(&device(2));
    let c = c.save(device(1), &s, different, Some("More recent"), 100).unwrap();
    assert_eq!(c.summaries(&state(&s))[0].id, c.active_profile_id);
}

#[test]
fn manual_visibility_of_new_screen_is_saved_without_automatic_drift() {
    let live = snapshot();
    let mut saved = live.clone();
    saved.native[0].screens.pop();
    saved.disabled.retain(|b| b.screen != screen(1, "b"));
    let c = catalog(&saved);
    let mut before = live.clone();
    before.placements[0].origin.x = 400.0;
    let mut after = before.clone();
    after
        .disabled
        .iter_mut()
        .find(|b| b.screen == screen(1, "b"))
        .unwrap()
        .disabled = false;
    let next = c
        .autosave_manual(device(1), &c.active_profile_id, &before, &after, &after, 100)
        .unwrap();
    let content = &next.profile(&c.active_profile_id).unwrap().content;
    assert!(
        content
            .disabled
            .iter()
            .any(|b| b.screen == screen(1, "b") && !b.disabled)
    );
    assert_eq!(content.native[0].screens.len(), 2);
    assert_eq!(content.placements[0].origin, saved.placements[0].origin);
}

#[test]
fn offline_sync_is_effectively_rigid_and_reconnect_retains_placement() {
    assert!(!effective_system_sync(false, false));
    assert!(!effective_system_sync(false, true));
    assert!(!effective_system_sync(true, false));
    assert!(effective_system_sync(true, true));
    let before = snapshot();
    let mut edited = before.clone();
    edited.placements[0].origin = Point::new(-300.0, 25.0);
    let mut owners = devices(&before);
    owners[0].sync_system_arrangement = true;
    owners[0].availability = Availability::Offline;
    let offline = restore(&edited, &before, &owners).unwrap();
    assert_eq!(offline.snapshot.placements[0], edited.placements[0]);
    assert_eq!(offline.snapshot.native[0].screens, before.native[0].screens);
    assert!(!offline.native_devices.contains(&device(1)));
    owners[0].availability = Availability::Ready;
    let online = restore(&offline.snapshot, &offline.snapshot, &owners).unwrap();
    assert_eq!(online.snapshot.placements, offline.snapshot.placements);
    assert_eq!(online.snapshot.native[0].screens, before.native[0].screens);
    assert!(online.native_devices.is_empty());
}

// The 0.2.0 catalog algorithm, kept here to exercise mixed-version import order.
// It preserves unknown profile fields but only the target catalog's extensions.

#[test]
fn second_join_discards_inherited_default_even_after_rename() {
    let s = snapshot();
    let target = catalog(&s);
    let first = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 10).unwrap();
    let second = Catalog::initial(GroupKey(GroupId([6; 16])), device(3), &s, state(&s), 1).unwrap();
    let merged = target.merge_group(&first, &"ab".repeat(32), device(1)).unwrap();
    assert_eq!(merged.default_profile().unwrap().id, first.active_profile_id);
    let merged = merged
        .rename(device(1), &first.active_profile_id, "Renamed inherited default")
        .unwrap();
    let custom = merged.save(device(1), &s, state(&s), Some("Keep custom"), 30).unwrap();
    let active_custom = custom.active_profile_id.clone();
    let active_default = custom.activate(device(1), &first.active_profile_id, true, 40).unwrap();
    let result = active_default
        .merge_group(&second, &"cd".repeat(32), device(1))
        .unwrap();
    assert!(!result.profiles.iter().any(|p| p.id == first.active_profile_id));
    assert_eq!(result.default_profile().unwrap().id, second.active_profile_id);
    assert_eq!(result.active_profile_id, second.active_profile_id);
    assert!(result.profiles.iter().any(|p| p.id == active_custom));
    let kept = custom.merge_group(&second, &"cd".repeat(32), device(1)).unwrap();
    assert_eq!(kept.active_profile_id, active_custom);
}

#[test]
fn deleted_defaults_are_not_recreated_and_surviving_default_is_not_pinned_over_mru() {
    let s = snapshot();
    let target = catalog(&s);
    let source = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 1).unwrap();
    let target = target
        .save(device(1), &s, state(&s), Some("Target custom"), 20)
        .unwrap()
        .delete(device(1), &target.active_profile_id, &s, state(&s), 21)
        .unwrap();
    let source = source
        .save(device(2), &s, state(&s), Some("Source custom"), 20)
        .unwrap()
        .delete(device(2), &source.active_profile_id, &s, state(&s), 21)
        .unwrap();
    let merged = target.merge_group(&source, &"ab".repeat(32), device(1)).unwrap();
    assert_eq!(merged.profiles.len(), 2);
    assert!(merged.default_profile().is_none());
    let mut full = catalog(&s);
    for n in 0..15 {
        full = full
            .save(device(1), &s, state(&s), Some(&format!("Desk {n}")), 50 + n)
            .unwrap();
    }
    let source = Catalog::initial(GroupKey(GroupId([6; 16])), device(2), &s, state(&s), 1)
        .unwrap()
        .save(device(2), &s, state(&s), Some("Newest"), 100)
        .unwrap();
    let merged = full.merge_group(&source, &"cd".repeat(32), device(1)).unwrap();
    assert_eq!(merged.profiles.len(), 16);
    assert!(merged.default_profile().is_none());
}

#[test]
fn acceptor_crash_retry_keeps_completion_and_later_deletions_atomic() {
    let s = snapshot();
    let target = catalog(&s);
    let source = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 10)
        .unwrap()
        .save(device(2), &s, state(&s), Some("Delete after import"), 20)
        .unwrap();
    let id = "bb".repeat(32);
    let root = tempfile::tempdir().unwrap();
    store::save(root.path(), &target).unwrap();
    let computed = target.merge_group(&source, &id, device(1)).unwrap();
    // Crash before the atomic import write: retry starts from the old envelope.
    let recovered = store::load(root.path(), target.group)
        .unwrap()
        .unwrap()
        .merge_group(&source, &id, device(1))
        .unwrap();
    assert_eq!(recovered, computed);
    store::save(root.path(), &recovered).unwrap();
    let deleted = recovered
        .delete(device(1), &source.active_profile_id, &s, state(&s), 40)
        .unwrap();
    store::save(root.path(), &deleted).unwrap();
    let recovered = store::load(root.path(), target.group).unwrap().unwrap();
    assert_eq!(recovered.merge_group(&source, &id, device(1)).unwrap(), deleted);
    assert!(recovered.completed_joins.contains_key(&id));
    assert!(
        recovered.completed_joins[&id]
            .dropped_defaults
            .contains(&target.active_profile_id)
    );
    assert!(!recovered.profiles.iter().any(|p| p.id == target.active_profile_id));
}

#[test]
fn ordinary_stale_catalog_receipt_does_not_reorder_or_advance_after_create() {
    let s = snapshot();
    let before = catalog(&s);
    let created = before.save(device(1), &s, state(&s), Some("New profile"), 20).unwrap();
    assert_eq!(created.receive(&before, device(1)).unwrap(), created);
}

#[test]
fn acceptor_result_is_adopted_without_member_side_merge() {
    let s = snapshot();
    let target = catalog(&s);
    let source = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 10).unwrap();
    let result = target.merge_group(&source, &"ab".repeat(32), device(1)).unwrap();
    assert_eq!(target.receive(&result, device(3)).unwrap(), result);
}

#[test]
fn simultaneous_acceptors_union_once_and_use_later_completed_default() {
    let s = snapshot();
    let target = catalog(&s);
    let source_a = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 10).unwrap();
    let source_b = Catalog::initial(GroupKey(GroupId([6; 16])), device(3), &s, state(&s), 20).unwrap();
    let a = target.merge_group(&source_a, &"aa".repeat(32), device(1)).unwrap();
    let b = target.merge_group(&source_b, &"bb".repeat(32), device(4)).unwrap();
    assert!(b.version > a.version);
    let joined = a.receive(&b, device(1)).unwrap();
    assert_eq!(joined.profiles.len(), 2);
    assert_eq!(joined.default_profile().unwrap().id, source_b.active_profile_id);
    assert_eq!(joined.completed_joins.len(), 2);
    assert!(joined.profiles.iter().any(|p| p.id == source_a.active_profile_id));
    assert!(!joined.profiles.iter().any(|p| p.id == target.active_profile_id));
    let other = b.receive(&a, device(4)).unwrap();
    let converged = joined.receive(&other, device(1)).unwrap();
    assert_eq!(other, converged);
    assert_eq!(a.receive(&converged, device(1)).unwrap(), converged);
    assert_eq!(b.receive(&converged, device(4)).unwrap(), converged);
    // Ordinary later deletion stays deleted even when an old result is replayed.
    let deleted = converged
        .delete(device(1), &source_a.active_profile_id, &s, state(&s), 30)
        .unwrap();
    assert_eq!(deleted.receive(&a, device(1)).unwrap(), deleted);
}

#[test]
fn rapid_second_pairing_result_needs_no_first_group_history() {
    let s = snapshot();
    let source = catalog(&s);
    let intermediate = Catalog::initial(GroupKey(GroupId([5; 16])), device(2), &s, state(&s), 10).unwrap();
    let final_target = Catalog::initial(GroupKey(GroupId([6; 16])), device(3), &s, state(&s), 20).unwrap();
    let first = intermediate.merge_group(&source, &"aa".repeat(32), device(2)).unwrap();
    let second = final_target.merge_group(&first, &"bb".repeat(32), device(3)).unwrap();
    assert_eq!(second.completed_joins.len(), 1);
    assert_eq!(second.default_profile().unwrap().id, source.active_profile_id);
    assert_eq!(final_target.receive(&second, device(4)).unwrap(), second);
    assert_eq!(second.receive(&final_target, device(3)).unwrap(), second);
}

#[test]
fn old_saved_profiles_load_unchanged_and_obsolete_fields_are_ignored() {
    use base64::Engine as _;
    use oink_protocol::v1::{RawValue as V, dto};
    let s = snapshot();
    let original = catalog(&s)
        .save(device(1), &s, state(&s), Some("Keep existing"), 20)
        .unwrap();
    let mut raw = wire::catalog(&original).unwrap().into_raw();
    raw.remove(&9);
    raw.insert(6, V::Array(vec![V::Bytes(vec![1; 32])]));
    raw.insert(7, V::Array(vec![V::Bytes(vec![2; 16])]));
    raw.insert(8, V::Array(vec![V::Bytes(vec![1; 32])]));
    let legacy = dto::LayoutProfileCatalogV2::from_raw(raw).unwrap();
    let root = tempfile::tempdir().unwrap();
    store::save(root.path(), &original).unwrap();
    let path = std::fs::read_dir(root.path().join("layout-profiles-v2"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let envelope = serde_json::json!({"schema":1,"catalog_cbor":base64::engine::general_purpose::STANDARD.encode(legacy.encode(MAX_BYTES).unwrap())});
    std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    let loaded = store::load(root.path(), original.group).unwrap().unwrap();
    assert_eq!(loaded, original);
    store::save(root.path(), &loaded).unwrap();
    assert_eq!(store::load(root.path(), original.group).unwrap().unwrap(), original);
    assert!(
        ![6, 7, 8]
            .iter()
            .any(|k| wire::catalog(&loaded).unwrap().raw().contains_key(k))
    );
}

#[test]
fn automatic_repair_cannot_complete_an_unfinished_manual_save() {
    let before = snapshot();
    let original = catalog(&before);
    let mut intended = before.clone();
    intended.placements[0].origin.x += 10.0;
    let mut actual = before.clone();
    // Recovery after a crash sees a hotplug repair, not the requested move.
    actual.placements[0].origin.x += 40.0;
    let saved = original
        .autosave_manual(device(1), &original.active_profile_id, &before, &intended, &actual, 20)
        .unwrap();
    assert_eq!(saved, original);
}

#[test]
fn automatic_native_change_cannot_complete_an_unfinished_manual_save() {
    let before = snapshot();
    let original = catalog(&before);
    let mut intended = before.clone();
    intended.native[0].screens[1].origin.y += 10.0;
    let mut actual = before.clone();
    actual.native[0].screens[1].origin.y += 40.0;
    let saved = original
        .autosave_manual(device(1), &original.active_profile_id, &before, &intended, &actual, 20)
        .unwrap();
    assert_eq!(saved, original);
}

fn physical(s: &ArrangementSnapshot) -> super::topology::PhysicalTopology {
    s.native
        .iter()
        .map(|n| super::topology::PhysicalDevice {
            device: n.device,
            screens: n
                .screens
                .iter()
                .map(|s| super::topology::PhysicalScreen {
                    id: s.screen.display.clone(),
                    bounds: oink_foundation::Rect::new(s.origin.x, s.origin.y, s.size.w, s.size.h),
                    pixel_width: s.size.w as u32 * 2,
                    pixel_height: s.size.h as u32 * 2,
                    scale: 2.0,
                    rotation: 0,
                    primary: s.screen == n.primary,
                })
                .collect(),
        })
        .collect()
}
#[test]
fn topology_return_restores_once_and_manual_save_cancels_prior_excursion() {
    let before = snapshot();
    let mut c = catalog(&before);
    let mut tracker = super::topology::RestoreTracker::default();
    let saved = physical(&before);
    // Profiles from older builds are inert, even after a topology excursion.
    let mut other = saved.clone();
    other[0].screens.pop();
    assert!(!tracker.observe(c.active(), Some(&other)));
    assert!(!tracker.observe(c.active(), Some(&saved)));
    c.profiles[0].saved_topology = Some(saved.clone());
    assert!(!tracker.observe(c.active(), Some(&saved)));
    for change in 0..7 {
        let mut other = saved.clone();
        match change {
            0 => {
                other[0].screens.pop();
            }
            1 => other[0].screens[0].id.push_str("-replacement"),
            2 => other[0].screens[0].bounds.w += 1.0,
            3 => other[0].screens[0].pixel_width += 1,
            4 => other[0].screens[0].scale += 0.5,
            5 => other[0].screens[0].rotation = 90,
            _ => other[0].screens[0].primary = !other[0].screens[0].primary,
        }
        assert!(!tracker.observe(c.active(), Some(&other)));
        assert!(!tracker.observe(c.active(), None)); // incomplete facts aren't an empty topology
        assert!(tracker.observe(c.active(), Some(&saved)));
        assert!(!tracker.observe(c.active(), Some(&saved))); // duplicate observations/restore echoes
    }
    assert!(!tracker.observe(c.active(), Some(&other)));
    c.profiles[0].content_revision.counter += 1;
    assert!(!tracker.observe(c.active(), Some(&saved))); // newer manual edit cancels excursion
    assert!(!tracker.observe(c.active(), Some(&other)));
    tracker.manual_layout(c.active()); // manual register arrived before its catalog
    assert!(!tracker.observe(c.active(), Some(&other))); // periodic ticks cannot re-arm stale content
    assert!(!tracker.observe(c.active(), Some(&saved)));
    c.profiles[0].content_revision.counter += 1;
    assert!(!tracker.observe(c.active(), Some(&other)));
    assert!(tracker.observe(c.active(), Some(&saved))); // companion save enables future excursions
    let wire = wire::catalog(&c).unwrap();
    assert_eq!(wire::read_catalog(wire).unwrap(), c);
    let encoded = serde_json::to_string(&c.active().saved_topology).unwrap();
    assert!(!encoded.contains("online"));
}

#[test]
fn profile_match_and_new_saved_state_do_not_track_presence() {
    let s = snapshot();
    let mut offline = state(&s);
    offline.online_devices.clear();
    assert!(state(&s).exact(&offline));
    let c = catalog(&s);
    assert!(c.active().match_state.online_devices.is_empty());
    assert!(c.summaries(&offline)[0].exact_match);
    let saved = c.save(device(1), &s, state(&s), None, 30).unwrap();
    assert!(saved.active().match_state.online_devices.is_empty());
}

#[test]
fn older_writer_cannot_rebind_retained_physical_topology() {
    use oink_protocol::v1::{RawValue as V, dto, schema::WireValue};
    let s = snapshot();
    let mut c = catalog(&s);
    c.profiles[0].saved_topology = Some(physical(&s));
    let old_revision = wire::catalog(&c).unwrap().profiles().unwrap().0[0]
        .content_revision()
        .unwrap()
        .write();
    c.version.counter += 1;
    c.profiles[0].content_revision = c.version.clone();
    c.profiles[0].content.placements[0].origin.x += 10.0;
    let mut raw = wire::catalog(&c).unwrap().into_raw();
    let V::Array(profiles) = raw.get_mut(&5).unwrap() else {
        panic!("profiles")
    };
    let V::Map(profile) = &mut profiles[0] else {
        panic!("profile")
    };
    // An older build updates content/revision but preserves unknown metadata.
    profile.insert(9, old_revision);
    let received = wire::read_catalog(dto::LayoutProfileCatalogV3::from_raw(raw).unwrap()).unwrap();
    assert!(received.active().saved_topology.is_none());
    assert_eq!(received.active().content, c.active().content);
    let encoded = wire::catalog(&received).unwrap();
    assert!(!encoded.profiles().unwrap().0[0].raw().contains_key(&8));
    assert!(!encoded.profiles().unwrap().0[0].raw().contains_key(&9));
}
