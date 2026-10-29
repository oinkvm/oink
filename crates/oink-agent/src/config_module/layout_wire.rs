//! Shared rigid computer placements. Native screen geometry is owned by its OS;
//! canvas rectangles are expanded views and are never independently persisted.
use oink_display::layout::{Layout, LayoutSettings, ScreenId, ScreenPlacement, SizeSource};
use oink_foundation::{DeviceId, Point, Rect};
use oink_protocol::v1::{
    self, ErrorCode, RawMap, RawValue, dto,
    schema::{WireDto, WireValue},
};
pub fn validate(layout: &dto::LayoutValue) -> v1::Result<()> {
    v1::layout::require_supported_geometry(layout)
}
fn native_device(device: &dto::DeviceId) -> v1::Result<DeviceId> {
    Ok(DeviceId(
        device
            .public_key()?
            .0
            .try_into()
            .map_err(|_| ErrorCode::UnsupportedValue)?,
    ))
}
pub fn from_wire(value: &dto::LayoutValue) -> v1::Result<Layout> {
    validate(value)?;
    let mut screens = Vec::new();
    for group in value.devices()?.0 {
        let k = group.scale()?;
        let origin = group.origin()?;
        for screen in group.screens()?.0 {
            let id = screen.id()?;
            let r = screen.local()?;
            let local = Rect::new(r.x()?, r.y()?, r.w()?, r.h()?);
            screens.push(ScreenPlacement {
                id: ScreenId::new(native_device(&id.device()?)?, id.display()?.0),
                name: screen.name()?.0,
                local,
                rect: Rect::new(
                    origin.x()? + local.x * k,
                    origin.y()? + local.y * k,
                    local.w * k,
                    local.h * k,
                ),
                size_source: match screen.size_source()? {
                    dto::SizeSource::Reported => SizeSource::Reported,
                    dto::SizeSource::Estimated => SizeSource::Estimated,
                    dto::SizeSource::Manual => SizeSource::Manual,
                    _ => return Err(ErrorCode::UnsupportedValue.into()),
                },
                primary: screen.primary()?,
                builtin: screen.builtin()?,
            });
        }
    }
    let st = value.topology()?;
    Ok(Layout {
        screens,
        settings: LayoutSettings {
            gap_travel: st.gap_travel()?,
            overlap_tolerance: st.overlap_tolerance()?,
            minimum_portal: st.minimum_portal()?,
            portal_snap: st.portal_snap()?,
            walls: st.walls()?,
            endpoint_reach: st.endpoint_reach()?,
        },
    })
}
fn rect(r: Rect) -> RawValue {
    RawValue::Map([(1, r.x.write()), (2, r.y.write()), (3, r.w.write()), (4, r.h.write())].into())
}
fn point(p: Point) -> RawValue {
    RawValue::Map([(1, p.x.write()), (2, p.y.write())].into())
}
fn to_screen(s: &ScreenPlacement) -> v1::Result<dto::ScreenPlacement> {
    dto::ScreenPlacement::from_raw(
        [
            (
                1,
                RawValue::Map(
                    [
                        (1, crate::config::store::wire_device(s.id.device).write()),
                        (2, RawValue::from(s.id.display.as_str())),
                    ]
                    .into(),
                ),
            ),
            (2, RawValue::from(s.name.as_str())),
            (3, rect(s.local)),
            (
                5,
                match s.size_source {
                    SizeSource::Reported => dto::SizeSource::Reported,
                    SizeSource::Estimated => dto::SizeSource::Estimated,
                    SizeSource::Manual => dto::SizeSource::Manual,
                }
                .write(),
            ),
            (9, s.primary.write()),
            (10, s.builtin.write()),
        ]
        .into(),
    )
}
pub fn to_wire(layout: &Layout, source: dto::LayoutSource) -> v1::Result<dto::LayoutValue> {
    let mut groups = Vec::new();
    for device in layout
        .screens
        .iter()
        .map(|s| s.id.device)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (origin, k) = layout.device_transform(device).ok_or(ErrorCode::StateConflict)?;
        let mut screens = Vec::new();
        for s in layout.screens.iter().filter(|s| s.id.device == device) {
            let expected = Rect::new(
                origin.x + s.local.x * k,
                origin.y + s.local.y * k,
                s.local.w * k,
                s.local.h * k,
            );
            if [
                s.rect.x - expected.x,
                s.rect.y - expected.y,
                s.rect.w - expected.w,
                s.rect.h - expected.h,
            ]
            .into_iter()
            .any(|d| d.abs() > 1e-9)
            {
                return Err(ErrorCode::StateConflict.into());
            }
            screens.push(to_screen(s)?.write());
        }
        groups.push(RawValue::Map(
            [
                (1, crate::config::store::wire_device(device).write()),
                (2, point(origin)),
                (3, k.write()),
                (4, RawValue::Array(screens)),
            ]
            .into(),
        ));
    }
    let s = &layout.settings;
    let value = dto::LayoutValue::from_raw(
        [
            (1, RawValue::Array(groups)),
            (2, source.write()),
            (
                4,
                RawValue::Map(
                    [
                        (6, s.gap_travel.write()),
                        (7, s.overlap_tolerance.write()),
                        (8, s.minimum_portal.write()),
                        (9, s.portal_snap.write()),
                        (10, s.walls.write()),
                        (12, s.endpoint_reach.write()),
                    ]
                    .into(),
                ),
            ),
        ]
        .into(),
    )?;
    validate(&value)?;
    Ok(value)
}
/// Read the owner-only layout inputs from a complete accepted preference
/// baseline. Missing or unsupported inputs cannot silently enable a device.
pub fn owner_preferences(
    preferences: &[dto::DevicePreference],
) -> v1::Result<oink_display::layout::OwnerLayoutPreferences> {
    use oink_display::layout::{OwnerLayoutPreferences, ScreenPreference, SpeedMode};
    fn value(preferences: &[dto::DevicePreference], option: dto::LocalOption) -> v1::Result<RawValue> {
        let mut items = preferences.iter().filter(|item| item.option() == Ok(option));
        let value = items.next().ok_or(ErrorCode::StateConflict)?.value()?;
        if items.next().is_some() {
            return Err(ErrorCode::StateConflict.into());
        }
        Ok(value)
    }
    let screen_shared = bool::read(&value(preferences, dto::LocalOption::ScreenShared)?)?;
    let screen_preferences =
        dto::ScreenPreferencesValue::read(&value(preferences, dto::LocalOption::ScreenPreferences)?)?;
    let speed_mode = match dto::SpeedMode::read(&value(preferences, dto::LocalOption::SpeedMode)?)? {
        dto::SpeedMode::Physical => SpeedMode::Physical,
        dto::SpeedMode::ScreenProportional => SpeedMode::ScreenProportional,
        _ => return Err(ErrorCode::UnsupportedValue.into()),
    };
    let mut screens = std::collections::BTreeMap::new();
    for screen in screen_preferences.screens()?.0 {
        if screens
            .insert(
                screen.display()?.0,
                ScreenPreference {
                    remote_gain: screen.remote_gain()?,
                    gain: screen.gain()?,
                    enabled: screen.enabled()?,
                },
            )
            .is_some()
        {
            return Err(ErrorCode::StateConflict.into());
        }
    }
    Ok(OwnerLayoutPreferences {
        screen_shared,
        device_gain: screen_preferences.device_gain()?,
        speed_mode,
        screens,
    })
}
pub(crate) fn same_screen(a: &dto::ScreenId, b: &dto::ScreenId) -> v1::Result<bool> {
    Ok(a.device()?.ordering_key()? == b.device()?.ordering_key()? && a.display()? == b.display()?)
}

