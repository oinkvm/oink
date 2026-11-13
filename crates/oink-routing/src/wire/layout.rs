//! Shared layout decoding for desktop and mobile routing. Geometry and visibility
//! come from the group register; input gains remain device-owned facts.
use oink_display::layout::{Layout, LayoutSettings, ScreenId, ScreenPlacement, SizeSource};
use oink_foundation::{DeviceId, Rect};
use oink_protocol::v2::{
    self, ErrorCode, RawValue, dto,
    schema::{WireDto, WireValue},
};
fn native_device(device: &dto::DeviceId) -> v2::Result<DeviceId> {
    Ok(DeviceId(
        device
            .public_key()?
            .0
            .try_into()
            .map_err(|_| ErrorCode::UnsupportedValue)?,
    ))
}
pub fn from_wire(value: &dto::LayoutValue) -> v2::Result<Layout> {
    v2::layout::require_supported_geometry(value)?;
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
                device_hidden: group.hidden()?,
                screen_hidden: screen.hidden()?,
                device_visibility_known: group.raw().contains_key(&5),
                screen_visibility_known: screen.raw().contains_key(&11),
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
/// Read device-owned gains from a complete preference baseline.
pub fn owner_preferences(
    preferences: &[dto::DevicePreference],
) -> v2::Result<oink_display::layout::OwnerLayoutPreferences> {
    use oink_display::layout::{OwnerLayoutPreferences, ScreenPreference, SpeedMode};
    fn value(preferences: &[dto::DevicePreference], option: dto::LocalOption) -> v2::Result<RawValue> {
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

/// Compile the same shared geometry on a phone as on a PC. Callers supply
/// presence separately; absence of PC-only gain preferences on Android uses
/// unit gains, while global visibility remains in the layout itself.
pub fn topology(
    value: &dto::LayoutValue,
    facts: &std::collections::BTreeMap<DeviceId, dto::DeviceFacts>,
    online: impl Fn(&DeviceId) -> bool,
) -> v2::Result<crate::TopologyLayout> {
    let layout = from_wire(value)?;
    let mut owners = std::collections::BTreeMap::new();
    for (device, facts) in facts {
        if facts.os()?.0 == "android" {
            owners.insert(*device, Default::default());
        } else if let Some(preferences) = facts.preferences()?
            && let Ok(owner) = owner_preferences(&preferences.0)
        {
            owners.insert(*device, owner);
        }
    }
    Ok(crate::TopologyLayout::from_effective(
        &layout.effective(&owners),
        online,
    ))
}
