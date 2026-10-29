use super::*;

#[test]
fn progress_path_matches_powershell_with_missing_or_unexpanded_environment() {
    const EXPECTED: &str = "OINK_TEST_MICROPHONE_PROGRESS_PATH";
    if let Some(expected) = std::env::var_os(EXPECTED) {
        assert_eq!(progress_path().unwrap(), PathBuf::from(expected));
        return;
    }
    // Query only. Neither this test nor its isolated children writes ProgramData.
    let output = Command::new(powershell().unwrap())
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "[Console]::Write((Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'OinkMicrophoneProgress/status.json'))"])
        .creation_flags(0x08000000)
        .output().unwrap();
    assert!(output.status.success());
    let expected = String::from_utf8(output.stdout).unwrap();
    for value in [None, Some("%SystemDrive%\\ProgramData"), Some("Z:\\unrelated-progress")] {
        // Change only a child process's environment; other tests remain isolated.
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", "microphone_plugin::windows::progress_tests::progress_path_matches_powershell_with_missing_or_unexpanded_environment", "--nocapture"])
            .env(EXPECTED, &expected).env_remove("ProgramData");
        if let Some(value) = value {
            child.env("ProgramData", value);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "environment={value:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn staged_powershell_steps_reach_reader_before_installer_exits() {
    use MicrophonePluginStep::*;
    for (action, expected) in [
        ("install", vec![Verify, Extract, InstallDriver, Finish]),
        ("uninstall", vec![Verify, UninstallDriver, Finish]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("progress-test.ps1");
        std::fs::write(&harness, include_str!("progress.ps1")).unwrap();
        let token = progress::token().unwrap();
        let mut child = Command::new(powershell().unwrap())
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(harness)
            .arg(action)
            .arg(&token)
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = serde_json::json!({"bootstrap":BOOTSTRAP,"installer":SCRIPT}).to_string();
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let path = dir.path().join("status.json");
        let mut observed = Vec::new();
        let status = wait_for_installer(&mut child, &path, &token, |step| {
            if observed.last() != Some(&step) {
                observed.push(step);
                // The real PowerShell producer cannot advance/exit until this
                // reader acknowledges each receipt. No timing-based dwell.
                let name = serde_json::to_value(step).unwrap().as_str().unwrap().to_owned();
                std::fs::write(dir.path().join(format!("{name}.ack")), []).unwrap();
            }
        })
        .unwrap();
        let mut error = String::new();
        child.stderr.take().unwrap().read_to_string(&mut error).unwrap();
        assert!(status.success(), "{error}");
        assert_eq!(observed, expected);
        assert!(progress(&path, &"0".repeat(32)).is_none(), "stale token accepted");
        assert!(!dir.path().join(format!("installer-{token}.ps1")).exists());
    }
}
