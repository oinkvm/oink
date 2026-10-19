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
async fn new_boot_durably_releases_keyboard_before_starting_owners() {
    let home = tempfile::tempdir().unwrap();
    let mut config = AgentConfig::load_from(home.path()).unwrap();
    config.update(|settings| settings.keyboard_disabled = true).unwrap();
    let previous = config.owner().revision(LocalOption::KeyboardDisabled).unwrap();
    let first = Node::prepare(options(home.path())).unwrap();
    let first_boot = first.inner.boot.nonce;
    for config in [
        lock(&first.inner.config).clone(),
        AgentConfig::load_from(home.path()).unwrap(),
    ] {
        assert!(!config.settings.keyboard_disabled);
        assert_eq!(
            config.owner().revision(LocalOption::KeyboardDisabled),
            Some(previous + 1)
        );
    }
    assert!(first.inner.input.get().is_none());
    assert!(first.inner.links.get().is_none());
    assert!(first.inner.group.get().is_none());
    drop(first);

    let second = Node::prepare(options(home.path())).unwrap();
    assert_ne!(second.inner.boot.nonce, first_boot);
    let disk = AgentConfig::load_from(home.path()).unwrap();
    assert!(!disk.settings.keyboard_disabled);
    assert_eq!(disk.owner().revision(LocalOption::KeyboardDisabled), Some(previous + 2));
    assert_eq!(
        lock(&second.inner.config)
            .owner()
            .revision(LocalOption::KeyboardDisabled),
        Some(previous + 2)
    );
    assert!(second.inner.input.get().is_none());
    assert!(second.inner.links.get().is_none());
}
