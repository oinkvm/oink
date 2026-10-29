//! Manual deltas are applied to saved content, not to an automatic live projection.
use super::{
    model::{self, Catalog, DeviceState},
    types::*,
};
use crate::api::{DeviceKey, ScreenKey};

// A newly connected screen enters saved content only when explicitly edited.
fn include_screen(content: &mut ArrangementSnapshot, actual: &ArrangementSnapshot, screen: &ScreenKey) {
    include_device(content, actual, screen.device);
    if let Some(native) = content.native.iter_mut().find(|n| n.device == screen.device)
        && !native.screens.iter().any(|s| &s.screen == screen)
        && let Some(s) = actual
            .native
            .iter()
            .flat_map(|n| &n.screens)
            .find(|s| &s.screen == screen)
    {
        native.screens.push(s.clone());
    }
    if !content.disabled.iter().any(|b| &b.screen == screen)
        && let Some(bit) = actual.disabled.iter().find(|b| &b.screen == screen)
    {
        content.disabled.push(bit.clone());
    }
}
fn include_device(content: &mut ArrangementSnapshot, actual: &ArrangementSnapshot, device: DeviceKey) {
    if content.placements.iter().any(|p| p.device == device) {
        return;
    }
    if let Some(p) = actual.placements.iter().find(|p| p.device == device) {
        content.placements.push(p.clone());
    }
    if let Some(n) = actual.native.iter().find(|n| n.device == device) {
        content.native.push(n.clone());
    }
    content
        .disabled
        .extend(actual.disabled.iter().filter(|b| b.screen.device == device).cloned());
    model::set_screen_shared(content, device, model::screen_shared(actual, device));
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ManualSave {
    pub profile_id: String,
    pub intended: ArrangementSnapshot,
    pub applied: bool,
}

impl Catalog {
    pub fn autosave_manual(
        &self,
        writer: DeviceKey,
        profile_id: &str,
        before: &ArrangementSnapshot,
        intended: &ArrangementSnapshot,
        actual: &ArrangementSnapshot,
        now: u64,
    ) -> Result<Self> {
        let mut content = self.profile(profile_id)?.content.clone();
        let mut changed = false;
        for after in &actual.placements {
            let Some(old) = before.placements.iter().find(|p| p.device == after.device) else {
                continue;
            };
            let Some(want) = intended.placements.iter().find(|p| p.device == after.device) else {
                continue;
            };
            if old == want || old == after {
                continue;
            }
            let x = old.origin.x != want.origin.x && model::close(want.origin.x, after.origin.x);
            let y = old.origin.y != want.origin.y && model::close(want.origin.y, after.origin.y);
            let scale = old.scale != want.scale && model::close(want.scale, after.scale);
            if !x && !y && !scale {
                continue;
            }
            include_device(&mut content, actual, after.device);
            let saved = content
                .placements
                .iter_mut()
                .find(|p| p.device == after.device)
                .expect("inserted");
            // A later factual repair is not evidence that this manual field
            // committed (notably when recovering an interrupted operation).
            if x {
                saved.origin.x = after.origin.x;
            }
            if y {
                saved.origin.y = after.origin.y;
            }
            if scale {
                saved.scale = after.scale;
            }
            changed = true;
        }
        for after in &actual.disabled {
            let old = before.disabled.iter().find(|p| p.screen == after.screen);
            let want = intended.disabled.iter().find(|p| p.screen == after.screen);
            if old == want || old == Some(after) || want.is_none() {
                continue;
            }
            include_screen(&mut content, actual, &after.screen);
            if let Some(saved) = content.disabled.iter_mut().find(|p| p.screen == after.screen) {
                *saved = after.clone();
                changed = true;
            }
        }
        for after in &actual.native {
            let Some(old) = before.native.iter().find(|p| p.device == after.device) else {
                continue;
            };
            let Some(want) = intended.native.iter().find(|p| p.device == after.device) else {
                continue;
            };
            for screen in &after.screens {
                if let (Some(old), Some(want)) = (
                    old.screens.iter().find(|s| s.screen == screen.screen),
                    want.screens.iter().find(|s| s.screen == screen.screen),
                ) && ((old.origin != want.origin && want.origin == screen.origin)
                    || (old.size != want.size && want.size == screen.size))
                {
                    include_screen(&mut content, actual, &screen.screen);
                }
            }
            let Some(saved) = content.native.iter_mut().find(|p| p.device == after.device) else {
                continue;
            };
            for screen in &after.screens {
                let Some(old) = old.screens.iter().find(|s| s.screen == screen.screen) else {
                    continue;
                };
                let Some(want) = want.screens.iter().find(|s| s.screen == screen.screen) else {
                    continue;
                };
                let Some(saved) = saved.screens.iter_mut().find(|s| s.screen == screen.screen) else {
                    continue;
                };
                if old.origin != want.origin && want.origin == screen.origin {
                    saved.origin = screen.origin;
                    changed = true;
                }
                if old.size != want.size && want.size == screen.size {
                    saved.size = screen.size;
                    changed = true;
                }
            }
        }
        for device in actual.placements.iter().map(|p| p.device) {
            let old = model::screen_shared(before, device);
            let want = model::screen_shared(intended, device);
            let shared = model::screen_shared(actual, device);
            if old != want && old != shared {
                include_device(&mut content, actual, device);
                model::set_screen_shared(&mut content, device, shared);
                changed = true;
            }
        }
        if before.topology != intended.topology && intended.topology == actual.topology {
            content.topology = actual.topology.clone();
            changed = true;
        }
        if !changed {
            return Ok(self.clone());
        }
        let mut selected = self.clone();
        selected.active_profile_id = profile_id.into();
        let state = DeviceState {
            online_devices: self.profile(profile_id)?.match_state.online_devices.clone(),
            connected_screens: content
                .native
                .iter()
                .map(|n| {
                    (
                        n.device,
                        Some(n.screens.iter().map(|s| s.screen.display.clone()).collect()),
                    )
                })
                .collect(),
        };
        let mut next = selected.save(writer, &content, state, None, now)?;
        next.active_profile_id = self.active_profile_id.clone();
        Ok(next)
    }
}
