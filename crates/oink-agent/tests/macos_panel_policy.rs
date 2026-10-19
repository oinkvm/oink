//! Intent checks only; installed AppKit behavior requires native acceptance.
use oink_foundation::panel_policy::PanelPolicy;

#[test]
fn backend_feedback_survives_application_hiding_without_activation() {
    let policy = PanelPolicy::BACKEND_OVERLAY;
    assert!(policy.nonactivating);
    assert!(!policy.hides_on_deactivate);
    assert!(!policy.can_hide);
}

#[test]
fn ui_popover_is_nonactivating_and_follows_ui_application_hiding() {
    let policy = PanelPolicy::WEB_POPOVER;
    assert!(policy.nonactivating);
    assert!(!policy.hides_on_deactivate);
    assert!(policy.can_hide);
}
