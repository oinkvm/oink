//! 日志：输出到 stderr，同时保留最近的日志供界面「状态与诊断」查看（`AgentApi::logs`、`ApiEvent::Log`）。
//! 服务进程（界面程序、`oink --service`、`oink-agent run`）另外写滚动日志文件（[`init_service`]），
//! 从 Finder / 登录项启动时也有日志；服务在独立进程里时界面进程写自己的文件（[`init_ui`]）。
//!
//! 日志不含按键与剪贴板内容（各模块只记录类型、长度）。
//!
//! Ordinary log levels come from typed local preferences. The UI ring is capped
//! at INFO; the independent diagnostic layer always receives its audited events.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterExt, LevelFilter};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::api::{LogLevel, LogLine};

#[cfg(all(test, debug_assertions))]
mod exit_tests;
use oink_foundation::logging::{FileWriter, RollingFiles};

static FILE_WRITER: OnceLock<FileWriter> = OnceLock::new();
static WRITER_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Update ordinary-log retention of this process's log directory, including
/// already closed files. No-op before a file writer exists.
pub fn set_max_mb(mb: u64) -> std::io::Result<()> {
    match WRITER_DIR.get() {
        Some(dir) => oink_foundation::logging::set_max_mb(dir, mb),
        None => Ok(()),
    }
}

/// Owned by the application entry boundary, outside every role's error handler.
/// Static subscribers are never dropped at process exit, so explicitly wait for
/// all records queued before the boundary returns or unwinds.
pub(crate) struct FlushOnExit;
impl Drop for FlushOnExit {
    fn drop(&mut self) {
        flush();
    }
}
/// Drain every record already produced. Also called before Tauri's direct exit.
pub fn flush() {
    if let Some(writer) = FILE_WRITER.get() {
        let _ = writer.drain();
    }
}

type LevelReloader = Box<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;
static LEVEL_RELOADERS: OnceLock<Vec<LevelReloader>> = OnceLock::new();

/// Apply a typed preference's canonical level after its durable save. Local
/// diagnostic recording has its own layer and is independent of this filter.
pub fn set_level(level: &str) -> std::io::Result<()> {
    if !matches!(level, "off" | "error" | "warn" | "info" | "debug" | "trace") {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    if let Some(reloaders) = LEVEL_RELOADERS.get() {
        for reload in reloaders {
            reload(level)?;
        }
    }
    Ok(())
}

fn reload_error(_: tracing_subscriber::reload::Error) -> std::io::Error {
    std::io::Error::other("logging subscriber unavailable")
}

/// 保留的日志条数。
const CAPACITY: usize = 2000;

type Listener = Arc<dyn Fn(&LogLine) + Send + Sync>;

#[derive(Default)]
struct Ring {
    seq: u64,
    lines: VecDeque<LogLine>,
    listeners: Vec<(u64, Listener)>,
    next_listener: u64,
}

fn ring() -> &'static Mutex<Ring> {
    static RING: OnceLock<Mutex<Ring>> = OnceLock::new();
    RING.get_or_init(Default::default)
}

fn configured_level() -> &'static str {
    crate::config::AgentConfig::load()
        .map_or_else(
            |_| crate::config::LogLevel::default(),
            |config| config.settings.log_level,
        )
        .as_str()
}

/// Initialize command logging with the applied local preference.
pub fn init() {
    install(configured_level(), None);
}

/// Initialize the background role's ordinary file and UI-ring logging.
pub fn init_service() {
    install(configured_level(), file_writer("oink"));
    #[cfg(target_os = "macos")]
    set_native_exception_capture(crate::config::AgentConfig::load().map_or_else(
        |_| crate::config::Settings::default().diagnostics_enabled,
        |config| config.settings.diagnostics_enabled,
    ));
}

/// The Windows pre-login backend's own file in the machine-level log directory
/// (`%ProgramData%\Oink\Prelogin\Logs`, fixed by its entry before this call).
/// Group diagnostic collection reads the same log root.
pub fn init_prelogin() {
    install(configured_level(), file_writer("oink-prelogin"));
}

/// Initialize the UI role's separate ordinary log file.
pub fn init_ui() {
    install(configured_level(), file_writer("oink-ui"));
}

/// The local replacement continuation outlives the backend that launched it.
/// Its entry boundary drains its own log even when stderr is /dev/null.
pub fn init_update() {
    install(configured_level(), file_writer("oink-update-worker"));
}

