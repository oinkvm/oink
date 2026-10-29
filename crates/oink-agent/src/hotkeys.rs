//! 全局快捷键的接入：把界面保存的快捷键交给路由识别，执行路由识别到的动作。
//!
//! 识别在路由里（截获回调的快路径，物理键盘所在的设备上，不论焦点在哪台）：匹配后这组按键吞掉，路由发出
//! `StatusEvent::Hotkey { action }`，这里按动作名执行。Windows 上服务在独立进程里，钩子、路由与这里都在服务
//! The Windows open action checks the real UIAccess token, grants the UI permission, then posts its native open message.

use oink_foundation::DeviceId;
use oink_routing::{Chord, HotkeyBinding};
use std::collections::BTreeMap;

use crate::api::{AgentApi, ApiEvent, Hotkey, HotkeyAction};
use crate::node::Node;

/// Missing configuration uses defaults; saved overrides retain their exact list.
pub(crate) fn configured(saved: Option<Vec<Hotkey>>) -> Vec<Hotkey> {
    saved.unwrap_or_else(crate::agent_api::default_hotkeys)
}

/// Slot indices 0..9 correspond to digits 1..9,0. OFF has no assigned key.
fn defaults(slots: &BTreeMap<DeviceId, Option<u8>>) -> Vec<Hotkey> {
    let mut slots = slots.iter().collect::<Vec<_>>();
    slots.sort_by_key(|(device, slot)| (slot.unwrap_or(10), **device));
    slots
        .into_iter()
        .map(|(device, slot)| Hotkey {
            action: HotkeyAction::MovePointerTo,
            device: Some((*device).into()),
            enabled: slot.is_some(),
            keys: slot
                .map(|s| vec!["Ctrl".into(), "Alt".into(), ((s + 1) % 10).to_string()])
                .unwrap_or_default(),
        })
        .collect()
}
fn action_name(h: &Hotkey) -> String {
    if h.action == HotkeyAction::MovePointerTo
        && let Some(device) = h.device
    {
        return format!("move_pointer_to:{}", device.0.to_hex());
    }
    h.action.as_str().into()
}
pub(crate) fn effective(saved: Option<Vec<Hotkey>>, slots: &BTreeMap<DeviceId, Option<u8>>) -> Vec<Hotkey> {
    let mut saved = configured(saved);
    // New actions are visible at runtime without changing the saved override list.
    for action in [HotkeyAction::LockEscape, HotkeyAction::ToggleMicrophoneMute] {
        if !saved.iter().any(|h| h.action == action) {
            saved.push(
                crate::agent_api::default_hotkeys()
                    .into_iter()
                    .find(|h| h.action == action)
                    .unwrap(),
            );
        }
    }
    let mut list: Vec<_> = saved
        .iter()
        .filter(|h| h.action != HotkeyAction::MovePointerTo)
        .cloned()
        .collect();
    for default in defaults(slots) {
        list.push(
            saved
                .iter()
                .find(|h| h.action == default.action && h.device == default.device)
                .cloned()
                .unwrap_or(default),
        );
    }
    list
}
/// Projected defaults are never persisted as overrides by an unrelated edit.
/// Existing local overrides, including departed devices, are retained locally.
pub(crate) fn save_overrides(
    incoming: &[Hotkey],
    saved: Option<Vec<Hotkey>>,
    slots: &BTreeMap<DeviceId, Option<u8>>,
) -> Vec<Hotkey> {
    let saved = configured(saved);
    let defaults = defaults(slots);
    let mut out: Vec<_> = incoming
        .iter()
        .filter(|h| h.action != HotkeyAction::MovePointerTo)
        .cloned()
        .collect();
    out.extend(
        saved
            .iter()
            .filter(|h| {
                h.action == HotkeyAction::MovePointerTo
                    && !incoming.iter().any(|i| i.action == h.action && i.device == h.device)
            })
            .cloned(),
    );
    out.extend(
        incoming
            .iter()
            .filter(|h| {
                h.action == HotkeyAction::MovePointerTo
                    && (saved.iter().any(|old| old.action == h.action && old.device == h.device)
                        || !defaults.contains(h))
            })
            .cloned(),
    );
    out
}

/// 保存前检查：启用的快捷键键名有效、互不相同。
pub(crate) fn validate(list: &[Hotkey]) -> Result<(), String> {
    for h in list {
        if (h.action == HotkeyAction::MovePointerTo) != h.device.is_some() {
            return Err("Move-pointer shortcuts require a target device".into());
        }
    }
    for h in list.iter().filter(|h| h.action == HotkeyAction::LockEscape) {
        oink_routing::hotkey::single_key(&h.keys).map_err(|e| e.to_string())?;
    }
    let mut seen = Vec::new();
    for h in list
        .iter()
        .filter(|h| h.enabled && !h.keys.is_empty() && h.action != HotkeyAction::LockEscape)
    {
        let chord = Chord::parse(&h.keys).map_err(|e| format!("快捷键 {}：{e}", h.keys.join("+")))?;
        if seen.contains(&chord) {
            return Err("两个快捷键不能相同".into());
        }
        seen.push(chord);
    }
    Ok(())
}

