use super::*;
use oink_protocol::v1::dto::LocalOption;
use oink_test_support::input::FakeInput;

fn options(data_dir: &std::path::Path) -> NodeOptions {
    NodeOptions {
        net: oink_test_support::network_config(data_dir.to_path_buf(), "synthetic", 0),
        input: Arc::new(FakeInput::new()),
        displays: Vec::new(),
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    }
}

#[tokio::test]
async fn new_boot_restores_disabled_sources_to_shared_and_preserves_enabled_modes() {
    use oink_foundation::input::InputMode;
    use oink_protocol::v1::schema::WireValue;

    for keyboard in [InputMode::Disabled, InputMode::Local, InputMode::Shared] {
        for mouse in [InputMode::Disabled, InputMode::Local, InputMode::Shared] {
            let home = tempfile::tempdir().unwrap();
            let mut config = AgentConfig::load_from(home.path()).unwrap();
            config
                .update(|settings| {
                    settings.routing.keyboard_mode = keyboard;
                    settings.routing.mouse_mode = mouse;
                })
                .unwrap();
            let revisions = [LocalOption::KeyboardMode, LocalOption::MouseMode]
                .map(|option| config.owner().revision(option).unwrap());
            let mut previous_boot = None;
            for _ in 0..2 {
                let node = Node::prepare(options(home.path())).unwrap();
                assert_ne!(Some(node.inner.boot.nonce), previous_boot);
                previous_boot = Some(node.inner.boot.nonce);
                for config in [
                    lock(&node.inner.config).clone(),
                    AgentConfig::load_from(home.path()).unwrap(),
                ] {
                    for (option, before, actual, revision) in [
                        (
                            LocalOption::KeyboardMode,
                            keyboard,
                            config.settings.routing.keyboard_mode,
                            revisions[0],
                        ),
                        (
                            LocalOption::MouseMode,
                            mouse,
                            config.settings.routing.mouse_mode,
                            revisions[1],
                        ),
                    ] {
                        let expected = if before == InputMode::Disabled {
                            InputMode::Shared
                        } else {
                            before
                        };
                        assert_eq!(actual, expected);
                        assert_eq!(
                            config.owner().value(option),
                            Some((if expected == InputMode::Shared { 0u32 } else { 1u32 }).write())
                        );
                        assert_eq!(
                            config.owner().revision(option),
                            Some(revision + u64::from(before == InputMode::Disabled))
                        );
                    }
                    assert_eq!(config.owner().value(LocalOption::KeyboardDisabled), Some(false.write()));
                }
                assert!(node.inner.input.get().is_none());
                assert!(node.inner.links.get().is_none());
                assert!(node.inner.group.get().is_none());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_disables_never_enter_started_input_or_published_owner_facts() {
    use oink_foundation::input::InputMode;
    use oink_protocol::v1::schema::WireValue;
    for mouse in [InputMode::Disabled, InputMode::Local] {
        let home = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(home.path()).unwrap();
        config
            .update(|settings| {
                settings.routing.keyboard_mode = InputMode::Disabled;
                settings.routing.mouse_mode = mouse;
            })
            .unwrap();
        let node = Node::start_with_fake_platform(options(home.path())).await.unwrap();
        let expected = if mouse == InputMode::Disabled {
            InputMode::Shared
        } else {
            InputMode::Local
        };
        assert!(!node.input_status().unwrap().keyboard_disabled);
        assert_eq!(node.input_status().unwrap().keyboard_mode, InputMode::Shared);
        assert_eq!(node.input_status().unwrap().mouse_mode, expected);
        let preferences = node
            .link_config()
            .unwrap()
            .local_facts()
            .preferences()
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(
            preferences
                .iter()
                .find(|p| p.option() == Ok(LocalOption::KeyboardDisabled))
                .unwrap()
                .value()
                .unwrap(),
            false.write()
        );
        assert_eq!(
            preferences
                .iter()
                .find(|p| p.option() == Ok(LocalOption::KeyboardMode))
                .unwrap()
                .value()
                .unwrap(),
            0u32.write()
        );
        assert_eq!(
            preferences
                .iter()
                .find(|p| p.option() == Ok(LocalOption::MouseMode))
                .unwrap()
                .value()
                .unwrap(),
            (if expected == InputMode::Shared { 0u32 } else { 1u32 }).write()
        );
        node.shutdown().await;
    }
}
