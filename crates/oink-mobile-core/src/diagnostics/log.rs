//! Rust log records in the mobile diagnostics upload.
//!
//! The same rule as the PC diagnostics layer (`docs/design/diagnostics.md`,
//! "日志是独立 tracing layer"): a record keeps its compile-time call site
//! (module, line, level), audited numeric/bool fields and a few audited enum
//! labels. The free-form message, Debug/Display values and every other string
//! never enter it: they can carry clipboard text, file names, addresses,
//! device ids or key material. The phone's system log (os_log / logcat) gets
//! the full text; only this projection leaves the phone.

use serde::{Deserialize, Serialize};
use tracing::field::{Field, Visit};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LogValue {
    Bool(bool),
    U64(u64),
    I64(i64),
    F64(f64),
    /// One of the audited enum labels (`LABEL_FIELDS`, `label_value`).
    Label(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticLog {
    pub timestamp_ms: u64,
    pub level: LogLevel,
    /// The dedicated `*_diagnostics` target, else the event's module path.
    pub module: String,
    pub line: u32,
    pub fields: Vec<(String, LogValue)>,
}

pub const MAX_LOG_FIELDS: usize = 16;
const MAX_MODULE_BYTES: usize = 128;
const MAX_LABEL_BYTES: usize = 32;

/// Audited numeric/bool fields of the call sites that run on the phone
/// (mobile core/FFI and the shared link layer). Counters, durations, local
/// sequence numbers and state bits only; never key codes, ports, sizes of
/// user content tied to names, or identities. Add a name only after checking
/// every call site that uses it.
const NUMERIC_FIELDS: &[&str] = &[
    "active",
    "arbiter",
    "audio",
    "bindings",
    "choice_counter",
    "choice_watermark",
    "columns",
    "commit_queued",
    "context_age_ms",
    "drain_counts_known",
    "drained_queued",
    "dropped",
    "duration_ms",
    "elapsed_ms",
    "group_input",
    "heartbeat_age_ms",
    "heartbeat_known",
    "limited",
    "link",
    "link_id",
    "local_drained",
    "local_input",
    "os_error",
    "os_error_known",
    "pacing_id",
    "path_counter",
    "peer_data_age_ms",
    "peer_data_known",
    "peer_drained",
    "pong_known",
    "probe_timeout",
    "readers_pending",
    "reason",
    "refused",
    "relayed",
    "reverse",
    "revision",
    "rtt_ms",
    "running",
    "selected",
    "target_ms",
    "writers_pending",
];

/// String fields whose call sites pass a literal enum label. The value must
/// also look like one (`label_value`), so a formatted string that reaches one
/// of these names by mistake is dropped instead of kept.
const LABEL_FIELDS: &[&str] = &[
    "association",
    "candidate_class",
    "decision_side",
    "direction",
    "initiator",
    "kind",
    "network_path",
    "reason",
    "result",
    "stage",
    "transport",
    "waiting",
    "waiting_on",
];

fn module_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MODULE_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':')
}

/// Lower-case letters and `_` only: no digits, so hex ids, addresses, paths
/// and numbers cannot pass as a label.
fn label_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value.starts_with(|first: char| first.is_ascii_lowercase())
        && value.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_')
}

fn field_allowed(name: &str, value: &LogValue) -> bool {
    match value {
        LogValue::Label(label) => LABEL_FIELDS.contains(&name) && label_value(label),
        LogValue::F64(number) => number.is_finite() && NUMERIC_FIELDS.contains(&name),
        LogValue::Bool(_) | LogValue::U64(_) | LogValue::I64(_) => NUMERIC_FIELDS.contains(&name),
    }
}

struct Fields(Vec<(String, LogValue)>);

impl Fields {
    fn add(&mut self, field: &Field, value: LogValue) {
        if self.0.len() < MAX_LOG_FIELDS && field_allowed(field.name(), &value) {
            self.0.push((field.name().to_owned(), value));
        }
    }
}

