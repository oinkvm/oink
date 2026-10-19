//! Only a small allowlisted projection of Apple .ips files crosses this boundary.
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
            "association": if self.available { "not_correlated_with_trigger" } else { "unavailable" },
            "belongs_to_previous_instance": Value::Null,
            "age_at_save_ms": self.timestamp_ms.map(|time| at_ms.saturating_sub(time)),
        })
    }
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
    })
}
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
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