/// 启用且键名有效的快捷键 → 路由的绑定。无效的（键名无法识别、与紧急暂停相同）记 warn 并跳过；
/// 两条相同的组合键只保留第一条（界面与 API 都会拒绝保存这种设置）。
pub(crate) fn bindings(list: &[Hotkey]) -> Vec<HotkeyBinding> {
    let mut out: Vec<HotkeyBinding> = Vec::new();
    for h in list.iter().filter(|h| h.enabled && !h.keys.is_empty()) {
        if h.action == HotkeyAction::LockEscape {
            if let Ok(key) = oink_routing::hotkey::single_key(&h.keys) {
                out.push(HotkeyBinding {
                    action: "lock_escape".into(),
                    chord: Chord {
                        mods: oink_routing::Mods::NONE,
                        key,
                    },
                });
            }
            continue;
        }
        match Chord::parse(&h.keys) {
            Ok(chord) if out.iter().any(|b| b.chord == chord) => {
                tracing::warn!(action = h.action.as_str(), keys = ?h.keys, "hotkey duplicates another one: ignored");
            }
            Ok(chord) => out.push(HotkeyBinding {
                chord,
                action: action_name(h),
            }),
            Err(e) => tracing::warn!(action = h.action.as_str(), keys = ?h.keys, %e, "hotkey ignored"),
        }
    }
    out
}

fn shared_microphone(view: &crate::device_cards::DeviceCardsView) -> Option<&crate::device_cards::MicrophoneCard> {
    view.cards
        .iter()
        .filter_map(|card| card.status.as_ref())
        .map(|status| &status.microphone)
        .find(|mic| mic.mode == crate::device_cards::MicrophoneMode::Source)
}

impl Node {
    pub(crate) fn effective_hotkeys(&self) -> Vec<Hotkey> {
        let slots = self
            .link_config()
            .ok()
            .map(|group| group.applied().pointer_slots.clone())
            .unwrap_or_default();
        effective(self.settings().hotkeys, &slots)
    }

    /// 把快捷键交给路由（启动时、设置变化时），立即生效。
    pub(crate) fn apply_hotkeys(&self, list: &[Hotkey]) {
        let b = bindings(list);
        tracing::debug!(count = b.len(), "hotkeys applied");
        if let Ok(input) = self.input_thread() {
            input.send(crate::threads::input::InputCommand::Hotkeys(b));
        }
    }

    fn toggle_shared_microphone_mute(&self) -> crate::api::ApiResult<()> {
        self.apply_shared_microphone_mute(&AgentApi::device_cards(self)?)
    }

    pub(crate) fn apply_shared_microphone_mute(
        &self,
        view: &crate::device_cards::DeviceCardsView,
    ) -> crate::api::ApiResult<()> {
        let source = shared_microphone(view);
        let Some(source) = source else {
            self.emit(ApiEvent::NoSharedMicrophone);
            return Ok(());
        };
        if let (Some(token), Some(forwarding)) = (&source.controls_token, &source.forwarding) {
            let id = crate::device_controls::runtime::fresh_request_id();
            self.track_microphone_notice(&id, !forwarding.muted);
            let reply = AgentApi::set_forwarding_controls(
                self,
                id.clone(),
                token.clone(),
                forwarding.volume_percent,
                !forwarding.muted,
            );
            match &reply {
                Ok(crate::device_cards::DeviceControlReply::Completed(completed)) => {
                    self.complete_microphone_notice(&id, &completed.result)
                }
                Err(_) => self.complete_microphone_notice(
                    &id,
                    &crate::device_cards::DeviceControlResult::refused(
                        crate::device_cards::DeviceControlFailure::InvalidValue,
                    ),
                ),
                _ => {}
            }
            reply?;
        }
        Ok(())
    }

