//! One arrangement description for durable catalog content and ephemeral previews.
//! Conversion is pure: neither rendering nor validation publishes owner facts.
use super::{model, types::*};
use crate::api::{DeviceKey, Placement, ScreenKey};
use oink_display::layout::{Layout, OwnerLayoutPreferences};
use oink_foundation::{DeviceId, Point, Rect, Size};
use std::collections::{BTreeMap, BTreeSet};

pub type Owners = BTreeMap<DeviceId, OwnerLayoutPreferences>;

pub fn capture(layout: &Layout, owners: &Owners) -> ArrangementSnapshot {
    let mut snapshot = ArrangementSnapshot {
        placements: Vec::new(),
        disabled: Vec::new(),
        screen_shared: Vec::new(),
        native: Vec::new(),
        topology: layout.settings.clone(),
        active_profile_id: String::new(),
    };
    for id in layout.screens.iter().map(|s| s.id.device).collect::<BTreeSet<_>>() {
        let Some((origin, scale)) = layout.device_transform(id) else {
            continue;
        };
        let device = DeviceKey(id);
        snapshot.placements.push(Placement { device, origin, scale });
        let owner = owners.get(&id).cloned().unwrap_or_default();
        snapshot.screen_shared.push(DeviceSharing {
            device,
            shared: owner.screen_shared,
        });
        let screens = layout.screens.iter().filter(|s| s.id.device == id).collect::<Vec<_>>();
        let primary = screens.iter().find(|s| s.primary).unwrap_or(&screens[0]);
        snapshot.native.push(NativeState {
            device,
            token: String::new(),
            primary: ScreenKey::from(&primary.id),
            screens: screens
                .iter()
                .map(|s| NativeScreen {
                    screen: ScreenKey::from(&s.id),
                    origin: Point::new(s.local.x, s.local.y),
                    size: Size::new(s.local.w, s.local.h),
                })
                .collect(),
        });
        snapshot.disabled.extend(screens.into_iter().map(|s| Disabled {
            screen: ScreenKey::from(&s.id),
            disabled: !owner.screens.get(&s.id.display).is_none_or(|p| p.enabled),
        }));
    }
    model::canonicalize(&mut snapshot);
    snapshot
}

/// Both the live arrangement and a preview reach the projection renderer here.
/// A preview can reposition native origins, but cannot invent a display or mode.
pub fn render(snapshot: &ArrangementSnapshot, base: &Layout, owners: &Owners) -> Result<(Layout, Owners)> {
    model::validate_snapshot(snapshot)?;
    let invalid = || ProfileError::new(ErrorCode::InvalidGeometry, "Stale or invalid arrangement preview");
    let live = capture(base, owners);
    if snapshot.native.len() != live.native.len() {
        return Err(invalid());
    }
    for n in &snapshot.native {
        let current = live.native.iter().find(|c| c.device == n.device).ok_or_else(invalid)?;
        if n.primary != current.primary
            || n.screens.len() != current.screens.len()
            || n.screens
                .iter()
                .any(|s| !current.screens.iter().any(|c| c.screen == s.screen && c.size == s.size))
        {
            return Err(invalid());
        }
        if !model::same_native(n, current) {
            oink_display::arrangement::validate_rectangles(&model::native_rects(n)).map_err(|_| invalid())?;
        }
    }
    let mut layout = base.clone();
    let mut owners = owners.clone();
    layout.settings = snapshot.topology.clone();
    for n in &snapshot.native {
        let p = snapshot
            .placements
            .iter()
            .find(|p| p.device == n.device)
            .ok_or_else(invalid)?;
        let owner = owners.entry(n.device.0).or_default();
        owner.screen_shared = model::screen_shared(snapshot, n.device);
        for native in &n.screens {
            let s = layout
                .screens
                .iter_mut()
                .find(|s| ScreenKey::from(&s.id) == native.screen)
                .ok_or_else(invalid)?;
            s.local = Rect::new(native.origin.x, native.origin.y, native.size.w, native.size.h);
            s.rect = Rect::new(
                p.origin.x + native.origin.x * p.scale,
                p.origin.y + native.origin.y * p.scale,
                native.size.w * p.scale,
                native.size.h * p.scale,
            );
            owner.screens.entry(native.screen.display.clone()).or_default().enabled = !snapshot
                .disabled
                .iter()
                .find(|b| b.screen == native.screen)
                .ok_or_else(invalid)?
                .disabled;
        }
    }
    Ok((layout, owners))
}

pub fn valid_preview(snapshot: &ArrangementSnapshot) -> bool {
    model::validate_snapshot(snapshot).is_ok()
        && snapshot.placements.iter().all(|p| {
            snapshot
                .native
                .iter()
                .filter(|n| n.device == p.device)
                .flat_map(|n| &n.screens)
                .all(|s| {
                    let x = p.origin.x + s.origin.x * p.scale;
                    let y = p.origin.y + s.origin.y * p.scale;
                    x.is_finite()
                        && y.is_finite()
                        && x.abs() <= 1_000_000.0 / 686.0
                        && y.abs() <= 1_000_000.0 / 686.0
                        && s.size.w * p.scale <= 4000.0 / 686.0
                        && s.size.h * p.scale <= 4000.0 / 686.0
                })
        })
}