impl Visit for Fields {
    /// The message and every `?value` / `%value` arrive here; none is kept.
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}

    fn record_str(&mut self, field: &Field, value: &str) {
        if LABEL_FIELDS.contains(&field.name()) && label_value(value) {
            self.add(field, LogValue::Label(value.to_owned()));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.add(field, LogValue::Bool(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.add(field, LogValue::U64(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.add(field, LogValue::I64(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.add(field, LogValue::F64(value));
    }
}

impl DiagnosticLog {
    /// The redacted projection of one tracing event, or `None` when the
    /// event is not collected: WARN/ERROR from any target, INFO only from
    /// Oink's own targets, nothing below INFO.
    pub fn from_event(event: &tracing::Event<'_>, timestamp_ms: u64) -> Option<Self> {
        let metadata = event.metadata();
        let target = metadata.target();
        let level = match *metadata.level() {
            tracing::Level::ERROR => LogLevel::Error,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::INFO if target.starts_with("oink") => LogLevel::Info,
            _ => return None,
        };
        let module = if target.starts_with("oink") && target.ends_with("_diagnostics") {
            target
        } else {
            metadata.module_path().unwrap_or("external")
        };
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        let log = Self {
            timestamp_ms,
            level,
            module: if module_name(module) { module } else { "external" }.to_owned(),
            line: metadata.line().unwrap_or(0),
            fields: fields.0,
        };
        log.validate().is_ok().then_some(log)
    }

    /// The receiving PC checks the same rule before storing a record.
    pub fn validate(&self) -> Result<(), String> {
        if self.timestamp_ms > (i64::MAX as u64) / 1_000 {
            return Err("invalid diagnostic log timestamp".into());
        }
        if !module_name(&self.module) {
            return Err("invalid diagnostic log module".into());
        }
        if self.fields.len() > MAX_LOG_FIELDS {
            return Err("too many diagnostic log fields".into());
        }
        if self.fields.iter().any(|(name, value)| !field_allowed(name, value)) {
            return Err("diagnostic log field is not allowed".into());
        }
        Ok(())
    }

    /// Bytes this record adds to an upload (its JSON form).
    pub(crate) fn encoded_bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    struct Capture(Arc<Mutex<Vec<DiagnosticLog>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if let Some(log) = DiagnosticLog::from_event(event, 7) {
                self.0.lock().unwrap().push(log);
            }
        }
    }

    fn capture(emit: impl FnOnce()) -> Vec<DiagnosticLog> {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(logs.clone()));
        tracing::subscriber::with_default(subscriber, emit);
        Arc::try_unwrap(logs).unwrap().into_inner().unwrap()
    }

    #[test]
    fn a_record_keeps_its_call_site_and_audited_scalars_only() {
        let clipboard = "secret clipboard text";
        let key = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let logs = capture(|| {
            tracing::warn!(
                elapsed_ms = 12u64,
                relayed = true,
                rtt_ms = f64::NAN,
                stage = "probe",
                result = "c:/Users/name",
                peer_id = key,
                text = clipboard,
                port = 5353u64,
                path = ?std::path::Path::new("/var/mobile/secret.txt"),
                error = %clipboard,
                "copy failed for {clipboard} with {key}"
            );
        });
        assert_eq!(logs.len(), 1);
        let log = &logs[0];
        assert_eq!(log.level, LogLevel::Warn);
        assert_eq!(log.module, module_path!());
        assert!(log.line > 0);
        assert_eq!(
            log.fields,
            vec![
                ("elapsed_ms".to_owned(), LogValue::U64(12)),
                ("relayed".to_owned(), LogValue::Bool(true)),
                ("stage".to_owned(), LogValue::Label("probe".into())),
            ]
        );
        let json = String::from_utf8(serde_json::to_vec(log).unwrap()).unwrap();
        for secret in [clipboard, key, "secret", "Users", "5353", "copy failed"] {
            assert!(!json.contains(secret), "{secret} leaked into {json}");
        }
    }

    #[test]
    fn info_is_collected_from_oink_targets_only_and_debug_never() {
        let logs = capture(|| {
            tracing::info!(target: "oink_link_diagnostics", kind = "link_lost", link_id = 3u64, "Link lost");
            tracing::info!(target: "quinn_proto", "external info");
            tracing::warn!(target: "quinn_proto", "external warning");
            tracing::debug!(target: "oink_link_diagnostics", kind = "x", "debug");
            tracing::error!("oink module error");
        });
        let seen: Vec<_> = logs.iter().map(|log| (log.level, log.module.as_str())).collect();
        assert_eq!(
            seen,
            vec![
                (LogLevel::Info, "oink_link_diagnostics"),
                (LogLevel::Warn, module_path!()),
                (LogLevel::Error, module_path!()),
            ]
        );
        assert_eq!(
            logs[0].fields,
            vec![
                ("kind".to_owned(), LogValue::Label("link_lost".into())),
                ("link_id".to_owned(), LogValue::U64(3)),
            ]
        );
    }

    #[test]
    fn validation_rejects_what_the_projection_never_produces() {
        let log = DiagnosticLog {
            timestamp_ms: 1,
            level: LogLevel::Info,
            module: "oink_network::link".into(),
            line: 9,
            fields: vec![("kind".into(), LogValue::Label("link_lost".into()))],
        };
        assert!(log.validate().is_ok());
        let with = |fields: Vec<(String, LogValue)>| DiagnosticLog { fields, ..log.clone() };
        assert!(
            with(vec![("text".into(), LogValue::Label("hello".into()))])
                .validate()
                .is_err()
        );
        assert!(
            with(vec![("kind".into(), LogValue::Label("a1b2".into()))])
                .validate()
                .is_err()
        );
        assert!(with(vec![("port".into(), LogValue::U64(5353))]).validate().is_err());
        assert!(
            with(vec![("rtt_ms".into(), LogValue::F64(f64::INFINITY))])
                .validate()
                .is_err()
        );
        assert!(
            DiagnosticLog {
                module: "/private/var/x".into(),
                ..log.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            with(vec![("link".into(), LogValue::U64(1)); MAX_LOG_FIELDS + 1])
                .validate()
                .is_err()
        );
    }
}
