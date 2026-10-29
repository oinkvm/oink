#![cfg(target_os = "macos")]
use oink_telemetry::{
    capture::{Image, Manifest},
    *,
};
use std::{
    process::Command,
    time::{Duration, Instant},
};

/// Opt in explicitly: asks the OS crash reporter to process an isolated child.
/// No installed app, desktop input, application settings or OS report deletion.
#[test]
#[ignore = "real OS crash reporter; run explicitly on a Mac"]
fn native_child_ips_has_matching_uuid_and_multiple_shipped_frames() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("crash.c");
    let executable = dir.path().join("oink-crash-fixture");
    std::fs::write(
        &source,
        br#"
#include <signal.h>
#include <unistd.h>
__attribute__((noinline)) static void inner(void) { raise(SIGSEGV); __asm__ volatile(""); }
__attribute__((noinline)) static void outer(void) { inner(); __asm__ volatile(""); }
int main(void) { sleep(1); outer(); return 0; }
"#,
    )
    .unwrap();
    assert!(
        Command::new("xcrun")
            .args(["clang", "-O0", "-g", "-fno-omit-frame-pointer"])
            .arg(format!("-fdebug-prefix-map={}=oink/test", dir.path().display()))
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let id = oink_crash::macos::executable_uuid(&executable).unwrap();
    use object::{Object, ObjectSegment};
    let image = std::fs::read(&executable).unwrap();
    let object = object::File::parse(image.as_slice()).unwrap();
    let text_size = object
        .segments()
        .find(|s| s.name() == Ok(Some("__TEXT")))
        .unwrap()
        .size();
    let build = BuildContext {
        app_version: AppVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 1,
        },
        os_version_class: OsVersionClass::MacOSMajor(26),
    };
    let manifest = Manifest {
        version: build.app_version,
        images: vec![Image {
            slot: ImageSlot::Agent,
            debug_id: id,
            debug_age: 0,
            code_id: None,
            text_size,
        }],
    };
    let mut child = Command::new(&executable).spawn().unwrap();
    let launched = oink_crash::macos::process_launch(child.id()).unwrap();
    let consent = ReportConsent {
        choice: Choice::Granted,
        policy_version: POLICY_VERSION,
        generation: 1,
    };
    let lifetime = oink_crash::macos::Lifetime {
        schema: 1,
        pid: child.id(),
        launched_at: launched,
        armed_at: launched,
        generation: 1,
        component: Component::Agent,
        image: ImageSlot::Agent,
        debug_id: id,
        build,
    };
    assert!(!child.wait().unwrap().success());
    let reports = std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("Library/Logs/DiagnosticReports");
    let deadline = Instant::now() + Duration::from_secs(45);
    let (crash, at) = loop {
        if let Some(report) = oink_crash::macos::recover(&reports, lifetime, consent, &manifest, now()) {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "OS did not publish an attributable .ips within 45 seconds"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(crash.frames().count() > 1, "{:?}", crash);
    assert!(
        crash
            .frames()
            .all(|f| matches!(f, Frame::Native {debug_id,debug_age:0,..} if *debug_id == id))
    );
    println!(
        "shipped_frames={}; safe_crash={}",
        crash.frames().count(),
        serde_json::to_string(&crash).unwrap()
    );
    assert!(at >= launched);
}
