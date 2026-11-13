use super::*;
use oink_test_support::input::FakeInput;

fn options(data_dir: &std::path::Path) -> NodeOptions {
    NodeOptions {
        net: oink_test_support::network_config(data_dir.to_path_buf(), "prelogin", 0),
        input: Arc::new(FakeInput::new()),
        displays: Vec::new(),
        display_backend: None,
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: false,
        cursor_trail: false,
    }
}

#[test]
fn user_profile_installs_everything_and_prelogin_only_network_routing_display_input() {
    let user = Profile::User.components();
    assert!(
        user.content
            && user.audio
            && user.companions
            && user.pairing
            && user.reports
            && user.create_identity
            && user.text_focus
    );
    let prelogin = Profile::Prelogin.components();
    assert_eq!(
        prelogin,
        Components {
            content: false,
            audio: false,
            companions: false,
            pairing: false,
            reports: false,
            create_identity: false,
            text_focus: false,
        }
    );
}

/// Review M4: merged v2 capabilities name protocol domains. The pre-login
/// backend serves device resources/options, the system arrangement and its
/// diagnostics recorder as 0.3.x pre-login did, so it keeps announcing
/// device/2, layout/2 and diagnostics/2; content and audio stay out.
#[test]
fn prelogin_offer_keeps_device_layout_and_diagnostics_and_drops_content_and_audio() {
    let user = Node::offer_profile_for(Profile::User);
    assert_eq!(user.capabilities, Node::offer_profile().capabilities);
    let offer = Node::offer_profile_for(Profile::Prelogin);
    for required in oink_protocol::v2::registry::BASELINE.iter().chain(&[
        "oink.relay/1",
        "oink.relay.transit/1",
        "oink.input/2",
        "oink.device/2",
        "oink.layout/2",
        "oink.diagnostics/2",
    ]) {
        assert!(offer.capabilities.iter().any(|c| c == required), "{required}");
    }
    for absent in oink_clipboard::negotiation::CAPABILITIES
        .iter()
        .filter(|c| **c != "oink.clipboard/2")
        .chain(oink_audio::negotiation::CAPABILITIES)
        .chain(&["oink.clipboard/2"])
    {
        assert!(!offer.capabilities.iter().any(|c| c == absent), "{absent}");
    }
    assert!(offer.audio.is_empty());
    assert_eq!(
        offer.formats,
        oink_network::negotiation::OfferProfile::default().formats
    );
}

#[test]
fn prelogin_never_creates_an_identity() {
    let home = tempfile::tempdir().unwrap();
    let error = Node::prepare_profile(options(home.path()), false, Profile::Prelogin).unwrap_err();
    assert!(format!("{error:#}").contains("identity"), "{error:#}");
    assert!(!home.path().join("device.key").exists());
}

#[tokio::test]
async fn prelogin_node_runs_without_content_audio_companions_or_reports() {
    let home = tempfile::tempdir().unwrap();
    let identity = oink_network::identity::Identity::load_or_create(home.path()).unwrap();
    let node = Node::start_prelogin_with_fake_platform(options(home.path()))
        .await
        .unwrap();
    assert_eq!(node.id(), identity.id());
    assert_eq!(node.profile(), Profile::Prelogin);
    assert!(node.links().is_ok());
    assert!(node.input_thread().is_ok());
    assert!(node.link_config().is_ok());
    assert!(node.inner.diagnostics.get().is_some());
    assert!(node.inner.audio.get().is_none());
    assert!(node.inner.dnd.get().is_none());
    assert!(node.inner.clip.get().is_none());
    assert!(node.inner.prelogin_export.get().is_none());
    assert!(!home.path().join("reports").exists());
    node.shutdown().await;
}
