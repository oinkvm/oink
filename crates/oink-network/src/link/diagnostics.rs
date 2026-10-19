//! Transition-only tracing. Addresses and error text stay in local logs; the
//! recorder projects reviewed labels and anonymizes device IDs before enqueueing.
use oink_foundation::DeviceId;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

pub(crate) fn hex(bytes: &[u8]) -> String {
    // Transition formatting is bounded; avoid a temporary allocation per byte.
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

pub(super) fn quic_reason(error: &quinn::ConnectionError) -> &'static str {
    match error {
        quinn::ConnectionError::VersionMismatch => "version_mismatch",
        quinn::ConnectionError::TransportError(_) => "transport_error",
        quinn::ConnectionError::ConnectionClosed(_) => "peer_connection_closed",
        quinn::ConnectionError::ApplicationClosed(_) => "peer_application_closed",
        quinn::ConnectionError::Reset => "peer_reset",
        quinn::ConnectionError::TimedOut => "timeout",
        quinn::ConnectionError::LocallyClosed => "locally_closed",
        quinn::ConnectionError::CidsExhausted => "cids_exhausted",
    }
}
pub(super) trait FailureReason: std::fmt::Display {
    fn reason(&self) -> &'static str;
    fn os_error(&self) -> Option<i32>;
}
impl FailureReason for crate::NetError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Closed => "cancelled",
            Self::Io(_) => "io_error",
            _ => "error",
        }
    }
    fn os_error(&self) -> Option<i32> {
        match self {
            Self::Io(error) => error.raw_os_error(),
            _ => None,
        }
    }
}
impl FailureReason for anyhow::Error {
    fn reason(&self) -> &'static str {
        if self.downcast_ref::<std::io::Error>().is_some() {
            "io_error"
        } else {
            "error"
        }
    }
    fn os_error(&self) -> Option<i32> {
        self.downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
    }
}

#[derive(Clone)]
pub(super) struct Steps(Arc<Mutex<State>>);
#[derive(Clone)]
struct State {
    peer: Option<DeviceId>,
    operation: Arc<str>,
    connection: Arc<str>,
    stage: &'static str,
    started: Instant,
    done: bool,
}
pub(super) struct StepGuard(pub Steps);
impl Steps {
    pub fn start(peer: Option<DeviceId>, operation: String, stage: &'static str) -> StepGuard {
        let operation: Arc<str> = operation.into();
        let trace = Self(Arc::new(Mutex::new(State {
            peer,
            connection: operation.clone(),
            operation,
            stage,
            started: Instant::now(),
            done: false,
        })));
        trace.emit("entered", "none", "", None);
        StepGuard(trace)
    }
    fn emit(&self, result: &'static str, reason: &'static str, error: &str, code: Option<i32>) {
        // Ordinary TLS/bootstrap work and the network owner share these step
        // labels. Never hold their metadata mutex across subscriber formatting
        // or a log write; Arc labels make this snapshot allocation-free.
        let st = self.0.lock().expect("diagnostic steps").clone();
        if result != "entered" {
            let stage = match st.stage {
                "tcp_connect" | "tcp_accept" => Some(super::timing::Stage::LowTcp),
                "tls" => Some(super::timing::Stage::LowTls),
                "bind_request" | "bind_reply" => Some(super::timing::Stage::LowBind),
                "deliver" => Some(super::timing::Stage::LowReady),
                _ => None,
            };
            if let Some(stage) = stage {
                super::timing::record(super::timing::Measurement::Stage(stage), st.started, Instant::now());
            }
        }
        tracing::info!(target: "oink_link_diagnostics", kind = "low_step",
            peer_id = st.peer.map(|p| p.to_hex()).unwrap_or_default().as_str(),
            operation = st.operation.as_ref(), connection = st.connection.as_ref(), stage = st.stage, result, reason,
            elapsed_ms = st.started.elapsed().as_secs_f64() * 1000., error, os_error = code.unwrap_or(0), os_error_known = code.is_some(),
            "Low bootstrap step");
    }
    pub fn enter(&self, stage: &'static str) {
        if !self.0.lock().expect("diagnostic steps").done {
            self.emit("completed", "none", "", None);
        }
        {
            let mut st = self.0.lock().expect("diagnostic steps");
            st.stage = stage;
            st.started = Instant::now();
            st.done = false;
        }
        self.emit("entered", "none", "", None);
    }
    pub fn peer(&self, peer: DeviceId) {
        self.0.lock().expect("diagnostic steps").peer = Some(peer);
    }
    pub fn connection(&self, connection: String) {
        let connection = connection.into();
        self.0.lock().expect("diagnostic steps").connection = connection;
    }
    pub fn operation(&self, operation: String) {
        let operation = operation.into();
        self.0.lock().expect("diagnostic steps").operation = operation;
    }
    pub fn end(&self, result: &'static str, reason: &'static str, error: &str) {
        if self.0.lock().expect("diagnostic steps").done {
            return;
        }
        self.emit(result, reason, error, None);
        self.0.lock().expect("diagnostic steps").done = true;
    }
    pub fn result<T, E: FailureReason>(&self, result: &Result<T, E>) {
        match result {
            Ok(_) => self.end("completed", "none", ""),
            Err(error) => self.fail(error),
        }
    }
    pub fn fail(&self, error: &impl FailureReason) {
        if self.0.lock().expect("diagnostic steps").done {
            return;
        }
        self.emit("failed", error.reason(), &error.to_string(), error.os_error());
        self.0.lock().expect("diagnostic steps").done = true;
    }
    pub fn stage(&self) -> &'static str {
        self.0.lock().expect("diagnostic steps").stage
    }
}
impl std::ops::Deref for StepGuard {
    type Target = Steps;
    fn deref(&self) -> &Steps {
        &self.0
    }
}
impl Drop for StepGuard {
    fn drop(&mut self) {
        self.end("failed", "cancelled", "operation dropped");
    }
}

