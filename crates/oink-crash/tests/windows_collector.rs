#![cfg(windows)]
use oink_telemetry::{capture::Manifest, *};
#[test]
fn native_child_is_extracted_locally_and_raw_dump_deleted() {
    run_crash(false);
}
#[test]
fn native_child_local_diagnostics_does_not_require_report_consent() {
    run_crash(true);
}
fn run_crash(local_only: bool) {
    const KEY: &str = "OINK_COLLECTOR_UNIT_CHILD";
    let build = BuildContext {
        app_version: AppVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 1,
        },
        os_version_class: OsVersionClass::Windows11,
    };
    let manifest = Manifest {
        version: build.app_version,
        images: vec![oink_crash::dump::executable_image(&std::env::current_exe().unwrap(), ImageSlot::Agent).unwrap()],
    };
    let consent = ReportConsent {
        choice: if local_only { Choice::Denied } else { Choice::Granted },
        policy_version: 3,
        generation: 1,
    };
    if let Some(root) = std::env::var_os(KEY) {
        let root = std::path::PathBuf::from(root);
        let capture = oink_crash::capsule::Capture::install();
        capture
            .arm(&root.join("numeric.bin"), 1, Component::Agent, build)
            .unwrap();
        let _collector = oink_crash::collector::Collector::start_with_local(
            std::path::Path::new(env!("CARGO_BIN_EXE_oink-crash-collector")),
            &root.join("raw"),
            consent,
            build,
            &manifest,
            Component::Agent,
            Some(oink_crash::local::Target {
                root: root.join("local"),
                instance: "abcdef012345abcdef012345".into(),
            }),
        )
        .unwrap();
        // SAFETY: fatal exception in this isolated child; no installed application.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(
                windows_sys::Win32::System::Diagnostics::Debug::SEM_NOGPFAULTERRORBOX,
            );
            crash_outer();
        }
        std::process::exit(4);
    }
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            if local_only {
                "native_child_local_diagnostics_does_not_require_report_consent"
            } else {
                "native_child_is_extracted_locally_and_raw_dump_deleted"
            },
            "--nocapture",
        ])
        .env(KEY, dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let raw = dir.path().join("raw");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(16);
    while !(if local_only {
        oink_crash::local::read(&dir.path().join("local")).is_some_and(|s| s.crash.frames().count() > 1)
    } else {
        raw.join("0.safe").exists()
    }) && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let summary =
        oink_crash::local::read(&dir.path().join("local")).expect("local diagnostic summary from the real child");
    assert!(
        summary.crash.frames().count() > 1,
        "real local unwind must retain multiple shipped-image frames: {:?}",
        summary.crash
    );
    println!(
        "local_only={local_only}; code={:?}; shipped_frames={}; debug_age={:?}",
        summary.crash.exception_code,
        summary.crash.frames().count(),
        summary.crash.frames().next()
    );
    assert!(
        std::fs::read_dir(&raw)
            .unwrap()
            .all(|f| f.unwrap().path().extension().is_none_or(|e| e != "dmp"))
    );
    if local_only {
        assert!(!raw.join("0.safe").exists());
        let later_grant = ReportConsent {
            choice: Choice::Granted,
            generation: 2,
            ..consent
        };
        assert!(
            oink_crash::staging::recover(&raw, later_grant, &manifest).is_empty(),
            "diagnostics-only crash must never be backfilled after a later grant"
        );
        return;
    }
    let projected = oink_crash::staging::recover(&raw, consent, &manifest);
    assert_eq!(projected.len(), 1, "collector must produce one numeric native report");
    assert_eq!(projected[0].0.exception_code, Some(0xe000_beef));
    assert!(
        projected[0].0.frames().count() > 1,
        "real local unwind must retain multiple shipped-image frames: {:?}",
        projected[0].0
    );
    assert!(
        std::fs::read_dir(&raw)
            .unwrap()
            .all(|f| !f.unwrap().path().extension().is_some_and(|e| e == "dmp"))
    );
}

#[inline(never)]
unsafe fn crash_inner() {
    // SAFETY: only called in the isolated fatal child.
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::RaiseException(0xe000_beef, 0, 0, std::ptr::null());
    }
    std::hint::black_box(1);
}
#[inline(never)]
unsafe fn crash_outer() {
    // SAFETY: only called in the isolated fatal child.
    unsafe {
        crash_inner();
    }
    std::hint::black_box(2);
}
