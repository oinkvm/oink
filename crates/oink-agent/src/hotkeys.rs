//! 全局快捷键的接入：把界面保存的快捷键交给路由识别，执行路由识别到的动作。
//!
//! 识别在路由里（截获回调的快路径，物理键盘所在的设备上，不论焦点在哪台）：匹配后这组按键吞掉，路由发出
//! `StatusEvent::Hotkey { action }`，这里按动作名执行。Windows 上服务在独立进程里，钩子、路由与这里都在服务
//! The Windows open action checks the real UIAccess token, grants the UI permission, then posts its native open message.

use oink_routing::{Chord, HotkeyBinding};

#[cfg(not(windows))]
use crate::api::ApiEvent;
use crate::api::{AgentApi, Hotkey, HotkeyAction};
use crate::node::Node;

/// Missing configuration uses the complete current defaults. A saved list is
/// exactly the user's list; do not backfill actions from another schema.
pub(crate) fn configured(saved: Option<Vec<Hotkey>>) -> Vec<Hotkey> {
    saved.unwrap_or_else(crate::agent_api::default_hotkeys)
}

/// 保存前检查：启用的快捷键键名有效、互不相同。
pub(crate) fn validate(list: &[Hotkey]) -> Result<(), String> {
    let mut seen = Vec::new();
    for h in list.iter().filter(|h| h.enabled && !h.keys.is_empty()) {
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
        match Chord::parse(&h.keys) {
            Ok(chord) if out.iter().any(|b| b.chord == chord) => {
                tracing::warn!(action = h.action.as_str(), keys = ?h.keys, "hotkey duplicates another one: ignored");
            }
            Ok(chord) => out.push(HotkeyBinding {
                chord,
                action: h.action.as_str().to_string(),
            }),
            Err(e) => tracing::warn!(action = h.action.as_str(), keys = ?h.keys, %e, "hotkey ignored"),
        }
    }
    out
}

impl Node {
    /// 把快捷键交给路由（启动时、设置变化时），立即生效。
    pub(crate) fn apply_hotkeys(&self, list: &[Hotkey]) {
        let b = bindings(list);
        tracing::debug!(count = b.len(), "hotkeys applied");
        if let Ok(input) = self.input_thread() {
            input.send(crate::threads::input::InputCommand::Hotkeys(b));
        }
    }

    /// 路由识别到了本机键盘上的快捷键。
    pub(crate) fn on_hotkey(&self, action: &str) {
        let Some(action) = HotkeyAction::parse(action) else {
            tracing::warn!(action, "unknown hotkey action");
            return;
        };
        let r = match action {
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

    fn hk(action: HotkeyAction, keys: &[&str], enabled: bool) -> Hotkey {
        Hotkey {
            action,
            keys: keys.iter().map(|s| s.to_string()).collect(),
            enabled,
        }
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
    fn saved_lists_are_exact_and_defaults_are_enabled() {
        let list = configured(Some(vec![hk(HotkeyAction::ToggleSharing, &["Ctrl", "Alt", "S"], true)]));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].keys, ["Ctrl", "Alt", "S"]);
        assert!(configured(None).iter().all(|h| h.enabled));
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
    fn action_names_match_serde() {
        for a in HotkeyAction::ALL {
            let json = serde_json::to_value(a).unwrap();
            assert_eq!(json.as_str(), Some(a.as_str()));
            assert_eq!(HotkeyAction::parse(a.as_str()), Some(*a));
        }
    }
}