/// Ordinary logs and diagnostics share the platform log root and separate budgets.
pub fn log_dir() -> PathBuf {
    oink_foundation::paths::log_dir()
}

fn file_writer(prefix: &str) -> Option<FileWriter> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let appender = RollingFiles::new(dir.clone(), prefix, || {
        crate::config::AgentConfig::load().map_or_else(
            |_| u64::from(crate::config::Settings::default().log_max_mb.get()),
            |config| u64::from(config.settings.log_max_mb.get()),
        )
    })
    .ok()?;
    // Unit tests never publish a live retention directory (unchanged from
    // before the writer moved to oink-foundation).
    #[cfg(not(test))]
    let _ = WRITER_DIR.set(dir);
    let writer = FileWriter::new(appender).ok()?;
    FILE_WRITER.set(writer.clone()).ok()?;
    Some(writer)
}

fn local_log_metadata(metadata: &tracing::Metadata<'_>) -> bool {
    !crate::diagnostics::bundle_only_target(metadata.target())
}

fn local_log_filter<S: Subscriber>() -> impl tracing_subscriber::layer::Filter<S> {
    LocalLogFilter(Mutex::new(DiagnosticBudget::default()))
}

#[derive(Default)]
struct DiagnosticBudget {
    began: Option<Instant>,
    used: u32,
}
impl DiagnosticBudget {
    fn admit(&mut self, now: Instant) -> bool {
        if self
            .began
            .is_none_or(|began| now.duration_since(began) >= Duration::from_secs(60))
        {
            self.began = Some(now);
            self.used = 0;
        }
        self.used = self.used.saturating_add(1);
        self.used <= 128
    }
}

// Only lifecycle decisions bypass the verbose bootstrap budget, and the one
// line naming the candidate a local dial completed on (2026-11-12: a restart's
// first 2 s of bootstrap steps used the whole budget, so two successful dials
// left no record of their address). Inspect the typed kind and result fields,
// never the formatted message or peer-controlled error text.
#[derive(Default)]
struct CriticalLinkEvent {
    lifecycle: bool,
    candidate_dial: bool,
    completed: bool,
}
impl CriticalLinkEvent {
    fn critical(&self) -> bool {
        self.lifecycle || self.candidate_dial && self.completed
    }
}
impl Visit for CriticalLinkEvent {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "kind" => {
                self.lifecycle = matches!(value, "link_lost" | "path_transition" | "connection_step");
                self.candidate_dial = value == "candidate_dial";
            }
            "result" => self.completed = value == "completed",
            _ => {}
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}
struct LocalLogFilter(Mutex<DiagnosticBudget>);
impl<S: Subscriber> tracing_subscriber::layer::Filter<S> for LocalLogFilter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: &Context<'_, S>) -> bool {
        local_log_metadata(metadata)
    }
    fn event_enabled(&self, event: &Event<'_>, _: &Context<'_, S>) -> bool {
        let metadata = event.metadata();
        // These detailed per-step records also enter the independent bounded
        // diagnostic recorder. Bound their ordinary text/ring copies even when
        // an older peer floods incoming connections; never suppress faults.
        if *metadata.level() > Level::WARN && matches!(metadata.target(), "oink_link_diagnostics" | "oink_dnd_read") {
            let mut critical = CriticalLinkEvent::default();
            if metadata.target() == "oink_link_diagnostics" {
                event.record(&mut critical);
            }
            if critical.critical() {
                return true;
            }
            return self.0.lock().unwrap_or_else(|e| e.into_inner()).admit(Instant::now());
        }
        true
    }
}

fn install(directive: &str, file: Option<FileWriter>) {
    let directive = if directive.trim().is_empty() { "info" } else { directive };
    let env = || EnvFilter::try_new(directive).unwrap_or_else(|_| EnvFilter::new("info"));
    let (stderr_filter, stderr_handle) = tracing_subscriber::reload::Layer::new(env());
    let (ring_filter, ring_handle) = tracing_subscriber::reload::Layer::new(env());
    let mut reloaders: Vec<LevelReloader> = vec![
        Box::new(move |level| stderr_handle.reload(EnvFilter::new(level)).map_err(reload_error)),
        Box::new(move |level| ring_handle.reload(EnvFilter::new(level)).map_err(reload_error)),
    ];
    let file_layer = file.map(|w| {
        let (filter, handle) = tracing_subscriber::reload::Layer::new(env());
        reloaders.push(Box::new(move |level| {
            handle.reload(EnvFilter::new(level)).map_err(reload_error)
        }));
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(w)
            .with_filter(filter.and(local_log_filter()))
    });
    let result = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter.and(local_log_filter())),
        )
        .with(file_layer)
        .with(crate::diagnostics::DiagnosticsLayer)
        .with(RingLayer.with_filter(ring_filter.and(LevelFilter::INFO).and(local_log_filter())))
        .try_init();
    if result.is_ok() {
        let _ = LEVEL_RELOADERS.set(reloaders);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Keep local panic details in the ordinary log, never in telemetry.
            if let Some(writer) = FILE_WRITER.get() {
                writer.record_panic(format!("ERROR oink_panic: {info}\n"));
            }
            previous(info);
        }));
    }
}

