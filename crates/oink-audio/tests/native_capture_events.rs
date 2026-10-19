#[cfg(target_os = "macos")]
#[test]
fn native_diagnostics_preserve_audio_and_never_wait_for_a_consumer() {
    // Actual bridge with synthetic callbacks. No audio service is invoked; this
    // is not evidence of live capture or realtime deadline performance.
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("capture-events");
    let compile = std::process::Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-framework",
            "Foundation",
            "-framework",
            "CoreAudio",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/native_capture_events.m"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    assert!(std::process::Command::new(binary).status().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_cleanup_requires_positive_release_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("capture-cleanup");
    let compile = std::process::Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-std=c11",
            "-Wall",
            "-Werror",
            "-framework",
            "Foundation",
            "-framework",
            "CoreAudio",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/native/tests/macos_capture_cleanup.m"
        ))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    assert!(std::process::Command::new(binary).status().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_tap_never_owns_local_silence() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("capture-tap-mode");
    let compile = std::process::Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-std=c11",
            "-mmacosx-version-min=14.2",
            "-Wall",
            "-Werror",
            "-framework",
            "Foundation",
            "-framework",
            "CoreAudio",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/native/tests/macos_capture_tap_mode.m"
        ))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    assert!(std::process::Command::new(binary).status().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_endpoint_mute_observes_user_takeover_and_exact_device_identity() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("endpoint-mute");
    let compile = std::process::Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-framework",
            "Foundation",
            "-framework",
            "CoreAudio",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/native/tests/macos_endpoint_mute.m"
        ))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    assert!(std::process::Command::new(binary).status().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_exit_volume_callbacks_and_writes_keep_exact_output_identity() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("exit-volume");
    let compile = std::process::Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-framework",
            "Foundation",
            "-framework",
            "CoreAudio",
            "-framework",
            "ApplicationServices",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/native/tests/exit_volume.m"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    assert!(std::process::Command::new(binary).status().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_permission_observation_and_requests_use_fake_tcc() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("capture-permission");
    let compile = std::process::Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-framework",
            "Foundation",
        ])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/native/tests/macos_capture_permission.m"
        ))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
    assert!(std::process::Command::new(binary).status().unwrap().success());
}
