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
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{FilterExt, LevelFilter};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::api::{LogLevel, LogLine};

mod files;
pub use files::set_max_mb;

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
}

/// Initialize the UI role's separate ordinary log file.
pub fn init_ui() {
    install(configured_level(), file_writer("oink-ui"));
}

/// Ordinary logs and diagnostics share the platform log root and separate budgets.
pub fn log_dir() -> PathBuf {
    oink_foundation::paths::log_dir()
}

fn file_writer(prefix: &str) -> Option<NonBlocking> {
    static GUARD: OnceLock<WorkerGuard> = OnceLock::new();
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let appender = files::Writer::new(dir, prefix, || {
        crate::config::AgentConfig::load().map_or_else(
            |_| u64::from(crate::config::Settings::default().log_max_mb.get()),
            |config| u64::from(config.settings.log_max_mb.get()),
        )
    })
    .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    // 进程一直持有写线程（静态变量不析构：退出前最后几行可能来不及写入，可以接受）。
    GUARD.set(guard).ok()?;
    Some(writer)
}

fn local_log_metadata(metadata: &tracing::Metadata<'_>) -> bool {
    !crate::diagnostics::bundle_only_target(metadata.target())
}

fn install(directive: &str, file: Option<NonBlocking>) {
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
            .with_filter(filter.and(tracing_subscriber::filter::filter_fn(local_log_metadata)))
    });
    let result = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter.and(tracing_subscriber::filter::filter_fn(local_log_metadata))),
        )
        .with(file_layer)
        .with(crate::diagnostics::DiagnosticsLayer)
        .with(
            RingLayer.with_filter(
                ring_filter
                    .and(LevelFilter::INFO)
                    .and(tracing_subscriber::filter::filter_fn(local_log_metadata)),
            ),
        )
        .try_init();
    if result.is_ok() {
        let _ = LEVEL_RELOADERS.set(reloaders);
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
            tracing::info!(target: "oink_unrelated", bytes=123_u64, "ordinary log");
        });
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("file transfer started"));
        assert!(text.contains("ordinary log"));
        assert!(!text.contains("file transfer boundary"));
        assert!(!text.contains("Low pacing observation"));
    }
}
