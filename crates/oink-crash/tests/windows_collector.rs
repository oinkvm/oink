#![cfg(windows)]
use oink_telemetry::{capture::Manifest, *};
#[test]
fn native_child_is_extracted_locally_and_raw_dump_deleted() {
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
        images: vec![],
    };
    let consent = ReportConsent {
        choice: Choice::Granted,
        policy_version: 3,
        generation: 1,
    };
    if let Some(root) = std::env::var_os(KEY) {
        let root = std::path::PathBuf::from(root);
        let capture = oink_crash::capsule::Capture::install();
        capture
            .arm(&root.join("numeric.bin"), 1, Component::Agent, build)
            .unwrap();
        let _collector = oink_crash::collector::Collector::start(
            std::path::Path::new(env!("CARGO_BIN_EXE_oink-crash-collector")),
            &root.join("raw"),
            consent,
            build,
            &manifest,
            Component::Agent,
        )
        .unwrap();
        // SAFETY: fatal exception in this isolated child; no installed application.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(
                windows_sys::Win32::System::Diagnostics::Debug::SEM_NOGPFAULTERRORBOX,
            );
            windows_sys::Win32::System::Diagnostics::Debug::RaiseException(0xe000_beef, 0, 0, std::ptr::null());
        }
        std::process::exit(4);
    }
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_child_is_extracted_locally_and_raw_dump_deleted",
            "--nocapture",
        ])
        .env(KEY, dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let raw = dir.path().join("raw");
    let projected = oink_crash::staging::recover(&raw, consent, &manifest);
    assert_eq!(projected.len(), 1, "collector must produce one numeric native report");
    assert_eq!(projected[0].0.exception_code, Some(0xe000_beef));
    assert!(
        projected[0].0.frames().next().is_none(),
        "unshipped frames must be omitted"
    );
    assert!(
        std::fs::read_dir(&raw)
            .unwrap()
            .all(|f| !f.unwrap().path().extension().is_some_and(|e| e == "dmp"))
    );
}
