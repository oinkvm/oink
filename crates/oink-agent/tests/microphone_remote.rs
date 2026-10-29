//! Installation commands must reach the service, never trait-default stubs.
use oink_agent::{
    api::{AgentApi, ApiError},
    microphone_plugin::Action,
    remote::RemoteApi,
};

#[test]
fn microphone_commands_require_the_service_on_both_platforms() {
    let dir = tempfile::tempdir().unwrap();
    let remote = RemoteApi::new(dir.path());
    assert!(matches!(
        remote.microphone_plugin_status(),
        Err(ApiError::Unavailable { .. })
    ));
    for action in [Action::Install, Action::Uninstall] {
        assert!(matches!(
            remote.microphone_plugin_action(action),
            Err(ApiError::Unavailable { .. })
        ));
    }
}

#[test]
fn microphone_mode_and_gain_use_the_shared_ipc_forwarders() {
    let dir = tempfile::tempdir().unwrap();
    let remote = RemoteApi::new(dir.path());
    assert!(matches!(remote.device_cards(), Err(ApiError::Unavailable { .. })));
    assert!(matches!(
        remote.invoke_device_card_action("request".into(), "microphone-token".into()),
        Err(ApiError::Unavailable { .. })
    ));
    assert!(matches!(
        remote.set_forwarding_controls("request".into(), "microphone-gain-token".into(), 80, false),
        Err(ApiError::Unavailable { .. })
    ));
}