    /// 路由识别到了本机键盘上的快捷键。
    pub(crate) fn on_hotkey(&self, action: &str) {
        if action.starts_with("move_pointer_to:") {
            if let Some(target) = self
                .effective_hotkeys()
                .iter()
                .find(|h| action_name(h) == action)
                .and_then(|h| h.device)
                && let Ok(input) = self.input_thread()
            {
                input.send(crate::threads::input::InputCommand::MovePointerTo(target.0));
            }
            return;
        }
        let Some(action) = HotkeyAction::parse(action) else {
            tracing::warn!(action, "unknown hotkey action");
            return;
        };
        let r = match action {
            HotkeyAction::MovePointerTo | HotkeyAction::LockEscape => Ok(()),
            HotkeyAction::LockFocus => {
                // The router already requested the toggle at physical key-down,
                // using that exact focus epoch. This event is notification only.
                tracing::info!("hotkey: group focus lock toggled");
                Ok(())
            }
            HotkeyAction::ReturnPointerHome => {
                tracing::info!("hotkey: call the pointer back to this device");
                if let Ok(input) = self.input_thread() {
                    input.send(crate::threads::input::InputCommand::ReturnPointerHome);
                }
                Ok(())
            }
            HotkeyAction::ToggleSharing => {
                let enabled = self.fully_paused();
                tracing::info!(enabled, "hotkey: toggle sharing");
                AgentApi::set_local_paused(self, crate::device_controls::runtime::fresh_request_id(), !enabled)
                    .map(|_| ())
            }
            HotkeyAction::PauseAll => {
                // Physical emergency handling already entered the safety path.
                Ok(())
            }
            HotkeyAction::OpenWindow => {
                tracing::info!("hotkey: open the Oink window");
                #[cfg(windows)]
                {
                    crate::backend::foreground::open_from_hotkey().map_err(|error| crate::api::ApiError::Unavailable {
                        message: error.to_string(),
                    })
                }
                #[cfg(not(windows))]
                {
                    self.emit(ApiEvent::ShowWindow);
                    Ok(())
                }
            }
            HotkeyAction::ToggleMicrophoneMute => self.toggle_shared_microphone_mute(),
            HotkeyAction::ToggleGameMode => {
                tracing::info!("hotkey: toggle game mode on the current focus device");
                AgentApi::toggle_game_mode(self)
            }
        };
        if let Err(e) = r {
            tracing::warn!(?action, ?e, "hotkey action failed");
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn microphone_hotkey_defaults_off_and_preserves_saved_choices() {
        let action = HotkeyAction::ToggleMicrophoneMute;
        for saved in [None, Some(vec![])] {
            let values = effective(saved, &BTreeMap::new());
            let h = values.iter().find(|h| h.action == action).unwrap();
            assert_eq!(h.keys, ["Ctrl", "Alt", "M"]);
            assert!(!h.enabled);
            assert!(!bindings(&values).iter().any(|b| b.action == action.as_str()));
        }
        assert!(configured(Some(vec![])).is_empty());
        let custom = hk(action, &["Ctrl", "J"], true);
        assert_eq!(configured(Some(vec![custom.clone()])), vec![custom]);
        assert_eq!(HotkeyAction::parse("toggle_microphone_mute"), Some(action));
    }

    fn hk(action: HotkeyAction, keys: &[&str], enabled: bool) -> Hotkey {
        Hotkey {
            action,
            device: None,
            keys: keys.iter().map(|s| s.to_string()).collect(),
            enabled,
        }
    }

    #[test]
    fn escape_binding_requires_exactly_one_key_and_supports_either_modifier_side() {
        for keys in [
            &["Meta"][..],
            &["Ctrl"],
            &["Alt"],
            &["Shift"],
            &["X"],
            &["Escape"],
            &["Tab"],
        ] {
            let h = hk(HotkeyAction::LockEscape, keys, true);
            assert!(validate(std::slice::from_ref(&h)).is_ok());
            let b = bindings(&[h]);
            assert_eq!(b.len(), 1);
            assert_eq!(b[0].action, "lock_escape");
        }
        for keys in [&[][..], &["Ctrl", "X"], &["Ctrl", "Alt"], &["X", "Y"], &["Hyper"]] {
            for enabled in [false, true] {
                assert!(validate(&[hk(HotkeyAction::LockEscape, keys, enabled)]).is_err());
            }
        }
        assert!(bindings(&[hk(HotkeyAction::LockEscape, &["Meta"], false)]).is_empty());
    }
    #[test]
    fn only_enabled_valid_unique_hotkeys_reach_the_router() {
        let b = bindings(&[
            hk(HotkeyAction::LockFocus, &["Ctrl", "Alt", "L"], true),
            hk(HotkeyAction::ReturnPointerHome, &["Ctrl", "Alt", "H"], false),
            hk(HotkeyAction::ToggleSharing, &["Ctrl", "Alt", "Hyper"], true),
            hk(HotkeyAction::OpenWindow, &["Alt", "L", "Ctrl"], true),
        ]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].action, "lock_focus");
        assert!(bindings(&[hk(HotkeyAction::OpenWindow, &[], true)]).is_empty());
    }

    #[test]
    fn saved_chords_are_preserved_and_new_hold_default_is_enabled() {
        let list = configured(Some(vec![hk(HotkeyAction::ToggleSharing, &["Ctrl", "Alt", "S"], true)]));
        assert_eq!(list.len(), 1);
        assert!(
            effective(Some(list.clone()), &BTreeMap::new())
                .iter()
                .any(|h| h.action == HotkeyAction::LockEscape && h.enabled && h.keys == ["Alt"])
        );
        assert_eq!(list[0].keys, ["Ctrl", "Alt", "S"]);
        assert!(
            configured(None)
                .iter()
                .filter(|h| h.action != HotkeyAction::ToggleMicrophoneMute)
                .all(|h| h.enabled)
        );
        assert_eq!(configured(None), crate::agent_api::default_hotkeys());
        assert!(validate(&list).is_ok());
        let dup = [
            hk(HotkeyAction::LockFocus, &["Ctrl", "Alt", "L"], true),
            hk(HotkeyAction::OpenWindow, &["Alt", "Ctrl", "L"], true),
        ];
        assert!(validate(&dup).is_err());
        assert!(validate(&[hk(HotkeyAction::OpenWindow, &["Ctrl", "Nope"], true)]).is_err());
        assert!(validate(&[hk(HotkeyAction::OpenWindow, &["Ctrl", "Nope"], false)]).is_ok());
    }

    #[test]
    fn defaults_include_local_and_off_without_persisting_unedited_keys() {
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        let c = DeviceId([3; 32]);
        let slots = [(a, Some(0)), (b, Some(9)), (c, None)].into();
        let effective = effective(None, &slots);
        let devices: Vec<_> = effective
            .iter()
            .filter(|h| h.action == HotkeyAction::MovePointerTo)
            .collect();
        assert_eq!(devices[0].keys, ["Ctrl", "Alt", "1"]);
        assert_eq!(devices[1].keys, ["Ctrl", "Alt", "0"]);
        assert!(!devices[2].enabled);
        assert!(devices[2].keys.is_empty());
        assert_eq!(
            bindings(&effective)
                .iter()
                .filter(|b| b.action.starts_with("move_pointer_to:"))
                .count(),
            2
        );
        assert!(
            save_overrides(&effective, None, &slots)
                .iter()
                .all(|h| h.action != HotkeyAction::MovePointerTo)
        );
        let mut edited = effective.clone();
        edited.iter_mut().find(|h| h.device == Some(a.into())).unwrap().enabled = false;
        let saved = save_overrides(&edited, None, &slots);
        assert_eq!(
            saved.iter().filter(|h| h.action == HotkeyAction::MovePointerTo).count(),
            1
        );
        assert!(
            !super::effective(Some(saved.clone()), &slots)
                .iter()
                .find(|h| h.device == Some(a.into()))
                .unwrap()
                .enabled
        );
        // A different capturing device still uses the group default.
        assert!(
            super::effective(None, &slots)
                .iter()
                .find(|h| h.device == Some(a.into()))
                .unwrap()
                .enabled
        );
        let next_slots = [(b, Some(9)), (c, None)].into();
        assert!(
            super::effective(Some(saved), &next_slots)
                .iter()
                .all(|h| h.device != Some(a.into()))
        );
    }

    #[test]
    fn device_defaults_use_the_existing_conflict_validation() {
        let a = DeviceId([1; 32]);
        let slots = [(a, Some(0))].into();
        let saved = vec![hk(HotkeyAction::LockFocus, &["Alt", "Ctrl", "1"], true)];
        assert!(validate(&effective(Some(saved), &slots)).is_err());
    }

    #[test]
    fn action_names_match_serde() {
        for a in HotkeyAction::ALL {
            let json = serde_json::to_value(a).unwrap();
            assert_eq!(json.as_str(), Some(a.as_str()));
            assert_eq!(HotkeyAction::parse(a.as_str()), Some(*a));
        }
    }
}

#[cfg(test)]
mod escape_default_tests {
    use super::*;
    #[test]
    fn passive_default_option_alt_preserves_explicit_old_meta_override() {
        let defaults = crate::agent_api::default_hotkeys();
        let escape = defaults.iter().find(|h| h.action == HotkeyAction::LockEscape).unwrap();
        assert_eq!(escape.keys, ["Alt"]);
        for keys in [vec!["Meta".to_owned()], vec!["X".to_owned()]] {
            let mut saved = escape.clone();
            saved.keys = keys.clone();
            let effective = effective(Some(vec![saved]), &BTreeMap::new());
            assert_eq!(
                effective
                    .iter()
                    .find(|h| h.action == HotkeyAction::LockEscape)
                    .unwrap()
                    .keys,
                keys
            );
        }
    }
}
