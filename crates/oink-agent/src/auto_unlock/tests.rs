use super::*;
use oink_foundation::DeviceId;
use std::time::{Duration, Instant};
const LOCAL: DeviceId = DeviceId([1; 32]);
const SOURCE: DeviceId = DeviceId([2; 32]);
const OTHER: DeviceId = DeviceId([3; 32]);

#[test]
fn locked_while_holding_focus_requires_exit_then_entry() {
    let now = Instant::now();
    let mut gate = EntryGate::default();
    gate.observe(Some(true), LOCAL, LOCAL);
    assert!(gate.enter_with_reason(SOURCE, 1, now).is_err());
    gate.observe(Some(true), SOURCE, LOCAL);
    assert!(gate.enter_with_reason(SOURCE, 2, now).is_ok());
    assert_eq!(gate.confirm(SOURCE, 2, true, now), Some(SOURCE));
    assert!(gate.authorized(SOURCE, now + Duration::from_secs(5)));
    assert!(!gate.authorized(SOURCE, now + Duration::from_secs(6)));
    assert_eq!(gate.confirm(SOURCE, 2, true, now), None);
    assert!(gate.enter_with_reason(SOURCE, 2, now).is_err());
}

#[test]
fn entry_rejection_reason_identifies_each_gate() {
    let now = Instant::now();
    let mut gate = EntryGate::default();
    assert_eq!(gate.enter_with_reason(SOURCE, 1, now), Err(EntryRejection::Locked));
    gate.observe(Some(true), LOCAL, LOCAL);
    assert_eq!(gate.enter_with_reason(SOURCE, 2, now), Err(EntryRejection::Armed));
    gate.observe(Some(true), SOURCE, LOCAL);
    assert_eq!(gate.enter_with_reason(SOURCE, 3, now), Ok(()));
    gate.observe(Some(true), SOURCE, LOCAL);
    assert_eq!(gate.enter_with_reason(SOURCE, 2, now), Err(EntryRejection::Stale));
    assert_eq!(EntryRejection::NotAttached.diagnostic_code(), 5);
}
#[test]
fn source_must_answer_for_itself_once_while_unlocked_and_before_deadline() {
    let now = Instant::now();
    let mut gate = EntryGate::default();
    gate.observe(Some(true), SOURCE, LOCAL);
    assert!(gate.enter_with_reason(SOURCE, 5, now).is_ok());
    assert_eq!(gate.confirm(OTHER, 5, true, now), None);
    assert_eq!(gate.confirm(SOURCE, 4, true, now), None);
    assert_eq!(gate.confirm(SOURCE, 5, false, now), None);
    assert_eq!(gate.confirm(SOURCE, 5, true, now), None);
    gate.observe(Some(true), SOURCE, LOCAL);
    assert!(gate.enter_with_reason(SOURCE, 6, now).is_ok());
    assert_eq!(gate.confirm(SOURCE, 6, true, now + Duration::from_secs(6)), None);
}
#[test]
fn unlocking_unknown_or_leaving_cancels_pending() {
    for state in [None, Some(false)] {
        let now = Instant::now();
        let mut gate = EntryGate::default();
        gate.observe(Some(true), SOURCE, LOCAL);
        assert!(gate.enter_with_reason(SOURCE, 1, now).is_ok());
        gate.observe(state, LOCAL, LOCAL);
        assert_eq!(gate.confirm(SOURCE, 1, true, now), None);
    }
    let now = Instant::now();
    let mut gate = EntryGate::default();
    gate.observe(Some(true), SOURCE, LOCAL);
    assert!(gate.enter_with_reason(SOURCE, 7, now).is_ok());
    gate.observe(Some(true), OTHER, LOCAL);
    assert_eq!(gate.confirm(SOURCE, 7, true, now), None);
}
#[test]
fn password_debug_output_is_redacted_even_inside_api_envelopes() {
    let password: Password = serde_json::from_str("\"synthetic-only-secret\"").unwrap();
    let request = crate::api::Request::SaveAutoUnlock { password };
    assert!(!format!("{request:?}").contains("synthetic-only-secret"));
    assert!(
        serde_json::to_string(&request)
            .unwrap()
            .contains("synthetic-only-secret")
    );
}

#[test]
fn malformed_password_values_are_not_echoed_into_errors() {
    for secret in [r#"{"synthetic-only-secret":"value"}"#, r#"["synthetic-only-secret"]"#] {
        let result = serde_json::from_str::<Password>(secret).unwrap_err().to_string();
        assert!(!result.contains("synthetic-only-secret"));
        assert!(result.contains("invalid password value"));
    }
}

#[cfg(windows)]
#[test]
fn logon_failure_is_reported_as_incorrect_password() {
    let error = std::io::Error::from_raw_os_error(1326);
    assert_eq!(save_error(&error), "密码不正确");
    assert_eq!(save_error(&std::io::Error::other("storage")), "无法保存登录密码");
}

#[test]
fn prelogin_is_never_an_unlocked_source_even_if_wts_says_unlocked() {
    for locked in [Some(true), Some(false), None] {
        assert!(!source_unlocked(crate::node::Profile::Prelogin, locked));
    }
    assert!(source_unlocked(crate::node::Profile::User, Some(false)));
    assert!(!source_unlocked(crate::node::Profile::User, None));
}

#[test]
fn failure_categories_are_stable_and_do_not_contain_display_copy() {
    let cases = [
        (FailureKind::IncorrectPassword, "password_incorrect"),
        (FailureKind::WindowsHelloOnly, "windows_hello_only"),
        (FailureKind::LoginUiNotReady, "login_ui_not_ready"),
        (FailureKind::Unknown, "unknown"),
    ];
    for (kind, wire) in cases {
        assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
        assert!(!serde_json::to_string(&kind).unwrap().contains('请'));
    }
}

#[test]
fn persisted_failure_round_trips_across_restart_and_is_cleared_by_save() {
    let dir = tempfile::tempdir().unwrap();
    persist_failure(dir.path(), FailureKind::WindowsHelloOnly).unwrap();
    assert_eq!(read_failure(dir.path()), Some(FailureKind::WindowsHelloOnly));
    clear_failure(dir.path()).unwrap();
    assert_eq!(read_failure(dir.path()), None);
}