/// 序号大于 `after` 的日志，最多 `limit` 条（取最新的）。
pub fn recent(after: Option<u64>, limit: usize) -> Vec<LogLine> {
    let r = ring().lock().unwrap_or_else(|e| e.into_inner());
    let v: Vec<LogLine> = r
        .lines
        .iter()
        .filter(|l| after.is_none_or(|a| l.seq > a))
        .cloned()
        .collect();
    let skip = v.len().saturating_sub(limit);
    v.into_iter().skip(skip).collect()
}

/// 订阅新日志（回调在写日志的线程里调用，必须很快）。返回取消订阅用的编号。
pub fn listen(f: Listener) -> u64 {
    let mut r = ring().lock().unwrap_or_else(|e| e.into_inner());
    r.next_listener += 1;
    let id = r.next_listener;
    r.listeners.push((id, f));
    id
}

pub fn unlisten(id: u64) {
    let mut r = ring().lock().unwrap_or_else(|e| e.into_inner());
    r.listeners.retain(|(i, _)| *i != id);
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct RingLayer;

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = match *meta.level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        };
        if level > LogLevel::Info {
            // 只保留 info 及以上：debug / trace 太多，而且可能刷掉有用的日志。
            return;
        }
        let mut v = MessageVisitor::default();
        event.record(&mut v);
        let listeners = {
            let mut r = ring().lock().unwrap_or_else(|e| e.into_inner());
            r.seq += 1;
            let line = LogLine {
                seq: r.seq,
                at_ms: now_ms(),
                level,
                target: meta.target().to_string(),
                message: format!("{}{}", v.message, v.fields),
            };
            if r.lines.len() >= CAPACITY {
                r.lines.pop_front();
            }
            r.lines.push_back(line.clone());
            (r.listeners.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>(), line)
        };
        for f in listeners.0 {
            f(&listeners.1);
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone)]
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn diagnostic_burst_is_bounded_without_hiding_faults_or_other_layers() {
        struct Observer(Arc<AtomicUsize>);
        impl<S: Subscriber> Layer<S> for Observer {
            fn on_event(&self, _: &Event<'_>, _: Context<'_, S>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(output.clone());
        let observed = Arc::new(AtomicUsize::new(0));
        let layer = tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .with_filter(local_log_filter());
        let subscriber = tracing_subscriber::registry()
            .with(layer)
            .with(Observer(observed.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for attempt in 0..1000 {
                tracing::info!(target: "oink_link_diagnostics", attempt, "connection attempt");
            }
            tracing::warn!("admission retry backed off");
            tracing::error!(target: "oink_link_diagnostics", "link fault");
        });
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(
            text.lines().count() <= 130,
            "ordinary logs must bound diagnostic bursts"
        );
        assert!(text.contains("admission retry backed off"));
        assert!(text.contains("link fault"));
        assert_eq!(
            observed.load(Ordering::Relaxed),
            1002,
            "diagnostic layer retains its independent budget"
        );
    }
    #[test]
    fn link_decisions_survive_exhausted_bootstrap_budget() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(output.clone());
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(local_log_filter()),
        );
        tracing::subscriber::with_default(subscriber, || {
            for attempt in 0..128 {
                tracing::info!(target: "oink_link_diagnostics", kind = "low_step", attempt, "bootstrap");
            }
            tracing::info!(target: "oink_link_diagnostics", kind = "low_control", "budget exhausted");
            tracing::info!(target: "oink_link_diagnostics", kind = "link_lost", reason = "heartbeat_timeout", "decision survives");
            tracing::info!(target: "oink_link_diagnostics", kind = "path_transition", "path survives");
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", "connection survives");
            tracing::warn!(target: "oink_link_diagnostics", kind = "udp_send", "error context survives");
            tracing::info!(target: "oink_link_diagnostics", kind = "candidate_dial", candidate = "192.0.2.1:47470",
                result = "completed", reason = "authenticated", "winning candidate survives");
            tracing::info!(target: "oink_link_diagnostics", kind = "candidate_dial", result = "entered", "dial start bounded");
            tracing::info!(target: "oink_link_diagnostics", kind = "candidate_dial", result = "failed", "dial failure bounded");
            tracing::info!(target: "oink_dnd_read", kind = "connection_step", "unrelated target remains bounded");
        });
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 133);
        for expected in [
            "decision survives",
            "path survives",
            "connection survives",
            "error context survives",
            "winning candidate survives",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("dial start bounded"));
        assert!(!text.contains("dial failure bounded"));
        assert!(!text.contains("budget exhausted"));
        assert!(!text.contains("unrelated target remains bounded"));
    }
    #[test]
    fn diagnostic_budget_recovers_on_its_window_without_a_wall_clock() {
        let now = Instant::now();
        let mut budget = DiagnosticBudget::default();
        for _ in 0..128 {
            assert!(budget.admit(now));
        }
        assert!(!budget.admit(now));
        assert!(!budget.admit(now + Duration::from_secs(59)));
        assert!(budget.admit(now + Duration::from_secs(60)));
    }
    #[test]
    fn live_level_reload_filters_ordinary_output_without_filtering_other_layers() {
        struct Observer(Arc<AtomicUsize>);
        impl<S: Subscriber> Layer<S> for Observer {
            fn on_event(&self, _: &Event<'_>, _: Context<'_, S>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(output.clone());
        let observed = Arc::new(AtomicUsize::new(0));
        let (filter, handle) = tracing_subscriber::reload::Layer::new(EnvFilter::new("info"));
        let ordinary = tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .with_filter(filter);
        let subscriber = tracing_subscriber::registry()
            .with(ordinary)
            .with(Observer(observed.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("visible info");
            handle.reload(EnvFilter::new("error")).unwrap();
            tracing::info!("filtered info");
            tracing::error!("visible error");
        });
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(text.contains("visible info"));
        assert!(text.contains("visible error"));
        assert!(!text.contains("filtered info"));
        assert_eq!(observed.load(Ordering::Relaxed), 3);
        assert_eq!(
            set_level("info,oink=trace").unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    #[test]
    fn bundle_only_records_do_not_duplicate_local_transfer_lines() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(output.clone());
        let layer = tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .with_filter(tracing_subscriber::filter::filter_fn(local_log_metadata));
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!(target: "oink_local_transfer", "file transfer started");
            tracing::info!(target: "oink_transfer_diagnostics", kind="transfer_start", "file transfer boundary");
            tracing::info!(target: "oink_pacing_diagnostics", kind="pacing_sample", "Low pacing observation");
            tracing::info!(target: "oink_touchpad_diagnostics", stage=1_u8, "contact availability");
            tracing::info!(target: "oink_unrelated", bytes=123_u64, "ordinary log");
        });
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("file transfer started"));
        assert!(text.contains("ordinary log"));
        assert!(!text.contains("file transfer boundary"));
        assert!(!text.contains("Low pacing observation"));
        assert!(!text.contains("contact availability"));
    }
}

/// Observe before native unwinding destroys the throwing frames. Raw NSException
/// text is kept in a bounded local sidecar, outside ordinary logs/ZIPs/telemetry.
/// This follows local diagnostic recording, independently of upload consent.
#[cfg(target_os = "macos")]
pub(crate) fn set_native_exception_capture(enabled: bool) {
    static CAPTURE: OnceLock<Mutex<Option<oink_crash::objc::Capture>>> = OnceLock::new();
    let mut slot = CAPTURE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let directory = log_dir().join("NativeExceptions");
    if enabled {
        if slot.is_none() {
            match oink_crash::objc::Capture::start(&directory) {
                Ok(capture) => *slot = Some(capture),
                Err(error) => tracing::warn!(%error, "native exception diagnostic capture unavailable"),
            }
        }
    } else {
        slot.take();
        if let Err(error) = oink_crash::objc::erase(&directory) {
            tracing::warn!(%error, "native exception diagnostic cleanup failed");
        }
    }
}