pub(super) struct CandidateDial {
    pub peer: DeviceId,
    pub address: std::net::SocketAddr,
    pub operation: String,
    started: Instant,
    done: bool,
}
impl CandidateDial {
    pub fn start(peer: DeviceId, address: std::net::SocketAddr, operation: String) -> Self {
        let trace = Self {
            peer,
            address,
            operation,
            started: Instant::now(),
            done: false,
        };
        trace.emit("entered", "none", "");
        trace
    }
    fn emit(&self, result: &'static str, reason: &'static str, error: &str) {
        let candidate_class = match self.address.ip() {
            std::net::IpAddr::V4(ip) if ip.is_loopback() => "ipv4_loopback",
            std::net::IpAddr::V4(ip) if ip.is_link_local() => "ipv4_link_local",
            std::net::IpAddr::V4(ip) if ip.is_private() => "ipv4_private",
            std::net::IpAddr::V4(_) => "ipv4_global",
            std::net::IpAddr::V6(ip) if ip.is_loopback() => "ipv6_loopback",
            std::net::IpAddr::V6(ip) if ip.is_unicast_link_local() => "ipv6_link_local",
            std::net::IpAddr::V6(ip) if ip.is_unique_local() => "ipv6_private",
            std::net::IpAddr::V6(_) => "ipv6_global",
        };
        tracing::info!(target: "oink_link_diagnostics", kind = "candidate_dial", peer_id = self.peer.to_hex().as_str(),
            operation = self.operation.as_str(), candidate_class, candidate = %self.address,
            initiator = "local", stage = "quic", result, reason, error,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000., "Candidate dial");
    }
    pub fn end(&mut self, result: &'static str, reason: &'static str, error: &str) {
        if !self.done {
            super::timing::record(
                super::timing::Measurement::Stage(super::timing::Stage::Dial),
                self.started,
                Instant::now(),
            );
            self.emit(result, reason, error);
            self.done = true;
        }
    }
}
impl Drop for CandidateDial {
    fn drop(&mut self) {
        self.end("failed", "cancelled", "candidate cancelled");
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use tracing::{
        Subscriber,
        field::{Field, Visit},
    };
    use tracing_subscriber::{
        Layer,
        layer::{Context, SubscriberExt},
    };
    pub fn init() {
        static INIT: std::sync::Once = std::sync::Once::new();
        // Other unit-test threads can first visit these callsites without a
        // dispatcher. Keep global interest enabled; collection stays scoped.
        INIT.call_once(|| tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap());
    }
    type TraceRows = Vec<Vec<(String, String)>>;
    pub struct Capture(pub Arc<Mutex<TraceRows>>);
    struct Fields(Vec<(String, String)>);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push((field.name().into(), format!("{value:?}")));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.push((field.name().into(), value.into()));
        }
    }
    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if event.metadata().target() == "oink_link_diagnostics" {
                let mut fields = Fields(Vec::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }
    }
    #[test]
    fn timeout_owner_names_nested_tcp_step_and_cancelled_candidate_has_one_outcome() {
        init();
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(rows.clone()));
        let _default = tracing::subscriber::set_default(subscriber);
        let trace = Steps::start(Some(DeviceId([1; 32])), hex(&[2; 16]), "open_reply");
        let work = async {
            let nested = trace.0.clone();
            nested.enter("tcp_connect");
            nested.enter("tls");
            std::future::pending::<()>().await;
        };
        assert!(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async { tokio::time::timeout(std::time::Duration::from_millis(1), work).await })
                .is_err()
        );
        trace.end("failed", "timeout", "setup deadline");
        drop(trace);
        drop(CandidateDial::start(
            DeviceId([1; 32]),
            "127.0.0.1:47470".parse().unwrap(),
            hex(&[3; 16]),
        ));
        let rows = rows.lock().unwrap();
        let failed: Vec<_> = rows
            .iter()
            .filter(|r| r.contains(&("result".into(), "failed".into())))
            .collect();
        assert_eq!(failed.len(), 2);
        assert!(failed[0].contains(&("stage".into(), "tls".into())));
        assert!(failed[0].contains(&("reason".into(), "timeout".into())));
        assert!(failed[1].contains(&("reason".into(), "cancelled".into())));
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    use tracing_subscriber::{Layer, layer::SubscriberExt};
    struct BlockLog {
        ready: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl<S: tracing::Subscriber> Layer<S> for BlockLog {
        fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            self.ready.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
    }
    #[test]
    fn blocked_ordinary_log_subscriber_does_not_own_network_step_metadata() {
        let step = Steps::start(None, "01".repeat(16), "test");
        let trace = step.0.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let subscriber = tracing_subscriber::registry().with(BlockLog {
                ready,
                release: Mutex::new(proceed),
            });
            tracing::subscriber::with_default(subscriber, || trace.emit("completed", "none", "", None));
        });
        started.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        let available = step.0.0.try_lock().is_ok();
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            available,
            "ordinary log formatting held metadata used by the network owner"
        );
    }
}
