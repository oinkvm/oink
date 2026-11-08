#![cfg(target_os = "macos")]
use std::{path::Path, process::Command};

unsafe fn fixture(library: *mut libc::c_void, name: &std::ffi::CStr) -> unsafe extern "C-unwind" fn() {
    // SAFETY: the test library exports these exact void(void) functions and is
    // deliberately kept loaded until the child process exits.
    let symbol = unsafe { libc::dlsym(library, name.as_ptr()) };
    assert!(!symbol.is_null());
    // SAFETY: exact fixture ABI from the library retained until process exit.
    unsafe { std::mem::transmute(symbol) }
}
#[test]
fn records_before_tokio_foreign_unwind_and_preserves_catches() {
    if let Ok(mode) = std::env::var("OINK_OBJC_CHILD") {
        let dir = std::path::PathBuf::from(std::env::var_os("OINK_OBJC_DIR").unwrap());
        let library_path = std::ffi::CString::new(dir.join("fixture.dylib").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: loading only our isolated fixture with matching function ABI.
        let library = unsafe { libc::dlopen(library_path.as_ptr(), libc::RTLD_NOW) };
        assert!(!library.is_null());
        // SAFETY: exact fixture ABI from the library retained until process exit.
        unsafe { fixture(library, c"fixture_prior")() };
        let directory = dir.join("capture");
        let capture = if mode == "report_only_abort" {
            None
        } else {
            Some(oink_crash::objc::Capture::start(&directory).unwrap())
        };
        let report_capture = oink_crash::objc::ReportCapture::start(&dir.join("safe.jsonl")).unwrap();
        // SAFETY: exact fixture ABI from the library retained until process exit.
        let throw = unsafe {
            fixture(
                library,
                if mode == "bad_abort" {
                    c"fixture_bad_throw"
                } else {
                    c"fixture_throw"
                },
            )
        };
        if mode.ends_with("abort") {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                runtime
                    .spawn_blocking(move || {
                        // SAFETY: fixture_throw's C-unwind ABI permits propagation;
                        // the child deliberately aborts at Tokio's Rust catch.
                        unsafe { throw() }
                    })
                    .await
                    .unwrap()
            });
            panic!("foreign exception should have aborted in Tokio catch_unwind");
        }
        // SAFETY: exact fixture ABI from the library retained until process exit.
        let caught = unsafe { fixture(library, c"fixture_caught") };
        for _ in 0..20 {
            // SAFETY: exact fixture ABI from the library retained until process exit.
            unsafe { caught() };
        }
        // SAFETY: exact fixture ABI from the library retained until process exit.
        unsafe { fixture(library, c"fixture_recursive")() };
        // Prior preprocessor receives every throw, including the nested getter.
        // SAFETY: exact fixture ABI from the library retained until process exit.
        let observed = unsafe { libc::dlsym(library, c"fixture_observed".as_ptr()) };
        assert!(!observed.is_null());
        // SAFETY: fixture_observed is exported as unsigned(void).
        let observed: unsafe extern "C" fn() -> u32 = unsafe { std::mem::transmute(observed) };
        // SAFETY: exact fixture ABI from the library retained until process exit.
        assert_eq!(unsafe { observed() }, 22);
        if mode == "after_catch" {
            std::process::abort();
        }
        drop(capture);
        drop(report_capture);
        let bytes = std::fs::read(directory.join("objc-exceptions.jsonl")).unwrap();
        // SAFETY: exact fixture ABI from the library retained until process exit.
        unsafe { caught() }; // off: no additional write, still chains previous
        assert_eq!(std::fs::read(directory.join("objc-exceptions.jsonl")).unwrap(), bytes);
        if mode == "erase" {
            oink_crash::objc::erase(&directory).unwrap();
            assert!(!directory.join("objc-exceptions.jsonl").exists());
        } else {
            // Rotate on restart/re-enable; an empty lifetime preserves previous.
            drop(oink_crash::objc::Capture::start(&directory).unwrap());
            drop(oink_crash::objc::Capture::start(&directory).unwrap());
            assert_eq!(
                std::fs::read(directory.join("objc-exceptions.previous.jsonl")).unwrap(),
                bytes
            );
        }
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("xcrun")
            .args([
                "clang",
                "-dynamiclib",
                "-g",
                "-O0",
                "-fobjc-arc",
                "-fobjc-exceptions",
                "-fexceptions",
                "-framework",
                "Foundation"
            ])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/objc_throw.m"))
            .arg("-o")
            .arg(dir.path().join("fixture.dylib"))
            .status()
            .unwrap()
            .success()
    );
    for mode in [
        "abort",
        "bad_abort",
        "report_only_abort",
        "handled",
        "after_catch",
        "erase",
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "records_before_tokio_foreign_unwind_and_preserves_catches",
                "--nocapture",
            ])
            .env("OINK_OBJC_CHILD", mode)
            .env("OINK_OBJC_DIR", dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            !mode.ends_with("abort") && mode != "after_catch",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if mode.ends_with("abort") || mode == "after_catch" {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(output.status.signal(), Some(libc::SIGABRT));
            assert!(String::from_utf8_lossy(&output.stderr).contains("fixture_previous_abort"));
        }
        let safe = std::fs::read(dir.path().join("safe.jsonl")).unwrap();
        assert!(!String::from_utf8_lossy(&safe).contains("reason"));
        assert!(!String::from_utf8_lossy(&safe).contains("poison"));
        assert_safe_projection(&safe, mode.ends_with("abort"), mode == "bad_abort", dir.path());
        if mode == "erase" || mode == "bad_abort" || mode == "report_only_abort" || mode == "after_catch" {
            continue;
        }
        let name = if mode == "abort" {
            "objc-exceptions.jsonl"
        } else {
            "objc-exceptions.previous.jsonl"
        };
        let bytes = std::fs::read(dir.path().join("capture").join(name)).unwrap();
        assert!(bytes.len() <= 128 * 1024);
        let records: Vec<serde_json::Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert!(!records.is_empty());
        let record = records.iter().find(|r| r["name"] == "OinkTestException").unwrap();
        assert_eq!(record["reason"], "fixture \"quoted\"\n原因");
        assert!(record["thread"].as_u64().unwrap() > 0);
        let uuid = oink_crash::macos::executable_uuid(&dir.path().join("fixture.dylib")).unwrap();
        let uuid: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
        let frame = record["frames"]
            .as_array()
            .unwrap()
            .iter()
            .find(|frame| frame["uuid"] == uuid)
            .unwrap();
        // backtrace returns return PCs. Normalize before symbolication: a
        // noreturn throw's return PC can be the next function's first byte.
        let offset = frame["offset"].as_u64().unwrap() - if cfg!(target_arch = "aarch64") { 4 } else { 1 };
        assert!(offset > 0, "throwing library must survive in pre-unwind stack");
        let symbols = Command::new("xcrun")
            .args(["atos", "-o"])
            .arg(dir.path().join("fixture.dylib"))
            .args(["-l", "0", &format!("0x{offset:x}")])
            .output()
            .unwrap();
        let symbol = String::from_utf8_lossy(&symbols.stdout);
        assert!(symbol.contains("fixture_throw"), "original throw call site: {symbol}");
        println!(
            "{mode}: name={}, reason={}, original_throw={}, records={}, bytes={}",
            record["name"],
            record["reason"],
            symbol.trim(),
            records.len(),
            bytes.len()
        );
        if mode.ends_with("abort") {
            assert!(String::from_utf8_lossy(&output.stderr).contains("Rust cannot catch foreign exceptions"));
        } else {
            assert_eq!(records.len(), 8);
        }
    }
}

