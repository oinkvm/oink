//! One device-control model for native tray, Dock and taskbar menus.
#[cfg(target_os = "macos")]
pub mod macos;

use crate::{config::UiLanguage, device_cards::*};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: &'static str,
    pub label: String,
    pub checked: Option<bool>,
    pub enabled: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: &'static str,
    pub label: String,
    pub items: Vec<Item>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Model {
    pub groups: Vec<Group>,
    pub tasks: Vec<Item>,
}

fn label(language: UiLanguage, id: &str) -> &'static str {
    let text = match id {
        "keyboard" => ["Keyboard", "键盘", "キーボード", "키보드"],
        "mouse" => ["Mouse", "鼠标", "マウス", "마우스"],
        "game" => ["Game mode", "游戏模式", "ゲームモード", "게임 모드"],
        "audio" => ["Audio", "音频", "オーディオ", "오디오"],
        "keyboard-disabled" | "mouse-disabled" => ["Disabled", "禁用", "無効", "비활성화"],
        "keyboard-local" | "mouse-local" | "audio-local" => ["Local", "本地", "ローカル", "로컬"],
        "keyboard-shared" | "mouse-shared" => ["Shared", "共享", "共有", "공유"],
        "game-auto" => ["Automatic", "自动", "自動", "자동"],
        "game-on" => ["On", "开启", "オン", "켜기"],
        "game-off" => ["Off", "关闭", "オフ", "끄기"],
        "audio-forward" => ["Forward", "转发", "転送", "전달"],
        "audio-exit" => ["Audio exit", "出口", "音声出力先", "오디오 출력"],
        "lock" => ["Lock pointer", "锁定指针", "ポインターをロック", "포인터 잠금"],
        "unlock" => [
            "Unlock pointer",
            "解锁指针",
            "ポインターのロックを解除",
            "포인터 잠금 해제",
        ],
        "hide-screens" => [
            "Hide all screens",
            "隐藏全部屏幕",
            "すべての画面を隠す",
            "모든 화면 숨기기",
        ],
        "show-screens" => [
            "Show all screens",
            "显示全部屏幕",
            "すべての画面を表示",
            "모든 화면 표시",
        ],
        "pause" => ["Pause sharing", "暂停共享", "共有を一時停止", "공유 일시 중지"],
        "resume" => ["Resume sharing", "恢复共享", "共有を再開", "공유 재개"],
        "show" => ["Show main window", "显示主界面", "メインウインドウを表示", "주 창 표시"],
        "about" => ["About Oink", "关于 Oink", "Oink について", "Oink 정보"],
        "quit" => ["Quit Oink", "退出 Oink", "Oink を終了", "Oink 종료"],
        _ => ["", "", "", ""],
    };
    text[match language {
        UiLanguage::SimplifiedChinese => 1,
        UiLanguage::Japanese => 2,
        UiLanguage::Korean => 3,
        _ => 0,
    }]
}
fn invokable(action: Option<&CardAction>) -> bool {
    matches!(action, Some(CardAction::Invoke { .. }))
}

