//! Only typed local crash summaries and an allowlisted Apple .ips projection cross this boundary.
//! Paths, symbol text, image/thread names, registers and exception messages are never copied.
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "macos")]
use std::io::Read;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    available: bool,
    timestamp_ms: Option<i64>,
    exception_class: oink_telemetry::ExceptionClass,
    faulting_thread: Option<u64>,
    #[serde(default)]
    exception_code: Option<u32>,
    #[serde(default)]
    frames: Vec<oink_telemetry::Frame>,
    #[serde(default)]
    previous_instance: bool,
}
impl Report {
    pub(super) fn network(&self) -> oink_protocol::v1::Result<oink_protocol::v1::dto::DiagCrashSummary> {
        use super::wire_values::{Value, dto};
        use oink_protocol::v1::{dto as w, schema::WireValue};
        use oink_telemetry::ExceptionClass as C;
        let class = match self.exception_class {
            C::Panic => w::DiagExceptionClass::Panic,
            C::BadAccess => w::DiagExceptionClass::BadAccess,
            C::BadInstruction => w::DiagExceptionClass::BadInstruction,
            C::Arithmetic => w::DiagExceptionClass::Arithmetic,
            C::Breakpoint => w::DiagExceptionClass::Breakpoint,
            C::Abort => w::DiagExceptionClass::Abort,
            C::Resource => w::DiagExceptionClass::Resource,
            C::Guard => w::DiagExceptionClass::Guard,
            C::Unhandled => w::DiagExceptionClass::Unhandled,
            C::Unknown => w::DiagExceptionClass::Indeterminate,
        };
        dto::<w::DiagCrashSummary>([
            ("available", self.available.wire()?),
            ("timestamp_ms", self.timestamp_ms.wire()?),
            ("exception_class", class.write()),
            ("faulting_thread", self.faulting_thread.wire()?),
        ])
    }
    /// Reports may predate the current process by hours. Without a validated process-lifetime
    /// match we never attribute the latest .ips to this trigger, including recovered-crash saves.
    pub(super) fn export_view(&self, at_ms: i64) -> Value {
        serde_json::json!({
            "available": self.available,
            "timestamp_ms": self.timestamp_ms,
            "exception_class": self.exception_class,
            "faulting_thread": self.faulting_thread,
            "exception_code": self.exception_code,
            "frames": self.frames,
            "association": if self.previous_instance { "previous_instance" } else if self.available { "not_correlated_with_trigger" } else { "unavailable" },
            "belongs_to_previous_instance": if self.previous_instance { Value::Bool(true) } else { Value::Null },
            "age_at_save_ms": self.timestamp_ms.map(|time| at_ms.saturating_sub(time)),
        })
    }
}
pub(super) fn latest_for(directory: &std::path::Path, previous: Option<&str>) -> Report {
    if let Some(summary) = ["crash", "crash-ui"]
        .iter()
        .filter_map(|name| oink_crash::local::read(&directory.join(name)))
        .max_by_key(|summary| (previous == Some(summary.instance.as_str()), summary.collected_at))
    {
        return Report {
            available: true,
            timestamp_ms: i64::try_from(summary.collected_at.saturating_mul(1000)).ok(),
            exception_class: summary.crash.exception_class,
            exception_code: summary.crash.exception_code,
            faulting_thread: Some(u64::from(summary.faulting_thread)),
            frames: summary.crash.frames().copied().collect(),
            previous_instance: previous == Some(summary.instance.as_str()),
        };
    }
    latest()
}
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn normalized_time(text: &str) -> Option<i64> {
    use time::{
        OffsetDateTime,
        format_description::{parse_borrowed, well_known::Rfc3339},
    };
    if text.len() > 64 {
        return None;
    }
    let date = OffsetDateTime::parse(text, &Rfc3339).ok().or_else(|| {
        [
            "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond] [offset_hour sign:mandatory][offset_minute]",
            "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]",
        ]
        .iter()
        .find_map(|format| OffsetDateTime::parse(text, &parse_borrowed::<2>(format).ok()?).ok())
    })?;
    i64::try_from(date.unix_timestamp_nanos() / 1_000_000).ok()
}
impl Default for Report {
    fn default() -> Self {
        missing()
    }
}
fn missing() -> Report {
    Report {
        available: false,
        timestamp_ms: None,
        exception_class: oink_telemetry::ExceptionClass::Unknown,
        faulting_thread: None,
        exception_code: None,
        frames: Vec::new(),
        previous_instance: false,
    }
}
#[cfg(target_os = "macos")]
pub(super) fn latest() -> Report {
    let Some(home) = std::env::var_os("HOME") else {
        return missing();
    };
    let path = std::path::PathBuf::from(home).join("Library/Logs/DiagnosticReports");
    let Ok(entries) = std::fs::read_dir(path) else {
        return missing();
    };
    let mut entries = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.ends_with(".ips")
                && ["Oink-", "oink-", "OinkFileProvider-"]
                    .iter()
                    .any(|prefix| s.starts_with(prefix))
        })
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok().map(|t| (t, e.path()))))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, path) in entries.into_iter().take(8) {
        let Ok(file) = std::fs::File::open(path) else { continue };
        let mut bytes = Vec::new();
        if file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes).is_err() || bytes.len() > 8 * 1024 * 1024 {
            continue;
        }
        if let Some(report) = parse(&bytes) {
            return report;
        }
    }
    missing()
}
#[cfg(not(target_os = "macos"))]
pub(super) fn latest() -> Report {
    missing()
}
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn parse(bytes: &[u8]) -> Option<Report> {
    // Apple writes a one-line metadata JSON object followed by the report JSON object.
    let mut objects = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let first = objects.next()?.ok()?;
    let report = objects.next().and_then(Result::ok).unwrap_or_else(|| first.clone());
    let name = report.get("procName").or_else(|| first.get("app_name"))?.as_str()?;
    if !matches!(
        name,
        "Oink" | "oink" | "oink-agent" | "oink-backend" | "OinkFileProvider" | "oink-file-provider"
    ) {
        return None;
    }
    let timestamp_ms = report
        .get("captureTime")
        .or_else(|| first.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(normalized_time);
    use oink_telemetry::ExceptionClass as Class;
    let exception_class = match report.pointer("/exception/type").and_then(Value::as_str) {
        Some("EXC_BAD_ACCESS") => Class::BadAccess,
        Some("EXC_BAD_INSTRUCTION") => Class::BadInstruction,
        Some("EXC_ARITHMETIC") => Class::Arithmetic,
        Some("EXC_BREAKPOINT") => Class::Breakpoint,
        Some("EXC_CRASH") => Class::Abort,
        Some("EXC_RESOURCE") => Class::Resource,
        Some("EXC_GUARD") => Class::Guard,
        _ => Class::Unknown,
    };
    Some(Report {
        available: true,
        timestamp_ms,
        exception_class,
        faulting_thread: report.get("faultingThread").and_then(Value::as_u64),
        frames: crate::telemetry::packaged_manifest()
            .1
            .as_ref()
            .and_then(|manifest| oink_crash::macos::project_frames(&report, manifest, oink_telemetry::ImageSlot::Agent))
            .unwrap_or_default(),
        ..Report::default()
    })
}
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn matching_previous_backend_summary_wins_over_newer_ui_crash() {
        use oink_telemetry::*;
        let directory = tempfile::tempdir().unwrap();
        let instance = "a".repeat(24);
        let crash = SafeCrash::empty(
            BuildContext {
                app_version: AppVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    build: 1,
                },
                os_version_class: OsVersionClass::Windows11,
            },
            Component::Agent,
            CrashKind::NativeException,
            ExceptionClass::BadAccess,
        );
        for (name, id, at, thread) in [
            ("crash", instance.clone(), now() - 1, 11),
            ("crash-ui", "b".repeat(24), now(), 22),
        ] {
            oink_crash::local::write(
                &oink_crash::local::Target {
                    root: directory.path().join(name),
                    instance: id,
                },
                crash.clone(),
                at,
                thread,
            )
            .unwrap();
        }
        let report = latest_for(directory.path(), Some(&instance));
        assert!(report.previous_instance);
        assert_eq!(report.faulting_thread, Some(11));
        assert_eq!(latest_for(directory.path(), None).faulting_thread, Some(22));
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "real Windows crash child; first build oink-crash-collector"]
    fn windows_child_crash_is_exported_with_previous_instance() {
        use crate::diagnostics::{Reason, store::Store};
        use oink_telemetry::*;
        const KEY: &str = "OINK_DIAG_NATIVE_CHILD";
        if let Some(root) = std::env::var_os(KEY) {
            let root = std::path::PathBuf::from(root);
            let store = Store::open(root.clone(), "a".repeat(24), true).unwrap();
            let executable = std::env::current_exe().unwrap();
            let version = crate::telemetry::packaged_manifest().0;
            let build = BuildContext {
                app_version: version,
                os_version_class: OsVersionClass::Windows11,
            };
            let manifest = oink_telemetry::capture::Manifest {
                version,
                images: vec![oink_crash::dump::executable_image(&executable, ImageSlot::Agent).unwrap()],
            };
            oink_crash::capsule::Capture::install();
            let _collector = oink_crash::collector::Collector::start_with_local(
                &executable
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("oink-crash-collector.exe"),
                &root.join("raw"),
                ReportConsent::default(),
                build,
                &manifest,
                Component::Agent,
                Some(oink_crash::local::Target {
                    root: root.join("crash"),
                    instance: store.instance.clone(),
                }),
            )
            .unwrap();
            // SAFETY: only this isolated child raises a fatal exception; no
            // installed process, UI desktop, or reporting grant is involved.
            unsafe {
                use windows::Win32::System::Diagnostics::Debug::*;
                SetErrorMode(SEM_NOGPFAULTERRORBOX);
                RaiseException(0xe000_beef, 0, None);
            }
            std::process::exit(4);
        }
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::crash_report::tests::windows_child_crash_is_exported_with_previous_instance",
                "--ignored",
                "--nocapture",
            ])
            .env(KEY, dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(16);
        while !oink_crash::local::read(&dir.path().join("crash")).is_some_and(|s| s.crash.frames().count() > 1)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let store = Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        assert!(store.previous_instance.is_some());
        let bundle = store.export(Reason::RecoveredCrash, 0, 0).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let report: Value = serde_json::from_reader(zip.by_name("crash-report.json").unwrap()).unwrap();
        assert_eq!(report["available"], true, "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(report["exception_code"], 0xe000_beefu32);
        assert_eq!(report["belongs_to_previous_instance"], true);
        assert!(report["frames"].as_array().unwrap().len() > 1);
        assert!(!dir.path().join("raw/0.dmp").exists());
        println!("real Windows ZIP crash summary: {}", report);
    }
    #[test]
    fn local_windows_crash_is_available_and_correlated_without_report_consent() {
        use oink_telemetry::*;
        let dir = tempfile::tempdir().unwrap();
        let target = oink_crash::local::Target {
            root: dir.path().join("crash"),
            instance: "abcdef012345abcdef012345".into(),
        };
        let mut crash = SafeCrash::empty(
            BuildContext {
                app_version: AppVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    build: 1,
                },
                os_version_class: OsVersionClass::Windows11,
            },
            Component::Agent,
            CrashKind::NativeException,
            ExceptionClass::BadAccess,
        );
        crash.exception_code = Some(0xc0000005);
        crash.frames[0][0] = Some(Frame::Native {
            image: ImageSlot::Agent,
            offset: 42,
            debug_id: [7; 16],
            debug_age: 1,
            code_id: Some(123),
            image_size: 4096,
        });
        oink_crash::local::write(&target, crash, now(), 9).unwrap();
        let report = latest_for(dir.path(), Some(&target.instance)).export_view((now() * 1000) as i64);
        assert_eq!(report["available"], true);
        assert_eq!(report["belongs_to_previous_instance"], true);
        assert_eq!(report["exception_code"], 0xc0000005u32);
        assert_eq!(report["faulting_thread"], 9);
        assert_eq!(report["frames"].as_array().unwrap().len(), 1);
        assert!(
            latest_for(dir.path(), Some("012345abcdef012345abcdef")).export_view(0)["belongs_to_previous_instance"]
                .is_null()
        );
    }
    #[test]
    fn historical_reports_use_utc_and_do_not_claim_trigger_correlation() {
        let western = normalized_time("2026-10-01 07:49:29.1955 -0700").unwrap();
        let eastern = normalized_time("2026-10-01 22:49:29.1955 +0800").unwrap();
        assert_eq!(western, eastern);
        let report = Report {
            available: true,
            timestamp_ms: Some(western),
            ..Report::default()
        };
        let exported = report.export_view(western + 3_600_000);
        assert_eq!(exported["timestamp_ms"], western);
        assert_eq!(exported["age_at_save_ms"], 3_600_000);
        assert_eq!(exported["association"], "not_correlated_with_trigger");
        assert!(exported["belongs_to_previous_instance"].is_null());
        assert!(normalized_time("secret-user").is_none());
    }
    #[test]
    fn report_drops_paths_names_registers_and_unknown_symbols() {
        let raw=br#"{"app_name":"Oink","timestamp":"2026-10-01 00:00:00.000 +0000","user":"secret-user"}
        {"procName":"Oink","procPath":"/Users/secret-user/Oink.app","exception":{"type":"EXC_BREAKPOINT","message":"keyboard 458756"},"faultingThread":0,"threads":[{"name":"secret-screen","frames":[{"symbol":"_dispatch_assert_queue_fail","image":"/Users/secret-user/file"},{"symbol":"/Users/secret-user/crash"},{"symbol":"oink_agent::run<secret-user>"}]}]}"#;
        let json = serde_json::to_string(&parse(raw).unwrap()).unwrap();
        for secret in ["secret-user", "secret-screen", "458756", "/Users/"] {
            assert!(!json.contains(secret));
        }
        assert!(!json.contains("_dispatch_assert_queue_fail"));
        assert!(!json.contains("oink_agent::run"));
        assert!(json.contains("breakpoint"));
    }
}
