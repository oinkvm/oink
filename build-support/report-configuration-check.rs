// Link only against the exact production release rlib. No Node, UI or backend startup.
fn main() {
    let home = std::env::var_os("OINK_HOME").expect("isolated home required");
    assert!(std::path::Path::new(&home).is_absolute());
    let expected = std::env::var("EXPECTED_VERSION").expect("expected release label required");
    assert_eq!(oink_agent::ipc::ServiceInfo::current().version, expected);
    let boot = oink_ui::boot::BootInfo::new(false);
    assert_eq!(boot.version, expected);
    assert_eq!(
        boot.build.as_deref(),
        Some(std::env::var("EXPECTED_BUILD").unwrap().as_str())
    );
    let owner = oink_agent::telemetry::Owner::new(std::path::Path::new(&home), Default::default());
    let status = owner.status();
    assert!(status.configured, "packaged report configuration absent");
    assert!(!status.consent.granted(), "probe must never grant delivery consent");
    assert_eq!(status.queued_crashes, 0);
    assert_eq!(status.queued_performance, 0);
    assert!(matches!(
        status.rust_crash,
        oink_agent::telemetry::Coverage::NumericCapsule
    ));
    assert!(matches!(
        status.native_crash,
        oink_agent::telemetry::Coverage::NumericCapsule
    ));
    println!(
        "{{\"configured\":true,\"consent_granted\":false,\"queued_crashes\":0,\"queued_performance\":0,\"rust_crash\":\"numeric_capsule\",\"native_crash\":\"numeric_capsule\",\"report_sent\":false,\"version_label\":\"{}\",\"ui_build\":\"{}\"}}",
        boot.version,
        boot.build.as_deref().unwrap()
    );
}
