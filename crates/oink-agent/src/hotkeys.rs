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

/// The retained slots order members across peers; local chord occupancy
/// determines whether a default is available.
/// Chords are assigned only in `effective`, after reserving local choices.
fn defaults(slots: &BTreeMap<DeviceId, Option<u8>>) -> Vec<Hotkey> {
    let mut slots = slots.iter().collect::<Vec<_>>();
    slots.sort_by_key(|(device, slot)| (slot.unwrap_or(10), **device));
    slots
        .into_iter()
        .map(|(device, _slot)| Hotkey {
            action: HotkeyAction::MovePointerTo,
            device: Some((*device).into()),
            profile: None,
            enabled: true,
            keys: Vec::new(),
        })
        .collect()
}
fn action_name(h: &Hotkey) -> String {
    if h.action == HotkeyAction::MovePointerTo
        && let Some(device) = h.device
    {
        return format!("move_pointer_to:{}", device.0.to_hex());
    }
    if h.action == HotkeyAction::SwitchLayout
        && let Some(profile) = &h.profile
    {
        return format!("switch_layout:{profile}");
    }
    h.action.as_str().into()
}
pub(crate) fn effective(saved: Option<Vec<Hotkey>>, slots: &BTreeMap<DeviceId, Option<u8>>) -> Vec<Hotkey> {
    let mut saved = configured(saved);
    // New actions are visible at runtime without changing the saved override list.
    for action in [
        HotkeyAction::OpenWindow,
        HotkeyAction::LockEscape,
        HotkeyAction::ToggleMicrophoneMute,
        HotkeyAction::LockRemoteWindows,
        HotkeyAction::MovePointerNext,
    ] {
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
    let defaults = defaults(slots);
    // Local overrides survive group merges, whose retained slot register can
    // have a different numbering. Reserve active overrides before assigning any
    // projected default; departed-device overrides must not consume digits.
    let mut used: Vec<_> = saved
        .iter()
        .filter(|h| {
            h.action != HotkeyAction::LockEscape
                && (h.action != HotkeyAction::MovePointerTo || defaults.iter().any(|d| d.device == h.device))
        })
        .filter_map(|h| Chord::parse(&h.keys).ok())
        .collect();
    for mut default in defaults {
        if let Some(custom) = saved
            .iter()
            .find(|h| h.action == default.action && h.device == default.device)
        {
            list.push(custom.clone());
            continue;
        }
        if default.enabled {
            default.keys = (0..10)
                .map(|slot| vec!["Ctrl".into(), "Alt".into(), ((slot + 1) % 10).to_string()])
                .find(|keys| !used.contains(&Chord::parse(keys).expect("valid digit chord")))
                .unwrap_or_default();
            default.enabled = !default.keys.is_empty();
            if default.enabled {
                used.push(Chord::parse(&default.keys).expect("valid assigned chord"));
            }
        }
        list.push(default);
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
    let defaults = effective(Some(saved.clone()), slots);
    let mut out = Vec::new();
    let mut push = |hotkey: &Hotkey| {
        if !out.contains(hotkey) {
            out.push(hotkey.clone());
        }
    };
    for hotkey in incoming.iter().filter(|h| h.action != HotkeyAction::MovePointerTo) {
        push(hotkey);
    }
    for hotkey in saved.iter().filter(|h| {
        h.action == HotkeyAction::MovePointerTo
            && !incoming.iter().any(|i| i.action == h.action && i.device == h.device)
    }) {
        push(hotkey);
    }
    for hotkey in incoming.iter().filter(|h| {
        h.action == HotkeyAction::MovePointerTo
            && (saved.iter().any(|old| old.action == h.action && old.device == h.device) || !defaults.contains(h))
    }) {
        push(hotkey);
    }
    out
}

/// 保存前检查：启用的快捷键键名有效、互不相同。
pub(crate) fn validate(list: &[Hotkey]) -> Result<(), String> {
    for h in list {
        if (h.action == HotkeyAction::SwitchLayout) != h.profile.is_some()
            || h.profile.as_ref().is_some_and(|id| id.is_empty())
        {
            return Err("Layout shortcuts require a target profile".into());
        }
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
        if h.action == HotkeyAction::LockRemoteWindows
            && chord.key == oink_foundation::input::KeyCode::keyboard(0x0f)
            && chord.mods == oink_routing::Mods::META
        {
            return Err("Win+L 保持锁定自身，请为远程锁屏选择其他快捷键".into());
        }
        if seen.contains(&chord) {
            return Err("两个快捷键不能相同".into());
        }
        seen.push(chord);
    }
    Ok(())
}

/// Validate only edited assignments. Pre-existing conflicts must not veto
/// an unrelated shortcut edit or a preference patch. Disabled assignments also
/// reserve their chord, matching the recorder's conflict checks.
pub(crate) fn validate_edits(list: &[Hotkey], previous: &[Hotkey]) -> Result<(), String> {
    for h in list.iter().filter(|h| !previous.contains(h)) {
        validate(std::slice::from_ref(h))?;
        if h.keys.is_empty() {
            continue;
        }
        let Ok(chord) = Chord::parse(&h.keys) else {
            continue;
        };
        if let Some(other) = list
            .iter()
            .find(|other| *other != h && Chord::parse(&other.keys).ok() == Some(chord))
        {
            return Err(format!("hotkey_conflict:{}", action_name(other)));
        }
    }
    Ok(())
}

/// 启用且键名有效的快捷键 → 路由的绑定。无效的（键名无法识别、与紧急暂停相同）记 warn 并跳过；
/// 两条相同的组合键只保留第一条；显式编辑快捷键时会拒绝冲突，既存冲突不阻塞其他设置。
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
            Ok(chord)
                if h.action == HotkeyAction::LockRemoteWindows
                    && chord.key == oink_foundation::input::KeyCode::keyboard(0x0f)
                    && chord.mods == oink_routing::Mods::META => {}
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

/// The online device after `current` in group order, wrapping around. `online`
/// includes this device; a focus device that is not online starts from its own
/// position in the order. None when no other online device exists.
pub(crate) fn next_device(online: &[DeviceId], order: &BTreeMap<DeviceId, i64>, current: DeviceId) -> Option<DeviceId> {
    let key = |device: DeviceId| oink_network::device_order::sort_key(order, device);
    let mut list = online.to_vec();
    list.sort_by_key(|device| key(*device));
    let next = list
        .iter()
        .find(|device| key(**device) > key(current))
        .or(list.first())
        .copied()?;
    (next != current).then_some(next)
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

    pub(crate) fn prune_profile_hotkeys(&self, ids: &[String]) -> crate::layout_profiles::types::Result<()> {
        let is_stale = |h: &Hotkey| {
            h.action == HotkeyAction::SwitchLayout && !h.profile.as_ref().is_some_and(|id| ids.contains(id))
        };
        if !self
            .settings()
            .hotkeys
            .as_ref()
            .is_some_and(|list| list.iter().any(is_stale))
        {
            return Ok(());
        }
        self.update_settings(|s| {
            if let Some(list) = &mut s.hotkeys {
                list.retain(|h| !is_stale(h));
            }
        })
        .map_err(crate::layout_profiles::state::unavailable)?;
        self.apply_hotkeys(&self.effective_hotkeys());
        self.emit(ApiEvent::InputSettings(crate::agent_api::input_settings(self)));
        Ok(())
    }

    /// 把快捷键交给路由（启动时、设置变化时），立即生效。
    pub(crate) fn apply_hotkeys(&self, list: &[Hotkey]) {
        if let Ok(input) = self.input_thread() {
            let mut applied = crate::node::lock(&self.inner.applied_hotkeys);
            if applied.as_deref() == Some(list) {
                return;
            }
            let b = bindings(list);
            tracing::debug!(count = b.len(), "hotkeys applied");
            input.send(crate::threads::input::InputCommand::Hotkeys(b));
            *applied = Some(list.to_vec());
        }
    }

    fn next_pointer_device(&self) -> Option<DeviceId> {
        let config = self.link_config().ok()?;
        let online = config
            .members()
            .members
            .iter()
            .filter(|m| m.is_self || m.online)
            .map(|m| m.device)
            .collect::<Vec<_>>();
        let current = self
            .input_status()
            .ok()
            .and_then(|status| status.group_focus.holder)
            .unwrap_or(self.id());
        next_device(&online, &config.applied().device_order, current)
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
        if let Some(profile_id) = action.strip_prefix("switch_layout:") {
            let result = self.profile_snapshot().map(|state| {
                // The menu ignores selection of the active layout.
                if state.active_profile_id.as_deref() == Some(profile_id) {
                    return;
                }
                let reply = self.layout_profile_command(crate::layout_profiles::types::Command::Load(
                    crate::layout_profiles::types::Load {
                        request_id: crate::device_controls::runtime::fresh_request_id(),
                        expected_token: state.token,
                        profile_id: profile_id.into(),
                    },
                ));
                if reply.get("error").is_some() {
                    tracing::warn!(?reply, "layout hotkey load refused");
                }
            });
            if let Err(error) = result {
                tracing::warn!(?error, "layout hotkey failed");
            }
            return;
        }
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
            HotkeyAction::MovePointerTo | HotkeyAction::SwitchLayout | HotkeyAction::LockEscape => Ok(()),
            HotkeyAction::LockRemoteWindows => Ok(()), // Router captured and dispatched the exact target.
            HotkeyAction::MovePointerNext => {
                // Same landing and handoff as the per-device move shortcuts.
                if let Some(target) = self.next_pointer_device()
                    && let Ok(input) = self.input_thread()
                {
                    tracing::info!("hotkey: move the pointer to the next online device");
                    input.send(crate::threads::input::InputCommand::MovePointerTo(target));
                }
                Ok(())
            }
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
    fn mouse_hotkeys_validate_all_actions_and_reserve_disabled_duplicates() {
        for name in ["Mouse3", "Mouse4", "Mouse5"] {
            for keys in [vec![name], vec!["Ctrl", name]] {
                let global = hk(HotkeyAction::OpenWindow, &keys, true);
                assert!(validate(std::slice::from_ref(&global)).is_ok());
                let mut device = hk(HotkeyAction::MovePointerTo, &keys, true);
                device.device = Some(DeviceId([1; 32]).into());
                let mut layout = hk(HotkeyAction::SwitchLayout, &keys, false);
                layout.profile = Some("profile".into());
                assert!(validate(std::slice::from_ref(&device)).is_ok());
                assert!(validate_edits(&[global.clone(), layout.clone()], &[layout]).is_err());
                assert!(validate(&[global, device]).is_err());
                assert_eq!(bindings(&[hk(HotkeyAction::OpenWindow, &keys, true)]).len(), 1);
            }
        }
    }

    #[test]
    fn layout_targets_are_local_opt_in_and_clearing_removes_the_override() {
        assert!(
            !effective(None, &BTreeMap::new())
                .iter()
                .any(|h| h.action == HotkeyAction::SwitchLayout)
        );
        let mut h = hk(HotkeyAction::SwitchLayout, &["Ctrl", "Shift", "F9"], true);
        assert!(validate(std::slice::from_ref(&h)).is_err());
        h.profile = Some("desk".into());
        assert!(validate(std::slice::from_ref(&h)).is_ok());
        let encoded = serde_json::to_value(&h).unwrap();
        assert_eq!(serde_json::from_value::<Hotkey>(encoded).unwrap(), h);
        let mut list = effective(Some(vec![h.clone()]), &BTreeMap::new());
        list.retain(|entry| entry.action != HotkeyAction::SwitchLayout);
        assert!(
            !save_overrides(&list, Some(vec![h]), &BTreeMap::new())
                .iter()
                .any(|h| h.action == HotkeyAction::SwitchLayout)
        );
    }

    #[test]
    fn saving_recorded_layout_shortcuts_removes_exact_duplicate_entries() {
        let mut layout = hk(HotkeyAction::SwitchLayout, &["Ctrl", "Alt", "F9"], true);
        layout.profile = Some("desk".into());
        let saved = save_overrides(
            &[layout.clone(), layout.clone()],
            Some(vec![layout.clone()]),
            &BTreeMap::new(),
        );
        assert_eq!(saved, vec![layout]);
    }

    #[test]
    fn profile_binding_keeps_target_and_checks_only_new_edits() {
        let mut profile = hk(HotkeyAction::SwitchLayout, &["Ctrl", "Shift", "F9"], true);
        profile.profile = Some("work".into());
        assert_eq!(bindings(std::slice::from_ref(&profile))[0].action, "switch_layout:work");
        let broken = vec![
            hk(HotkeyAction::OpenWindow, &["Ctrl", "A"], true),
            hk(HotkeyAction::LockFocus, &["Ctrl", "A"], true),
        ];
        let mut next = broken.clone();
        next.push(profile.clone());
        assert!(validate_edits(&next, &broken).is_ok());
        profile.keys = vec!["Ctrl".into(), "A".into()];
        next.pop();
        next.push(profile);
        assert!(validate_edits(&next, &broken).is_err());
    }

    #[test]
    fn exhausted_digits_leave_no_default_and_projection_is_not_saved() {
        let device = DeviceId([1; 32]);
        let slots = [(device, Some(0))].into();
        let mut saved = Vec::new();
        for digit in ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"] {
            saved.push(hk(HotkeyAction::OpenWindow, &["Alt", "Ctrl", digit], false));
        }
        let list = effective(Some(saved.clone()), &slots);
        let default = list.iter().find(|h| h.device.is_some()).unwrap();
        assert!(!default.enabled);
        assert!(default.keys.is_empty());
        assert!(
            save_overrides(&list, Some(saved.clone()), &slots)
                .iter()
                .all(|h| h.device.is_none())
        );
        saved.pop();
        let list = effective(Some(saved), &slots);
        assert_eq!(
            list.iter().find(|h| h.device.is_some()).unwrap().keys,
            ["Ctrl", "Alt", "0"]
        );
    }

    #[test]
    fn fourth_member_gets_first_free_digit_across_all_actions() {
        let devices = [
            DeviceId([1; 32]),
            DeviceId([2; 32]),
            DeviceId([3; 32]),
            DeviceId([4; 32]),
        ];
        let slots = devices.iter().enumerate().map(|(i, d)| (*d, Some(i as u8))).collect();
        let mut saved = Vec::new();
        for (device, key) in devices[..3].iter().zip(["1", "8", "9"]) {
            let mut h = hk(HotkeyAction::MovePointerTo, &["Ctrl", "Alt", key], true);
            h.device = Some((*device).into());
            saved.push(h);
        }
        saved.push(hk(HotkeyAction::OpenWindow, &["Ctrl", "Alt", "2"], false));
        let list = effective(Some(saved), &slots);
        assert_eq!(
            list.iter().find(|h| h.device == Some(devices[3].into())).unwrap().keys,
            ["Ctrl", "Alt", "3"]
        );
        assert!(validate(&list).is_ok());
    }

    #[test]
    fn new_member_default_does_not_collide_with_saved_member_override() {
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        let mut old = hk(HotkeyAction::MovePointerTo, &["Ctrl", "Alt", "1"], true);
        old.device = Some(b.into());
        let list = effective(Some(vec![old.clone()]), &[(a, Some(0)), (b, Some(1))].into());
        assert!(validate(&list).is_ok());
        assert_eq!(list.iter().find(|h| h.device == old.device), Some(&old));
        assert_eq!(
            list.iter().find(|h| h.device == Some(a.into())).unwrap().keys,
            ["Ctrl", "Alt", "2"]
        );
    }

    #[test]
    fn open_window_defaults_off_and_preserves_customization() {
        let defaults = effective(None, &BTreeMap::new());
        let window = defaults.iter().find(|h| h.action == HotkeyAction::OpenWindow).unwrap();
        assert!(!window.enabled);
        assert_eq!(window.keys, ["Ctrl", "Alt", "A"]);
        assert!(
            !bindings(&defaults)
                .iter()
                .any(|binding| binding.action == "open_window")
        );
        let empty = effective(Some(vec![]), &BTreeMap::new());
        assert_eq!(
            empty.iter().find(|h| h.action == HotkeyAction::OpenWindow),
            Some(window)
        );
        for enabled in [true, false] {
            let custom = hk(HotkeyAction::OpenWindow, &["Ctrl", "Shift", "W"], enabled);
            let saved = effective(Some(vec![custom.clone()]), &BTreeMap::new());
            assert_eq!(
                saved.iter().find(|h| h.action == HotkeyAction::OpenWindow),
                Some(&custom)
            );
        }
    }

    #[test]
    fn remote_windows_lock_validation_preserves_saved_choices_and_rejects_bare_win_l() {
        let valid = hk(HotkeyAction::LockRemoteWindows, &["Meta", "Shift", "L"], true);
        assert!(validate(std::slice::from_ref(&valid)).is_ok());
        let mut disabled = valid.clone();
        disabled.enabled = false;
        let list = effective(Some(vec![disabled.clone()]), &BTreeMap::new());
        assert_eq!(
            list.iter().find(|h| h.action == HotkeyAction::LockRemoteWindows),
            Some(&disabled)
        );
        assert!(
            validate(&[
                valid.clone(),
                hk(HotkeyAction::OpenWindow, &["Shift", "Meta", "L"], true)
            ])
            .is_err()
        );
        let invalid = hk(HotkeyAction::LockRemoteWindows, &["Meta", "L"], true);
        assert!(validate(std::slice::from_ref(&invalid)).is_err());
        assert!(bindings(&[invalid]).is_empty());
    }

    #[test]
    fn remote_windows_lock_default_is_enabled_and_merged_into_saved_lists() {
        let defaults = crate::agent_api::default_hotkeys();
        let item = defaults.iter().find(|h| h.action.as_str() == "lock_remote_windows");
        assert!(item.is_some(), "Windows remote lock action must have a default");
        let item = item.unwrap();
        assert!(item.enabled);
        assert_eq!(item.keys, ["Meta", "Shift", "L"]);
        assert!(
            effective(Some(Vec::new()), &BTreeMap::new())
                .iter()
                .any(|h| h.action.as_str() == "lock_remote_windows" && h.enabled)
        );
    }

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
            profile: None,
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
                .filter(|h| {
                    !matches!(
                        h.action,
                        HotkeyAction::ToggleMicrophoneMute | HotkeyAction::OpenWindow | HotkeyAction::MovePointerNext
                    )
                })
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
    fn defaults_include_local_without_persisting_unedited_keys() {
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
        assert_eq!(devices[1].keys, ["Ctrl", "Alt", "2"]);
        // A group overflow marker cannot veto a digit freed by local choices.
        assert!(devices[2].enabled);
        assert_eq!(devices[2].keys, ["Ctrl", "Alt", "3"]);
        assert_eq!(
            bindings(&effective)
                .iter()
                .filter(|b| b.action.starts_with("move_pointer_to:"))
                .count(),
            3
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
    fn device_defaults_avoid_other_action_overrides() {
        let a = DeviceId([1; 32]);
        let slots = [(a, Some(0))].into();
        let saved = vec![hk(HotkeyAction::LockFocus, &["Alt", "Ctrl", "1"], true)];
        assert!(validate(&effective(Some(saved), &slots)).is_ok());
    }

    #[test]
    fn next_device_follows_group_order_wraps_includes_self_and_skips_offline() {
        let [me, b, c, d] = [1, 2, 3, 4].map(|i| DeviceId([i; 32]));
        let order: BTreeMap<_, _> = [(b, 0), (me, 1), (c, 2), (d, 3)].into();
        let online = [c, me, b];
        assert_eq!(next_device(&online, &order, b), Some(me));
        assert_eq!(next_device(&online, &order, me), Some(c));
        // d is offline: c wraps to the first online device.
        assert_eq!(next_device(&online, &order, c), Some(b));
        // A focus device that just went offline continues from its own position.
        assert_eq!(next_device(&online, &order, d), Some(b));
        let tied: BTreeMap<_, _> = [(b, 0), (me, 0), (c, 0)].into();
        assert_eq!(next_device(&online, &tied, me), Some(b));
        assert_eq!(next_device(&online, &tied, b), Some(c));
        assert_eq!(next_device(&[me], &order, me), None);
        assert_eq!(next_device(&[me], &order, b), Some(me));
        let defaults = effective(None, &BTreeMap::new());
        let next = defaults
            .iter()
            .find(|h| h.action == HotkeyAction::MovePointerNext)
            .unwrap();
        assert!(!next.enabled);
        assert_eq!(next.keys, ["Ctrl", "Alt", "N"]);
        assert!(validate(&defaults).is_ok());
        let mut on = next.clone();
        on.enabled = true;
        assert_eq!(bindings(&[on])[0].action, "move_pointer_next");
        assert!(effective(Some(vec![]), &BTreeMap::new()).iter().any(|h| h == next));
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