#[derive(Clone, Debug)]
pub struct DisplaySync {
    pub layout: dto::LayoutValue,
    pub detached: super::detached::DetachedScreens,
    pub changed: bool,
}
/// Refresh only this owner's native geometry. Keep the group transform even with
/// no connected screens; reattachment uses current OS bounds, never stale canvas
/// rectangles. Detached records retain identity extensions and last native geometry.
pub fn sync_local_displays(
    current: &dto::LayoutValue,
    device: DeviceId,
    members: &[DeviceId],
    displays: &[oink_display::DisplayInfo],
    detached: &super::detached::DetachedScreens,
    now_ms: u64,
) -> v1::Result<DisplaySync> {
    validate(current)?;
    let mut groups = current.devices()?.0;
    let index = groups
        .iter()
        .position(|g| g.device().and_then(|d| native_device(&d)) == Ok(device));
    let mut memory = detached.clone();
    let old_screens = index
        .map(|i| groups[i].screens())
        .transpose()?
        .map(|s| s.0)
        .unwrap_or_default();
    for screen in &old_screens {
        if !displays
            .iter()
            .any(|d| screen.id().and_then(|id| id.display()).is_ok_and(|key| key.0 == d.key))
        {
            memory
                .remember(screen.clone(), now_ms)
                .map_err(|_| ErrorCode::UnsupportedValue)?;
        }
    }
    let mut expanded = from_wire(current)?;
    // Retained nonmembers are replicated verbatim, never executable geometry or
    // obstacles. Native observation must not normalize their stale placements.
    expanded.screens.retain(|s| members.contains(&s.id.device));
    let before = expanded.clone();
    let mut imported_transform = expanded.device_transform(device);
    expanded.sync_device(device, displays);
    if let Some(i) = index
        && old_screens.is_empty()
        && !displays.is_empty()
    {
        // Empty groups retain a transform but have no prior active geometry or
        // attachments. Import the screens, restore that saved transform, then
        // repair newly occupied space without inventing a placeholder contact.
        let origin = groups[i].origin()?;
        let manual = memory.screens.iter().any(|saved| {
            saved
                .placement
                .id()
                .and_then(|id| id.device())
                .and_then(|id| native_device(&id))
                == Ok(device)
                && saved.placement.size_source() == Ok(dto::SizeSource::Manual)
        });
        let origin = Point::new(origin.x()?, origin.y()?);
        let scale = groups[i].scale()?;
        expanded.place_device(device, origin, scale, manual);
        imported_transform = Some((origin, scale));
        let added = expanded
            .screens
            .iter()
            .filter(|s| s.id.device == device)
            .map(|s| s.id.clone())
            .collect::<Vec<_>>();
        expanded.repair_new_screens(device, &added);
    }
    // Carry colliding peers' translations into the register, retaining every
    // untouched field and exact stored origin (including nonmember extensions).
    for group in &mut groups {
        let peer = native_device(&group.device()?)?;
        if peer == device {
            continue;
        }
        if let Some(old) = before.screens.iter().find(|s| s.id.device == peer)
            && let Some(next) = expanded.screen(&old.id)
        {
            let dx = next.rect.x - old.rect.x;
            let dy = next.rect.y - old.rect.y;
            if dx != 0.0 || dy != 0.0 {
                let origin = group.origin()?;
                let mut raw = group.raw().clone();
                raw.insert(2, point(Point::new(origin.x()? + dx, origin.y()? + dy)));
                *group = dto::DevicePlacement::from_raw(raw)?;
            }
        }
    }
    let generated = to_wire(&expanded, current.source()?)?;
    let mut group = match generated
        .devices()?
        .0
        .into_iter()
        .find(|g| g.device().and_then(|d| native_device(&d)) == Ok(device))
    {
        Some(group) => group,
        None if index.is_some() => groups[index.expect("existing group")].clone(),
        None => {
            return Ok(DisplaySync {
                layout: current.clone(),
                detached: memory,
                changed: false,
            });
        }
    };
    let mut raw = group.raw().clone();
    if let Some(i) = index {
        // Recover only the actual repair translation, using the same expansion
        // formula as sync_device. Re-inferring a stored origin from rectangles
        // with negative native offsets can otherwise drift by an ulp on every
        // observation. An unchanged import must retain the exact stored anchor.
        let stored = groups[i].origin()?;
        let mut origin = stored.write();
        if let Some((baseline, scale)) = imported_transform
            && let Some(screen) = expanded.screens.iter().find(|s| s.id.device == device)
        {
            let dx = screen.rect.x - (baseline.x + screen.local.x * scale);
            let dy = screen.rect.y - (baseline.y + screen.local.y * scale);
            if dx != 0.0 || dy != 0.0 {
                origin = point(Point::new(stored.x()? + dx, stored.y()? + dy));
            }
        }
        raw.insert(2, origin);
        raw.insert(3, groups[i].scale()?.write());
    }
    let mut records = Vec::new();
    for screen in group.screens()?.0.into_iter().filter(|_| !displays.is_empty()) {
        let id = screen.id()?;
        let saved = old_screens
            .iter()
            .find(|s| s.id().is_ok_and(|other| same_screen(&id, &other) == Ok(true)))
            .or_else(|| memory.find(&id));
        let mut retained = saved.map(|s| s.raw().clone()).unwrap_or_default();
        for (key, value) in screen.raw() {
            if *key != 1 || !retained.contains_key(key) {
                retained.insert(*key, value.clone());
            }
        }
        records.push(RawValue::Map(retained));
    }
    raw.insert(4, RawValue::Array(records));
    group = dto::DevicePlacement::from_raw(raw)?;
    if let Some(i) = index {
        groups[i] = group;
    } else {
        groups.push(group);
    }
    let mut raw = current.raw().clone();
    raw.insert(1, RawValue::Array(groups.iter().map(WireValue::write).collect()));
    // Native observations are automatic even when the retained layout was
    // originally authored by a manual edit. An unchanged refresh stays exact.
    if raw != *current.raw() {
        raw.insert(2, dto::LayoutSource::Default.write());
    }
    let layout = dto::LayoutValue::from_raw(raw)?;
    validate(&layout)?;
    Ok(DisplaySync {
        changed: layout != *current,
        layout,
        detached: memory,
    })
}
fn fields(current: &mut RawMap, before: &RawMap, after: &RawMap, known: &[u32]) {
    for key in known {
        if before.get(key) != after.get(key) {
            match after.get(key) {
                Some(v) => {
                    current.insert(*key, v.clone());
                }
                None => {
                    current.remove(key);
                }
            }
        }
    }
}
/// Patch known geometry and preserve stable identity extensions. Empty groups
/// survive edits so unplugging the last display never forgets device placement.
pub fn patch(current: &mut RawMap, before: &dto::LayoutValue, after: &dto::LayoutValue) -> v1::Result<()> {
    validate(before)?;
    validate(after)?;
    let latest = dto::LayoutValue::from_raw(current.clone())?;
    validate(&latest)?;
    let old = before.devices()?.0;
    let edited = after.devices()?.0;
    let mut groups = Vec::new();
    for group in latest.devices()?.0 {
        let key = group.device()?.ordering_key()?;
        let prior = old
            .iter()
            .find(|g| g.device().and_then(|d| d.ordering_key()) == Ok(key.clone()));
        let next = edited
            .iter()
            .find(|g| g.device().and_then(|d| d.ordering_key()) == Ok(key.clone()));
        if let Some(next) = next {
            let mut raw = group.raw().clone();
            if let Some(prior) = prior {
                fields(&mut raw, prior.raw(), next.raw(), &[2, 3]);
            }
            let old_screens = group.screens()?.0;
            let screens = next
                .screens()?
                .0
                .into_iter()
                .map(|screen| {
                    let id = screen.id()?;
                    let mut raw = old_screens
                        .iter()
                        .find(|s| s.id().is_ok_and(|other| same_screen(&id, &other) == Ok(true)))
                        .map(|s| s.raw().clone())
                        .unwrap_or_default();
                    for (key, value) in screen.raw() {
                        if *key != 1 || !raw.contains_key(key) {
                            raw.insert(*key, value.clone());
                        }
                    }
                    Ok(RawValue::Map(raw))
                })
                .collect::<v1::Result<Vec<_>>>()?;
            raw.insert(4, RawValue::Array(screens));
            groups.push(RawValue::Map(raw));
        } else if prior.is_none() {
            groups.push(group.write());
        }
    }
    for group in edited {
        if !latest
            .devices()?
            .0
            .iter()
            .any(|g| g.device().and_then(|d| d.ordering_key()) == group.device().and_then(|d| d.ordering_key()))
        {
            groups.push(group.write());
        }
    }
    let mut raw = current.clone();
    raw.insert(1, RawValue::Array(groups));
    fields(&mut raw, before.raw(), after.raw(), &[2, 4]);
    validate(&dto::LayoutValue::from_raw(raw.clone())?)?;
    *current = raw;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotplug_preserves_owner_and_noncolliders_in_recorded_three_computer_layout() {
        // Diagnostic layout revisions 117 -> 118, 2026-10-23. Identities and
        // display names are synthetic; all native and canvas coordinates are exact.
        let retired_laptop = DeviceId([11; 32]);
        let above = DeviceId([22; 32]);
        let retired_large = DeviceId([33; 32]);
        let changed = DeviceId([44; 32]);
        let unrelated = DeviceId([55; 32]);
        let mut builtin = display("builtin", 0.0, 0.0);
        builtin.bounds = Rect::new(0.0, 0.0, 1728.0, 1117.0);
        builtin.pixel_width = 3456;
        builtin.pixel_height = 2234;
        builtin.is_primary = true;
        builtin.is_builtin = true;
        let mut external = display("external", 1496.0, -1080.0);
        external.bounds = Rect::new(1496.0, -1080.0, 1920.0, 1080.0);
        external.pixel_width = 1920;
        external.pixel_height = 1080;
        let mut large = display("large", 0.0, 0.0);
        large.bounds = Rect::new(0.0, 0.0, 3840.0, 2160.0);
        large.pixel_width = 3840;
        large.pixel_height = 2160;
        large.is_primary = true;
        let mut layout = Layout::auto(&[
            (changed, vec![builtin.clone()]),
            (above, vec![large.clone()]),
            (unrelated, vec![builtin.clone()]),
            (retired_large, vec![large]),
            (retired_laptop, vec![builtin.clone()]),
        ]);
        let scale = 0.5018144325252422 / 1728.0;
        layout.place_device(
            changed,
            Point::new(0.19628255740738265, -1.1102230246251565e-16),
            scale,
            true,
        );
        layout.place_device(
            above,
            Point::new(0.654165322276049, -0.4195967062023351),
            0.7459496999152622 / 3840.0,
            true,
        );
        layout.place_device(
            unrelated,
            Point::new(0.8192865465286432, 0.02796160827244118),
            scale,
            true,
        );
        // The register also retained two old, no-longer-member computers. They
        // must never become obstacles or cause cascading movement of live ones.
        layout.place_device(
            retired_large,
            Point::new(0.6060010288883654, -0.4591005033232923),
            0.8161786725747416 / 3840.0,
            true,
        );
        layout.place_device(
            retired_laptop,
            Point::new(0.8047854579208121, 0.024531430265244347),
            scale,
            true,
        );
        let wire = to_wire(&layout, dto::LayoutSource::Manual).unwrap();
        let update = sync_local_displays(
            &wire,
            changed,
            &[changed, above, unrelated],
            &[builtin.clone(), external.clone()],
            &Default::default(),
            1,
        )
        .unwrap();
        let repaired = from_wire(&update.layout).unwrap();
        for device in [unrelated, retired_large, retired_laptop] {
            assert_eq!(
                repaired.device_transform(device),
                layout.device_transform(device),
                "a native hotplug must not relocate another computer"
            );
            let before = wire
                .devices()
                .unwrap()
                .0
                .into_iter()
                .find(|g| native_device(&g.device().unwrap()) == Ok(device))
                .unwrap();
            let after = update
                .layout
                .devices()
                .unwrap()
                .0
                .into_iter()
                .find(|g| native_device(&g.device().unwrap()) == Ok(device))
                .unwrap();
            assert_eq!(
                after, before,
                "other owners' retained register records must be byte-for-byte unchanged"
            );
        }
        let mut active = repaired.clone();
        active
            .screens
            .retain(|s| [changed, above, unrelated].contains(&s.id.device));
        assert!(active.overlaps().is_empty());
        let own = repaired.screen(&ScreenId::new(changed, "builtin")).unwrap();
        let new = repaired.screen(&ScreenId::new(changed, "external")).unwrap();
        assert!((new.rect.x - own.rect.x - 1496.0 * scale).abs() < 1e-12);
        assert!((new.rect.y - own.rect.y + 1080.0 * scale).abs() < 1e-12);
        assert_eq!(
            own.rect,
            layout.screen(&ScreenId::new(changed, "builtin")).unwrap().rect
        );
        assert_eq!(repaired.device_transform(changed), layout.device_transform(changed));
        assert_ne!(repaired.device_transform(above), layout.device_transform(above));
        let repeat = sync_local_displays(
            &update.layout,
            changed,
            &[changed, above, unrelated],
            &[builtin, external],
            &update.detached,
            2,
        )
        .unwrap();
        assert_eq!(from_wire(&repeat.layout).unwrap().screens, repaired.screens);
    }

    fn display(key: &str, x: f64, y: f64) -> oink_display::DisplayInfo {
        oink_display::DisplayInfo {
            key: key.into(),
            name: key.into(),
            bounds: Rect::new(x, y, 1000.0, 600.0),
            pixel_width: 2000,
            pixel_height: 1200,
            reference_extent: Some(oink_foundation::Size::new(1.0, 0.6)),
            rotation_deg: 0,
            is_primary: key == "a",
            is_builtin: false,
            is_virtual: false,
        }
    }
    fn fixture() -> (DeviceId, dto::LayoutValue) {
        let device = DeviceId([11; 32]);
        let mut layout = Layout::auto(&[(device, vec![display("a", 0.0, 0.0), display("b", -1000.0, 100.0)])]);
        let (origin, scale) = layout.device_transform(device).unwrap();
        layout.place_device(device, origin, scale, true);
        (device, to_wire(&layout, dto::LayoutSource::Manual).unwrap())
    }
    #[test]
    fn native_refresh_preserves_exact_stored_origin_when_first_screen_has_negative_bounds() {
        let (device, wire) = fixture();
        let mut groups = wire.devices().unwrap().0;
        let mut group = groups[0].raw().clone();
        let mut screens = groups[0].screens().unwrap().0;
        screens.reverse();
        group.insert(4, RawValue::Array(screens.iter().map(WireValue::write).collect()));
        group.insert(2, point(Point::new(0.1, 0.2)));
        groups[0] = dto::DevicePlacement::from_raw(group).unwrap();
        let mut raw = wire.raw().clone();
        raw.insert(1, RawValue::Array(groups.iter().map(WireValue::write).collect()));
        let wire = dto::LayoutValue::from_raw(raw).unwrap();
        let refreshed = sync_local_displays(
            &wire,
            device,
            &[device],
            &[display("b", -1000.0, 100.0), display("a", 0.0, 0.0)],
            &Default::default(),
            1,
        )
        .unwrap();
        assert_eq!(
            refreshed.layout, wire,
            "expanded rectangle roundoff must not move the persisted native anchor"
        );
        let changed = sync_local_displays(
            &wire,
            device,
            &[device],
            &[display("b", -1000.0, -400.0), display("a", 0.0, 0.0)],
            &Default::default(),
            2,
        )
        .unwrap();
        assert_eq!(changed.layout.source().unwrap(), dto::LayoutSource::Default);
        assert_eq!(
            changed.layout.devices().unwrap().0[0].origin().unwrap(),
            wire.devices().unwrap().0[0].origin().unwrap()
        );
    }

    #[test]
    fn register_contains_one_transform_and_no_per_screen_canvas_rectangles() {
        let (_, value) = fixture();
        let group = &value.devices().unwrap().0[0];
        assert_eq!(group.screens().unwrap().0.len(), 2);
        assert!(group.screens().unwrap().0.iter().all(|s| !s.raw().contains_key(&4)));
        let expanded = from_wire(&value).unwrap();
        assert_eq!(to_wire(&expanded, dto::LayoutSource::Manual).unwrap(), value);
        let mut invalid = expanded;
        invalid.screens[1].rect.x += 1.0;
        assert_eq!(
            to_wire(&invalid, dto::LayoutSource::Manual).unwrap_err().code,
            ErrorCode::StateConflict
        );
    }
    #[test]
    fn unplug_all_and_reattach_preserves_group_origin_scale_and_identity_extensions() {
        let (device, value) = fixture();
        let mut raw = value.raw().clone();
        let RawValue::Array(groups) = raw.get_mut(&1).unwrap() else {
            panic!()
        };
        let RawValue::Map(group) = &mut groups[0] else { panic!() };
        let RawValue::Array(screens) = group.get_mut(&4).unwrap() else {
            panic!()
        };
        let RawValue::Map(screen) = &mut screens[1] else {
            panic!()
        };
        let RawValue::Map(id) = screen.get_mut(&1).unwrap() else {
            panic!()
        };
        id.insert(99, RawValue::from("retained"));
        let value = dto::LayoutValue::from_raw(raw).unwrap();
        let original = value.devices().unwrap().0[0].clone();
        let removed = sync_local_displays(&value, device, &[device], &[], &Default::default(), 1).unwrap();
        assert_eq!(removed.detached.screens.len(), 2);
        assert!(removed.layout.devices().unwrap().0[0].screens().unwrap().0.is_empty());
        let reattached = sync_local_displays(
            &removed.layout,
            device,
            &[device],
            &[display("b", 200.0, -600.0)],
            &removed.detached,
            2,
        )
        .unwrap();
        let group = reattached.layout.devices().unwrap().0[0].clone();
        assert_eq!(group.origin().unwrap(), original.origin().unwrap());
        assert_eq!(group.scale().unwrap(), original.scale().unwrap());
        let screen = group.screens().unwrap().0[0].clone();
        assert_eq!(screen.id().unwrap().raw().get(&99), Some(&RawValue::from("retained")));
        assert_eq!(screen.size_source().unwrap(), dto::SizeSource::Manual);
        assert_eq!(screen.local().unwrap().x().unwrap(), 200.0);
        assert_eq!(screen.local().unwrap().y().unwrap(), -600.0);
    }
    #[test]
    fn reattaching_to_an_empty_group_still_repairs_newly_occupied_geometry() {
        let a = DeviceId([11; 32]);
        let b = DeviceId([12; 32]);
        let info = display("a", 0.0, 0.0);
        let mut layout = Layout::auto(&[(a, vec![info.clone()]), (b, vec![info.clone()])]);
        layout.place_device(a, Point::default(), 1.0, true);
        layout.place_device(b, Point::new(1000.0, 0.0), 1.0, true);
        let wire = to_wire(&layout, dto::LayoutSource::Manual).unwrap();
        let removed = sync_local_displays(&wire, a, &[a, b], &[], &Default::default(), 1).unwrap();
        let mut raw = removed.layout.raw().clone();
        let mut groups = removed.layout.devices().unwrap().0;
        let index = groups
            .iter()
            .position(|g| native_device(&g.device().unwrap()).unwrap() == b)
            .unwrap();
        let mut occupied = groups[index].raw().clone();
        occupied.insert(2, point(Point::default()));
        groups[index] = dto::DevicePlacement::from_raw(occupied).unwrap();
        raw.insert(1, RawValue::Array(groups.iter().map(WireValue::write).collect()));
        let occupied = dto::LayoutValue::from_raw(raw).unwrap();
        let restored = sync_local_displays(&occupied, a, &[a, b], &[info], &removed.detached, 2).unwrap();
        let expanded = from_wire(&restored.layout).unwrap();
        assert_eq!(expanded.device_transform(a).unwrap().0, Point::default());
        assert_eq!(expanded.device_transform(b).unwrap().0, Point::new(0.0, -600.0));
        assert!(expanded.overlaps().is_empty());
    }
    #[test]
    fn live_native_rearrangement_keeps_native_origin_and_one_scale() {
        let (device, value) = fixture();
        let before = value.devices().unwrap().0[0].clone();
        let update = sync_local_displays(
            &value,
            device,
            &[device],
            &[display("a", 0.0, 0.0), display("b", 1000.0, -200.0)],
            &Default::default(),
            1,
        )
        .unwrap();
        let after = update.layout.devices().unwrap().0[0].clone();
        assert_eq!(before.origin().unwrap(), after.origin().unwrap());
        assert_eq!(before.scale().unwrap(), after.scale().unwrap());
        let layout = from_wire(&update.layout).unwrap();
        let a = &layout.screens[0];
        let b = &layout.screens[1];
        assert!((b.rect.y - a.rect.y + 200.0 * after.scale().unwrap()).abs() < 1e-12);
        let owners = std::collections::BTreeMap::from([(
            device,
            oink_display::layout::OwnerLayoutPreferences {
                screens: std::collections::BTreeMap::from([(
                    "b".into(),
                    oink_display::layout::ScreenPreference {
                        remote_gain: 1.0,
                        enabled: false,
                        gain: 1.0,
                    },
                )]),
                ..Default::default()
            },
        )]);
        let effective = layout.effective(&owners);
        assert_eq!(effective.screens()[1].rect, b.rect);
        assert!(!effective.screens()[1].enabled);
    }
}