/// The caller resolves Auto using the native process's preferred UI language.
/// Unknown facts never imply a selected mode or an available mutation.
pub fn model(view: &DeviceCardsView, language: UiLanguage) -> Model {
    let card = view.cards.iter().find(|c| c.is_self);
    let status = card.and_then(|c| c.status.as_ref());
    let entry = |id, checked, enabled| Item {
        id,
        label: label(language, id).into(),
        checked,
        enabled,
    };
    let group = |id, items| Group {
        id,
        label: label(language, id).into(),
        items,
    };
    let keyboard = |id, mode| {
        let checked = status.is_some_and(|s| {
            if mode == SharingMode::Disabled {
                s.keyboard.disabled == Some(true)
            } else {
                s.keyboard.disabled == Some(false) && s.keyboard.resource.configured_mode == Some(mode)
            }
        });
        let action = status.map(|s| match mode {
            SharingMode::Disabled => &s.actions.keyboard_disabled,
            _ if s.keyboard.disabled == Some(true) => &s.actions.keyboard_enabled,
            SharingMode::Local => &s.actions.keyboard_local,
            SharingMode::Shared => &s.actions.keyboard_shared,
        });
        entry(id, Some(checked), invokable(action))
    };
    let mouse = |id, mode| {
        entry(
            id,
            Some(status.is_some_and(|s| s.mouse.configured_mode == Some(mode))),
            invokable(status.map(|s| match mode {
                SharingMode::Disabled => &s.actions.mouse_disabled,
                SharingMode::Local => &s.actions.mouse_local,
                SharingMode::Shared => &s.actions.mouse_shared,
            })),
        )
    };
    let game = |id, mode| {
        entry(
            id,
            Some(status.is_some_and(|s| s.game.preference == Some(mode))),
            invokable(status.map(|s| match mode {
                GamePreference::Auto => &s.game.actions.auto,
                GamePreference::On => &s.game.actions.on,
                GamePreference::Off => &s.game.actions.off,
            })),
        )
    };
    let mut audio = vec![entry(
        "audio-local",
        Some(status.is_some_and(|s| s.speaker.resource.configured_mode == Some(SpeakerMode::Unused))),
        invokable(status.map(|s| {
            if s.speaker.resource.configured_mode == Some(SpeakerMode::Receiver) {
                &s.speaker.resource.action
            } else {
                &s.actions.forwarding_disabled
            }
        })),
    )];
    let exit = view.exit_volume.as_ref().map(|v| v.receiver_device).or_else(|| {
        view.cards
            .iter()
            .find(|c| {
                c.status
                    .as_ref()
                    .is_some_and(|s| s.speaker.resource.configured_mode == Some(SpeakerMode::Receiver))
            })
            .map(|c| c.device)
    });
    if exit.is_some() && exit != card.map(|c| c.device) {
        audio.push(entry(
            "audio-forward",
            Some(status.is_some_and(|s| s.speaker.forwarding_enabled == Some(true))),
            invokable(status.map(|s| &s.actions.forwarding_enabled)),
        ));
    }
    audio.push(entry(
        "audio-exit",
        Some(status.is_some_and(|s| s.speaker.resource.configured_mode == Some(SpeakerMode::Receiver))),
        invokable(status.map(|s| &s.speaker.make_exit_action)),
    ));
    let locked = status.is_some_and(|s| s.cursor.locked_device.is_some());
    let hidden = status.is_some_and(|s| s.screen.configured_mode == Some(SharingMode::Local));
    let pause = card.and_then(|c| c.pause.as_ref());
    Model {
        groups: vec![
            group(
                "keyboard",
                vec![
                    keyboard("keyboard-disabled", SharingMode::Disabled),
                    keyboard("keyboard-local", SharingMode::Local),
                    keyboard("keyboard-shared", SharingMode::Shared),
                ],
            ),
            group(
                "mouse",
                vec![
                    mouse("mouse-disabled", SharingMode::Disabled),
                    mouse("mouse-local", SharingMode::Local),
                    mouse("mouse-shared", SharingMode::Shared),
                ],
            ),
            group(
                "game",
                vec![
                    game("game-auto", GamePreference::Auto),
                    game("game-on", GamePreference::On),
                    game("game-off", GamePreference::Off),
                ],
            ),
            group("audio", audio),
        ],
        tasks: vec![
            entry(
                if locked { "unlock" } else { "lock" },
                None,
                status
                    .is_some_and(|s| !matches!(s.cursor.context, CursorContext::Unknown | CursorContext::LocalPaused)),
            ),
            entry(
                if hidden { "show-screens" } else { "hide-screens" },
                None,
                invokable(status.map(|s| {
                    if hidden {
                        &s.actions.screen_shared
                    } else {
                        &s.actions.screen_local
                    }
                })),
            ),
            entry(
                if pause.is_some_and(|p| p.paused) {
                    "resume"
                } else {
                    "pause"
                },
                None,
                invokable(pause.map(|p| &p.action)) && pause.is_none_or(|p| !p.paused || p.can_resume),
            ),
            entry("show", None, true),
            entry("quit", None, true),
        ],
    }
}

