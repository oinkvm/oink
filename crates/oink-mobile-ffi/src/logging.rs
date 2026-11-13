//! The process's Rust log output.
//!
//! `install` sets one global tracing subscriber per process (the app and the
//! share extension each run their own): INFO and above go to the system log
//! (iOS os_log with the process bundle id as subsystem and the event target
//! as category; Android logcat with tag "Oink"), and a redacted projection
//! (`oink_mobile_core::DiagnosticLog`) goes to the open diagnostics
//! collectors, which upload it with the other diagnostic events to the proxy
//! PC. There is no debug switch on the phone, so the level is fixed at INFO.

use oink_mobile_core::DiagnosticLog;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// Receives redacted log records; implemented by the diagnostics collector.
pub(crate) trait LogTarget: Send + Sync {
    fn record_log(&self, log: DiagnosticLog);
}

/// Records produced while no collector is open (session start-up, before the
/// host opens one) wait here for the first collector.
const EARLY_RECORDS: usize = 256;

#[derive(Default)]
pub(crate) struct LogHub {
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    early: VecDeque<DiagnosticLog>,
    targets: Vec<Weak<dyn LogTarget>>,
}

impl std::fmt::Debug for LogHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogHub").finish_non_exhaustive()
    }
}

impl LogHub {
    fn record(&self, log: DiagnosticLog) {
        let targets: Vec<Arc<dyn LogTarget>> = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.targets.retain(|target| target.strong_count() > 0);
            let targets: Vec<_> = state.targets.iter().filter_map(Weak::upgrade).collect();
            if targets.is_empty() {
                if state.early.len() == EARLY_RECORDS {
                    state.early.pop_front();
                }
                state.early.push_back(log);
                return;
            }
            targets
        };
        // Outside the lock: a collector may cut and queue a segment here.
        for target in targets {
            target.record_log(log.clone());
        }
    }

    pub(crate) fn attach(&self, target: Weak<dyn LogTarget>) {
        let early = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.targets.push(target.clone());
            std::mem::take(&mut state.early)
        };
        if let Some(target) = target.upgrade() {
            for log in early {
                target.record_log(log);
            }
        }
    }
}

static HUB: OnceLock<LogHub> = OnceLock::new();

fn hub() -> &'static LogHub {
    HUB.get_or_init(LogHub::default)
}

/// Lets a collector receive the process's log records from now on (and the
/// ones logged before any collector was open).
pub(crate) fn attach(target: Weak<dyn LogTarget>) {
    hub().attach(target);
}

/// Installs the process's subscriber once; later calls do nothing. If the
/// host process already set a global subscriber, that one stays.
pub(crate) fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::registry()
            .with(LevelFilter::INFO)
            .with(SystemLog)
            .with(DiagnosticsLayer(hub()));
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

