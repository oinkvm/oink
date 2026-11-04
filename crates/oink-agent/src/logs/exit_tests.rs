use super::*;

#[test]
fn role_exit_fixture() {
    let Ok(mode) = std::env::var("OINK_LOG_EXIT_FIXTURE") else {
        return;
    };
    if mode == "hook_abort" {
        // Model an abort-profile panic: no unwinding and no entry guard Drop.
        std::panic::set_hook(Box::new(|_| std::process::abort()));
    }
    let code = crate::telemetry::fatal_boundary(|| {
        match std::env::var("OINK_LOG_ROLE").unwrap().as_str() {
            "oink" => init_service(),
            "oink-ui" => init_ui(),
            "oink-prelogin" => init_prelogin(),
            "oink-update-worker" => init_update(),
            _ => unreachable!(),
        }
        // Production installs its crash hook after ordinary logging. It must
        // preserve local panic output without copying that payload into reports.
        let _capture = oink_crash::capsule::Capture::install();
        for n in 0..2048 {
            tracing::info!(n, "queued startup record");
        }
        if mode == "caught" {
            let _ = std::panic::catch_unwind(|| panic!("caught panic fixture"));
        }
        assert_eq!(
            crate::host::service_exit_code(Err(anyhow::anyhow!("last failure before return"))),
            1
        );
        if mode == "panic" || mode == "hook_abort" {
            panic!("startup panic fixture");
        }
        if mode == "direct_exit" {
            flush();
            std::process::exit(1);
        }
        u8::from(mode == "error")
    });
    // Exit immediately: a test harness finishing later must not mask lost lines.
    std::process::exit(code.into());
}

#[test]
fn every_role_drains_final_failure_and_panic_before_process_exit() {
    for role in ["oink", "oink-ui", "oink-prelogin", "oink-update-worker"] {
        for mode in ["return", "error", "caught", "panic", "hook_abort", "direct_exit"] {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "logs::exit_tests::role_exit_fixture", "--nocapture"])
                .env("OINK_HOME", dir.path())
                .env("OINK_LOG_EXIT_FIXTURE", mode)
                .env("OINK_LOG_ROLE", role)
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                matches!(mode, "return" | "caught"),
                "{role}/{mode}: {output:?}"
            );
            let logs = std::fs::read_dir(dir.path().join("Logs"))
                .unwrap()
                .map(Result::unwrap)
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
                .map(|e| std::fs::read_to_string(e.path()).unwrap())
                .collect::<String>();
            assert_eq!(logs.matches("queued startup record").count(), 2048, "{role}/{mode}");
            assert!(logs.contains("last failure before return"), "{role}/{mode}: {logs}");
            if mode == "panic" || mode == "hook_abort" {
                assert!(logs.contains("startup panic fixture"), "{logs}");
            }
            if mode == "caught" {
                assert!(logs.contains("caught panic fixture"), "{logs}");
            }
        }
    }
}