/// Stable argument values contain no device identity or revision token. A launch
/// resolves its action against fresh local facts in the running backend.
pub const ARGUMENT: &str = "--native-control";
/// Accept exactly one recognized command; extra arguments cannot become a
/// partially applied action or an arbitrary backend request.
pub fn launch_command(args: impl IntoIterator<Item = String>) -> Option<String> {
    let args: Vec<_> = args.into_iter().collect();
    if args.len() != 2 || args[0] != ARGUMENT {
        return None;
    }
    let id = &args[1];
    matches!(
        id.as_str(),
        "keyboard-disabled"
            | "keyboard-local"
            | "keyboard-shared"
            | "mouse-disabled"
            | "mouse-local"
            | "mouse-shared"
            | "game-auto"
            | "game-on"
            | "game-off"
            | "audio-local"
            | "audio-forward"
            | "audio-exit"
            | "lock"
            | "unlock"
            | "show"
            | "pause"
            | "resume"
            | "quit"
    )
    .then(|| id.clone())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellAction {
    Show,
    About,
    Quit,
}

pub fn dispatch(api: &dyn crate::api::AgentApi, id: &str) -> crate::api::ApiResult<Option<ShellAction>> {
    use crate::api::ApiError;
    match id {
        "show" => return Ok(Some(ShellAction::Show)),
        "about" => return Ok(Some(ShellAction::About)),
        "quit" => return Ok(Some(ShellAction::Quit)),
        _ => {}
    }
    let unavailable = || ApiError::Unavailable {
        message: "native control is unavailable".into(),
    };
    let mut view = api.device_cards()?;
    let menu = model(&view, UiLanguage::English);
    if !menu
        .tasks
        .iter()
        .chain(menu.groups.iter().flat_map(|g| &g.items))
        .any(|i| i.id == id && i.enabled)
    {
        return Err(unavailable());
    }
    let card = view.cards.iter().find(|c| c.is_self).ok_or_else(unavailable)?;
    if id == "lock" {
        api.lock_focus_to(card.device)?;
        return Ok(None);
    }
    if id == "unlock" {
        api.set_focus_locked(false)?;
        return Ok(None);
    }
    if matches!(id, "keyboard-local" | "keyboard-shared")
        && card.status.as_ref().is_some_and(|s| s.keyboard.disabled == Some(true))
    {
        // Unlock preserves the saved mode. An explicit mode selection then sets
        // its requested mode using a fresh owner revision rather than a stale token.
        api.set_keyboard_disabled(card.device, false)?;
        view = api.device_cards()?;
    }
    if id == "audio-local"
        && view
            .cards
            .iter()
            .find(|c| c.is_self)
            .and_then(|c| c.status.as_ref())
            .is_some_and(|s| s.speaker.resource.configured_mode == Some(SpeakerMode::Receiver))
    {
        // Relinquishing the exit alone preserves the old source preference.
        // Clear it first so choosing Local cannot start forwarding to a peer
        // when that peer becomes the next exit.
        api.set_audio_settings(
            serde_json::from_value(serde_json::json!({
                "forwarding_enabled": false
            }))
            .expect("static audio preference patch"),
        )?;
        view = api.device_cards()?;
    }
    let card = view.cards.iter().find(|c| c.is_self).ok_or_else(unavailable)?;
    let status = card.status.as_ref();
    let action = match id {
        "pause" | "resume" => card.pause.as_ref().map(|p| &p.action),
        "hide-screens" => status.map(|s| &s.actions.screen_local),
        "show-screens" => status.map(|s| &s.actions.screen_shared),
        "keyboard-disabled" => status.map(|s| &s.actions.keyboard_disabled),
        "keyboard-local" => status.map(|s| &s.actions.keyboard_local),
        "keyboard-shared" => status.map(|s| &s.actions.keyboard_shared),
        "mouse-disabled" => status.map(|s| &s.actions.mouse_disabled),
        "mouse-local" => status.map(|s| &s.actions.mouse_local),
        "mouse-shared" => status.map(|s| &s.actions.mouse_shared),
        "game-auto" => status.map(|s| &s.game.actions.auto),
        "game-on" => status.map(|s| &s.game.actions.on),
        "game-off" => status.map(|s| &s.game.actions.off),
        "audio-exit" => status.map(|s| &s.speaker.make_exit_action),
        "audio-forward" => status.map(|s| &s.actions.forwarding_enabled),
        "audio-local" => status.map(|s| {
            if s.speaker.resource.configured_mode == Some(SpeakerMode::Receiver) {
                &s.speaker.resource.action
            } else {
                &s.actions.forwarding_disabled
            }
        }),
        _ => None,
    };
    let Some(CardAction::Invoke { token, .. }) = action else {
        return Err(unavailable());
    };
    let request_id = crate::device_controls::runtime::fresh_request_id();
    let reply = if matches!(id, "pause" | "resume") {
        api.set_device_paused(request_id, token.clone(), id == "pause")?
    } else {
        api.invoke_device_card_action(request_id, token.clone())?
    };
    if let DeviceControlReply::Completed(done) = reply {
        match done.result {
            DeviceControlResult::Applied | DeviceControlResult::NoChange => {}
            _ => {
                return Err(ApiError::Unavailable {
                    message: format!("native control refused: {:?}", done.result),
                });
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cards() -> DeviceCardsView {
        let mut projector = Projector::new().unwrap();
        let inputs = CardInputs {
            group: None,
            members: vec![CardMember {
                device: oink_foundation::DeviceId([1; 32]).into(),
                name: "This device".into(),
                os: "macos".into(),
                is_self: true,
                ready: true,
                offline_reason: OfflineKind::Disconnected,
                last_seen_ms: None,
                rtt_ms: None,
                live_boot: Some([1; 16]),
                facts: None,
                capabilities: vec![],
            }],
            focus: FocusObservation::default(),
            exit_baseline: false,
            local_exit_selection_ready: false,
            selected_exit: None,
            exit_claim: None,
            emergency_delivery: None,
        };
        (*projector.update(&inputs, 0).unwrap()).clone()
    }
    fn item<'a>(model: &'a Model, id: &str) -> &'a Item {
        model
            .tasks
            .iter()
            .chain(model.groups.iter().flat_map(|g| &g.items))
            .find(|i| i.id == id)
            .unwrap()
    }
    #[test]
    fn native_menu_keeps_unknown_modes_unchecked_and_unavailable() {
        let model = model(&cards(), UiLanguage::English);
        assert_eq!(
            model.groups.iter().map(|g| g.id).collect::<Vec<_>>(),
            ["keyboard", "mouse", "game", "audio"]
        );
        for id in [
            "keyboard-disabled",
            "keyboard-local",
            "keyboard-shared",
            "mouse-disabled",
            "mouse-local",
            "mouse-shared",
        ] {
            assert_eq!(item(&model, id).checked, Some(false));
            assert!(!item(&model, id).enabled);
        }
        assert!(
            !model
                .groups
                .iter()
                .flat_map(|g| &g.items)
                .any(|i| i.id == "audio-forward")
        );
        assert!(item(&model, "show").enabled);
    }
    #[test]
    fn native_menu_uses_configured_modes_and_absolute_pause_actions() {
        let mut view = cards();
        let status = view.cards[0].status.as_mut().unwrap();
        status.mouse.configured_mode = Some(SharingMode::Disabled);
        status.actions.mouse_disabled = CardAction::Invoke {
            command: CardCommand::SetLocalOption,
            token: "mouse".into(),
        };
        status.keyboard.disabled = Some(true);
        status.keyboard.resource.configured_mode = Some(SharingMode::Local);
        status.game.preference = Some(GamePreference::Off);
        view.cards[0].pause = Some(LocalPauseCard {
            action: CardAction::Invoke {
                command: CardCommand::SetDevicePaused,
                token: "pause".into(),
            },
            paused: true,
            can_resume: true,
            reason: PauseReason::LocalUser,
            revision: DecimalU64(1),
        });
        let model = model(&view, UiLanguage::English);
        for id in ["mouse-disabled", "keyboard-disabled", "game-off"] {
            assert_eq!(item(&model, id).checked, Some(true));
        }
        assert_eq!(item(&model, "keyboard-local").checked, Some(false));
        assert!(item(&model, "mouse-disabled").enabled);
        assert_eq!(item(&model, "resume").label, "Resume sharing");
    }
    #[test]
    fn native_menu_audio_forwarding_appears_only_with_a_group_exit() {
        let mut view = cards();
        view.exit_volume = Some(ExitVolumeCard {
            receiver_device: oink_foundation::DeviceId([2; 32]).into(),
            receiver_name: "Other device".into(),
            output_name: None,
            volume_percent: None,
            muted: None,
            condition: ExitVolumeCondition::StateUnknown,
            version_token: None,
            can_set: false,
            refusal: None,
        });
        let menu = model(&view, UiLanguage::SimplifiedChinese);
        assert_eq!(
            menu.groups.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(),
            ["键盘", "鼠标", "游戏模式", "音频"]
        );
        assert_eq!(item(&menu, "audio-forward").label, "转发");
        assert!(!item(&menu, "audio-forward").enabled);
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    #[test]
    fn launch_accepts_only_one_fixed_command_without_trailing_arguments() {
        let command = |args: &[&str]| launch_command(args.iter().map(|s| s.to_string()));
        assert_eq!(command(&[ARGUMENT, "mouse-disabled"]), Some("mouse-disabled".into()));
        assert_eq!(command(&[ARGUMENT, "pause"]), Some("pause".into()));
        for args in [
            vec![],
            vec![ARGUMENT],
            vec![ARGUMENT, "arbitrary"],
            vec![ARGUMENT, "show", "extra"],
            vec!["--background", "show"],
        ] {
            assert_eq!(command(&args), None);
        }
    }
}

// The live-node fixture uses debug-only test controls; parser tests above run in both profiles.
#[cfg(all(test, debug_assertions))]
mod dispatch_tests {
    use super::*;
    use crate::{
        api::AgentApi,
        snapshot_tests::{Displays, start_with_clip, until},
    };
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_exit_choice_remains_checked_when_output_is_unavailable() {
        let home = tempfile::tempdir().unwrap();
        let net = oink_test_support::network_config(home.path().into(), "synthetic", 0);
        let node = start_with_clip(&net, &Displays::new("screen"), None).await;
        let n = node.clone();
        tokio::task::spawn_blocking(move || {
            n.set_audio_settings(serde_json::from_value(serde_json::json!({"forwarding_enabled": true})).unwrap())
        })
        .await
        .unwrap()
        .unwrap();
        until("exit action ready", || {
            model(&node.device_cards().unwrap(), UiLanguage::English)
                .groups
                .iter()
                .flat_map(|g| &g.items)
                .any(|i| i.id == "audio-exit" && i.enabled)
        })
        .await;
        let n = node.clone();
        tokio::task::spawn_blocking(move || dispatch(&n, "audio-exit"))
            .await
            .unwrap()
            .unwrap();
        until("native exit selected", || {
            model(&node.device_cards().unwrap(), UiLanguage::English)
                .groups
                .iter()
                .flat_map(|g| &g.items)
                .any(|i| i.id == "audio-exit" && i.checked == Some(true))
        })
        .await;
        // This node deliberately has no native output. The durable exit choice
        // remains selected, while an unavailable primary action stays disabled.
        let menu = model(&node.device_cards().unwrap(), UiLanguage::English);
        let local = menu
            .groups
            .iter()
            .flat_map(|g| &g.items)
            .find(|i| i.id == "audio-local")
            .unwrap();
        assert!(!local.enabled);
        let n = node.clone();
        assert!(
            tokio::task::spawn_blocking(move || dispatch(&n, "audio-local"))
                .await
                .unwrap()
                .is_err()
        );
        assert!(node.audio_status().unwrap().settings.forwarding_enabled);
        node.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_commands_apply_requested_modes_and_pause_without_toggling_twice() {
        let home = tempfile::tempdir().unwrap();
        let net = oink_test_support::network_config(home.path().into(), "synthetic", 0);
        let node = start_with_clip(&net, &Displays::new("screen"), None).await;
        for id in [
            "keyboard-disabled",
            "keyboard-local",
            "mouse-disabled",
            "mouse-shared",
            "pause",
            "resume",
        ] {
            until("native action is available", || {
                let menu = model(&node.device_cards().unwrap(), UiLanguage::English);
                menu.tasks
                    .iter()
                    .chain(menu.groups.iter().flat_map(|g| &g.items))
                    .any(|i| i.id == id && i.enabled)
            })
            .await;
            let n = node.clone();
            tokio::task::spawn_blocking(move || dispatch(&n, id))
                .await
                .unwrap()
                .unwrap_or_else(|error| panic!("{id}: {error:?}"));
            // Card actions acknowledge admission before their owner transaction
            // finishes. Observe the same published facts the native menu uses.
            until("native menu reflects completed action", || {
                let menu = model(&node.device_cards().unwrap(), UiLanguage::English);
                match id {
                    "pause" => menu.tasks.iter().any(|item| item.id == "resume"),
                    "resume" => menu.tasks.iter().any(|item| item.id == "pause"),
                    _ => menu
                        .groups
                        .iter()
                        .flat_map(|group| &group.items)
                        .any(|item| item.id == id && item.checked == Some(true)),
                }
            })
            .await;
            match id {
                "keyboard-disabled" => assert!(node.settings().keyboard_disabled),
                "keyboard-local" => {
                    assert!(!node.settings().keyboard_disabled);
                    assert_eq!(
                        node.settings().routing.keyboard_mode,
                        oink_foundation::input::InputMode::Local
                    );
                }
                "mouse-disabled" => assert_eq!(
                    node.settings().routing.mouse_mode,
                    oink_foundation::input::InputMode::Disabled
                ),
                "mouse-shared" => assert_eq!(
                    node.settings().routing.mouse_mode,
                    oink_foundation::input::InputMode::Shared
                ),
                "pause" | "resume" => {
                    let paused = id == "pause";
                    until("native absolute pause applied", || {
                        node.device_cards()
                            .unwrap()
                            .cards
                            .iter()
                            .find(|c| c.is_self)
                            .and_then(|c| c.pause.as_ref())
                            .is_some_and(|p| p.paused == paused)
                    })
                    .await;
                    let n = node.clone();
                    let result = tokio::task::spawn_blocking(move || dispatch(&n, id)).await.unwrap();
                    assert!(
                        result.is_err(),
                        "an obsolete pause/resume item cannot toggle the opposite state"
                    );
                }
                _ => unreachable!(),
            }
        }
        node.shutdown().await;
    }
}