struct DiagnosticsLayer(&'static LogHub);

impl<S: Subscriber> Layer<S> for DiagnosticsLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if let Some(log) = DiagnosticLog::from_event(event, now_ms()) {
            self.0.record(log);
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// The full event text, as the desktop's local log writes it.
struct Text(String);

impl Visit for Text {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

/// Upper bound of one system log line; longer text is cut at a character
/// boundary (logcat drops what exceeds its ~4 KiB entry).
const MAX_LINE_BYTES: usize = 4000;

struct SystemLog;

impl<S: Subscriber> Layer<S> for SystemLog {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut text = Text(String::new());
        event.record(&mut text);
        let metadata = event.metadata();
        system::write(*metadata.level(), metadata.target(), &line(text.0));
    }
}

fn line(mut text: String) -> std::ffi::CString {
    if text.len() > MAX_LINE_BYTES {
        let mut end = MAX_LINE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text.retain(|c| c != '\0');
    std::ffi::CString::new(text).unwrap_or_default()
}

#[cfg(target_os = "ios")]
mod system {
    use std::collections::HashMap;
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::sync::{Mutex, OnceLock};

    unsafe extern "C" {
        fn oink_os_log_create(category: *const c_char) -> *mut c_void;
        fn oink_os_log_write(log: *mut c_void, kind: u8, message: *const c_char);
    }

    // os_log_type_t. INFO is not persisted by default, so INFO and WARN use
    // DEFAULT; ERROR keeps its own type.
    const OS_LOG_TYPE_DEFAULT: u8 = 0x00;
    const OS_LOG_TYPE_DEBUG: u8 = 0x02;
    const OS_LOG_TYPE_ERROR: u8 = 0x10;

    /// One log object per category (event target), kept for the process.
    struct Handle(*mut c_void);
    // SAFETY: os_log objects are immutable and thread-safe per os_log(3).
    unsafe impl Send for Handle {}

    fn handle(target: &'static str) -> *mut c_void {
        static HANDLES: OnceLock<Mutex<HashMap<&'static str, Handle>>> = OnceLock::new();
        let mut handles = HANDLES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        handles
            .entry(target)
            .or_insert_with(|| {
                let category = CString::new(target).unwrap_or_default();
                // SAFETY: `category` is a valid C string for the call; the
                // returned object is retained for the rest of the process.
                Handle(unsafe { oink_os_log_create(category.as_ptr()) })
            })
            .0
    }

    pub(super) fn write(level: tracing::Level, target: &'static str, message: &CStr) {
        let kind = match level {
            tracing::Level::ERROR => OS_LOG_TYPE_ERROR,
            tracing::Level::WARN | tracing::Level::INFO => OS_LOG_TYPE_DEFAULT,
            _ => OS_LOG_TYPE_DEBUG,
        };
        let log = handle(target);
        // SAFETY: `log` came from os_log_create and is never released;
        // `message` is a valid C string for the call.
        unsafe { oink_os_log_write(log, kind, message.as_ptr()) }
    }
}

#[cfg(target_os = "android")]
mod system {
    use std::ffi::{CStr, CString, c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    // android_LogPriority.
    const DEBUG: c_int = 3;
    const INFO: c_int = 4;
    const WARN: c_int = 5;
    const ERROR: c_int = 6;

    pub(super) fn write(level: tracing::Level, target: &'static str, message: &CStr) {
        let priority = match level {
            tracing::Level::ERROR => ERROR,
            tracing::Level::WARN => WARN,
            tracing::Level::INFO => INFO,
            _ => DEBUG,
        };
        // One tag for the whole core; the target leads the text instead.
        let text = CString::new(format!("{target}: {}", message.to_string_lossy())).unwrap_or_default();
        // SAFETY: both pointers are valid C strings for the call.
        unsafe {
            __android_log_write(priority, c"Oink".as_ptr(), text.as_ptr());
        }
    }
}

/// Host builds (unit tests) have no system log.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod system {
    pub(super) fn write(_: tracing::Level, _: &'static str, _: &std::ffi::CStr) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_mobile_core::{DiagnosticSegment, DiagnosticSink, LogLevel, LogValue, MobileDiagnostics};

    #[derive(Default)]
    struct Sink(Mutex<Vec<DiagnosticSegment>>);
    impl DiagnosticSink for Sink {
        fn submit_segment(&self, segment: DiagnosticSegment) -> Result<(), String> {
            self.0.lock().unwrap().push(segment);
            Ok(())
        }
    }

    struct Collector(MobileDiagnostics<Sink>);
    impl LogTarget for Collector {
        fn record_log(&self, log: DiagnosticLog) {
            self.0.emit_log(log).unwrap();
        }
    }

    fn collector() -> (Arc<Sink>, Arc<Collector>) {
        let sink = Arc::new(Sink::default());
        let collector = Arc::new(Collector(MobileDiagnostics::new(sink.clone(), "phone")));
        (sink, collector)
    }

    fn weak(collector: &Arc<Collector>) -> Weak<dyn LogTarget> {
        let target: Arc<dyn LogTarget> = collector.clone();
        Arc::downgrade(&target)
    }

    /// The installed layers, scoped to this thread with a private hub.
    fn with_layers(hub: &'static LogHub, emit: impl FnOnce()) {
        let subscriber = tracing_subscriber::registry()
            .with(LevelFilter::INFO)
            .with(SystemLog)
            .with(DiagnosticsLayer(hub));
        tracing::subscriber::with_default(subscriber, emit);
    }

    #[test]
    fn rust_logs_reach_the_diagnostic_segment_without_sensitive_content() {
        let hub: &'static LogHub = Box::leak(Box::default());
        let (sink, collector) = collector();
        let clipboard = "my clipboard secret";
        let identity = "4f1c0de2a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2";
        with_layers(hub, || {
            tracing::info!(target: "oink_link_diagnostics", kind = "link_lost", link_id = 9u64, "Link lost");
            hub.attach(weak(&collector));
            tracing::warn!(
                peer_id = identity,
                text = clipboard,
                elapsed_ms = 30u64,
                ?identity,
                "clipboard {clipboard} from {identity} not sent"
            );
            tracing::debug!(target: "oink_link_diagnostics", kind = "hidden", "debug is not collected");
        });
        collector.0.flush().unwrap();
        let segments = sink.0.lock().unwrap();
        assert_eq!(segments.len(), 1);
        let logs = &segments[0].logs;
        // The record logged before the collector opened came first.
        assert_eq!(logs.len(), 2);
        assert_eq!(
            (logs[0].level, logs[0].module.as_str()),
            (LogLevel::Info, "oink_link_diagnostics")
        );
        assert_eq!(
            logs[0].fields,
            vec![
                ("kind".to_owned(), LogValue::Label("link_lost".into())),
                ("link_id".to_owned(), LogValue::U64(9)),
            ]
        );
        assert_eq!(
            (logs[1].level, logs[1].module.as_str()),
            (LogLevel::Warn, module_path!())
        );
        assert!(logs[1].line > 0);
        assert_eq!(logs[1].fields, vec![("elapsed_ms".to_owned(), LogValue::U64(30))]);
        let upload = serde_json::to_string(&oink_mobile_core::DiagnosticUpload {
            events: segments[0].events.clone(),
            logs: logs.clone(),
            dropped_events: segments[0].evicted_events,
        })
        .unwrap();
        for secret in [clipboard, identity, "secret", "not sent", "hidden"] {
            assert!(!upload.contains(secret), "{secret} leaked into {upload}");
        }
    }

    #[test]
    fn early_records_are_bounded_and_a_closed_collector_stops_receiving() {
        let hub: &'static LogHub = Box::leak(Box::default());
        with_layers(hub, || {
            for index in 0..(EARLY_RECORDS as u64 + 10) {
                tracing::warn!(dropped = index, "early");
            }
        });
        let (sink, collector) = collector();
        hub.attach(weak(&collector));
        collector.0.flush().unwrap();
        {
            let segments = sink.0.lock().unwrap();
            let logs: Vec<_> = segments.iter().flat_map(|segment| &segment.logs).collect();
            assert_eq!(logs.len(), EARLY_RECORDS);
            assert_eq!(logs[0].fields, vec![("dropped".to_owned(), LogValue::U64(10))]);
        }
        drop(collector);
        with_layers(hub, || tracing::warn!("after close"));
        assert_eq!(hub.state.lock().unwrap().early.len(), 1);
        assert!(hub.state.lock().unwrap().targets.is_empty());
    }

    #[test]
    fn installing_twice_does_not_panic_and_keeps_logging() {
        install();
        install();
        tracing::warn!(elapsed_ms = 1u64, "after install");
        // Formatting for the system log copes with NUL bytes and long text.
        assert_eq!(line("a\0b".into()).to_bytes(), b"ab");
        assert_eq!(line("é".repeat(MAX_LINE_BYTES)).to_bytes().len(), MAX_LINE_BYTES);
    }
}
