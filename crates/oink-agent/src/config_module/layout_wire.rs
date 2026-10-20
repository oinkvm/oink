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
    if let Some(i) = index
        && old_screens.is_empty()
        && let Some(info) = displays.first()
    {
        expanded
            .screens
            .push(ScreenPlacement::from_display(device, info, Point::default()));
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
        expanded.place_device(device, Point::new(origin.x()?, origin.y()?), groups[i].scale()?, manual);
    }
    expanded.sync_device(device, displays);
    let generated = to_wire(&expanded, current.source()?)?;
    for existing in &mut groups {
        if existing.device().and_then(|d| native_device(&d)) == Ok(device) {
            continue;
        }
        if let Some(next) = generated.devices()?.0.iter().find(|g| g.device() == existing.device()) {
            let mut raw = existing.raw().clone();
            raw.insert(2, next.origin()?.write());
            *existing = dto::DevicePlacement::from_raw(raw)?;
        }
    }
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
        raw.insert(2, groups[i].origin()?.write());
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
        let removed = sync_local_displays(&value, device, &[], &Default::default(), 1).unwrap();
        assert_eq!(removed.detached.screens.len(), 2);
        assert!(removed.layout.devices().unwrap().0[0].screens().unwrap().0.is_empty());
        let reattached = sync_local_displays(
            &removed.layout,
            device,
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
    fn live_native_rearrangement_keeps_native_origin_and_one_scale() {
        let (device, value) = fixture();
        let before = value.devices().unwrap().0[0].clone();
        let update = sync_local_displays(
            &value,
            device,
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
