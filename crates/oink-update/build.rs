fn main() {
    println!("cargo:rerun-if-env-changed=OINK_WINDOWS_UPDATE_TEST_FEED");
    println!("cargo:rerun-if-env-changed=OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH");
    let origin = std::env::var("OINK_WINDOWS_UPDATE_TEST_FEED").ok();
    let fail_health = std::env::var("OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH").ok();
    if std::env::var_os("CARGO_FEATURE_TEST_FEED").is_some() {
        let origin = origin.expect("test-feed requires an explicit loopback origin");
        let port = origin
            .strip_prefix("http://127.0.0.1:")
            .expect("test feed must use literal IPv4 loopback");
        assert!(
            port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|p| p != 0),
            "test feed requires one port without path/query/userinfo"
        );
        println!("cargo:rustc-env=OINK_WINDOWS_UPDATE_TEST_FEED={origin}");
        assert!(
            matches!(fail_health.as_deref(), None | Some("1")),
            "test health fault must be absent or 1"
        );
        println!(
            "cargo:rustc-env=OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH={}",
            fail_health.as_deref().unwrap_or("0")
        );
    } else {
        assert!(
            fail_health.is_none(),
            "test health fault requires the explicit test-feed feature"
        );
        assert!(
            origin.is_none(),
            "test feed origin requires the explicit test-feed feature"
        );
    }
}