fn assert_safe_projection(bytes: &[u8], aborted: bool, bad_name: bool, directory: &Path) {
    use oink_telemetry::*;
    let records: Vec<serde_json::Value> = bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let last = records.iter().max_by_key(|r| r["sequence"].as_u64()).unwrap();
    assert_eq!(last["fatal"], if aborted { 1 } else { 0 });
    let id = oink_crash::macos::executable_uuid(&directory.join("fixture.dylib")).unwrap();
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let build = BuildContext {
        app_version: AppVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 1,
        },
        os_version_class: OsVersionClass::MacOSMajor(27),
    };
    let lifetime = oink_crash::macos::Lifetime {
        schema: 1,
        pid: last["pid"].as_u64().unwrap() as u32,
        launched_at: last["unix_seconds"].as_u64().unwrap(),
        armed_at: last["unix_seconds"].as_u64().unwrap(),
        generation: 1,
        component: Component::Agent,
        image: ImageSlot::Agent,
        debug_id: id,
        build,
    };
    let manifest = capture::Manifest {
        version: build.app_version,
        images: vec![capture::Image {
            slot: ImageSlot::Agent,
            debug_id: id,
            debug_age: 0,
            code_id: None,
            text_size: 1 << 30,
        }],
    };
    let consent = ReportConsent {
        choice: Choice::Granted,
        policy_version: POLICY_VERSION,
        generation: 1,
    };
    let date = time::OffsetDateTime::from_unix_timestamp(lifetime.armed_at as i64).unwrap();
    let timestamp = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        date.hour(),
        date.minute(),
        date.second()
    );
    let mut ips = serde_json::json!({"pid":lifetime.pid,"procLaunch":timestamp,"captureTime":timestamp,
        "exception":{"type":"EXC_CRASH","signal":"SIGABRT"},"usedImages":[{"uuid":uuid,"arch":"arm64"}],
        "faultingThread":0,"threads":[{"id":last["thread"],"frames":[{"imageIndex":0,"imageOffset":24}]}]});
    let extract = |ips: &serde_json::Value, consent, raw: &[u8]| {
        oink_crash::macos::extract_with_objc(
            &serde_json::to_vec(ips).unwrap(),
            lifetime,
            consent,
            &manifest,
            lifetime.armed_at + 1,
            raw,
        )
    };
    let (crash, _) = extract(&ips, consent, bytes).unwrap();
    assert_eq!(crash.objc_exception.is_some(), aborted);
    if let Some(throw) = crash.objc_exception {
        assert_eq!(
            throw.name.as_str(),
            if bad_name { "other" } else { "OinkTestException" }
        );
        assert!(throw.frames().count() > 0);
        assert!(
            throw
                .frames()
                .all(|frame| matches!(frame, Frame::Native { debug_id, .. } if *debug_id == id))
        );
    }
    assert!(extract(&ips, ReportConsent::default(), bytes).is_none());
    assert!(
        extract(
            &ips,
            ReportConsent {
                generation: 2,
                ..consent
            },
            bytes
        )
        .is_none()
    );
    ips["threads"][0]["id"] = serde_json::json!(last["thread"].as_u64().unwrap() + 1);
    assert!(extract(&ips, consent, bytes).unwrap().0.objc_exception.is_none());
    ips["threads"][0]["id"] = last["thread"].clone();
    ips["exception"]["signal"] = serde_json::json!("SIGSEGV");
    assert!(extract(&ips, consent, bytes).unwrap().0.objc_exception.is_none());
    ips["exception"]["signal"] = serde_json::json!("SIGABRT");
    let mut poisoned = last.clone();
    poisoned["reason"] = serde_json::json!("private free text");
    assert!(
        extract(&ips, consent, &serde_json::to_vec(&poisoned).unwrap())
            .unwrap()
            .0
            .objc_exception
            .is_none()
    );
    poisoned.as_object_mut().unwrap().remove("reason");
    poisoned["name"] = serde_json::json!("/private/free text");
    assert!(
        extract(&ips, consent, &serde_json::to_vec(&poisoned).unwrap())
            .unwrap()
            .0
            .objc_exception
            .is_none()
    );
}
