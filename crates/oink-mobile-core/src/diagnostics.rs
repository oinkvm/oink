use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use crate::model::{Peer, PeerKind, PeerState};

mod log;

pub use log::{DiagnosticLog, LogLevel, LogValue};

/// A bounded, in-memory segment handed to the current diagnostic proxy. The
/// proxy owns v2 Message encoding, cross-owner de-duplication, and export state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticSegment {
    pub mobile_device_id: String,
    pub session_id: String,
    pub batch_id: String,
    pub proxy_id: Option<String>,
    pub sequence: u64,
    pub offset: u64,
    pub events: Vec<DiagnosticEvent>,
    /// Redacted Rust log records (`DiagnosticLog`).
    pub logs: Vec<DiagnosticLog>,
    pub bytes: usize,
    /// Events and log records evicted before this segment was cut.
    pub evicted_events: u64,
}

/// Payload of a mobile diagnostics upload. Device and operation identity are
/// carried by the authenticated v2 diagnostic envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticUpload {
    pub events: Vec<DiagnosticEvent>,
    pub logs: Vec<DiagnosticLog>,
    /// Events and log records lost on the phone before this upload.
    pub dropped_events: u64,
}

impl DiagnosticUpload {
    pub const MAX_BYTES: usize = 256 * 1024;
}

pub(crate) const AGGREGATE_WINDOW_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticKind {
    Status,
    Error,
    Link,
    Audio,
    Transfer,
    Privileged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticCode {
    Started,
    Stopped,
    Failed,
    Retry,
    PermissionDenied,
    Connected,
    Disconnected,
    Packet,
    PacketLoss,
    EndpointStarted,
    EndpointStopped,
    TransferStarted,
    TransferFinished,
    PrivilegedEnabled,
    PrivilegedDisabled,
    /// Case A AEC bypassed blocks (no valid reference / not eligible):
    /// `count` = bypassed blocks, `metric` = all AEC blocks in the window.
    AecBypass,
    /// Case A AEC worker over its block budget: `count` = missed blocks,
    /// `metric` = all AEC blocks in the window.
    AecDeadlineMiss,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub timestamp_ms: u64,
    pub kind: DiagnosticKind,
    pub code: DiagnosticCode,
    pub metric: Option<u64>,
    pub count: u32,
}

impl DiagnosticEvent {
    pub fn validate(self) -> Result<(), String> {
        if self.count == 0 || self.timestamp_ms > (i64::MAX as u64) / 1_000 {
            return Err("invalid diagnostic count or timestamp".into());
        }
        let allowed = match self.kind {
            DiagnosticKind::Status => matches!(self.code, DiagnosticCode::Started | DiagnosticCode::Stopped),
            DiagnosticKind::Error => matches!(
                self.code,
                DiagnosticCode::Failed | DiagnosticCode::Retry | DiagnosticCode::PermissionDenied
            ),
            DiagnosticKind::Link => matches!(
                self.code,
                DiagnosticCode::Connected
                    | DiagnosticCode::Disconnected
                    | DiagnosticCode::Packet
                    | DiagnosticCode::PacketLoss
            ),
            DiagnosticKind::Audio => matches!(
                self.code,
                DiagnosticCode::EndpointStarted
                    | DiagnosticCode::EndpointStopped
                    | DiagnosticCode::AecBypass
                    | DiagnosticCode::AecDeadlineMiss
            ),
            DiagnosticKind::Transfer => matches!(
                self.code,
                DiagnosticCode::TransferStarted | DiagnosticCode::TransferFinished
            ),
            DiagnosticKind::Privileged => matches!(
                self.code,
                DiagnosticCode::PrivilegedEnabled | DiagnosticCode::PrivilegedDisabled
            ),
        };
        if allowed {
            Ok(())
        } else {
            Err("diagnostic code is not allowed for this event kind".into())
        }
    }

    fn estimated_bytes(self) -> usize {
        32
    }
}

pub trait DiagnosticSink: Send + Sync {
    /// The sink owns forwarding and aggregation. Core never retries by
    /// silently storing segments locally; a failed sink is reported to caller.
    fn submit_segment(&self, segment: DiagnosticSegment) -> Result<(), String>;
}

#[derive(Debug)]
struct Pending {
    events: Vec<DiagnosticEvent>,
    /// Each record with its encoded size.
    logs: Vec<(DiagnosticLog, usize)>,
    bytes: usize,
    evicted_events: u64,
}

#[derive(Debug)]
pub struct MobileDiagnostics<S> {
    sink: Arc<S>,
    mobile_device_id: String,
    session_id: String,
    batch_id: String,
    next_sequence: Mutex<u64>,
    proxy_id: Mutex<Option<String>>,
    pending: Mutex<Pending>,
    max_pending_bytes: usize,
    segment_bytes: usize,
}

impl<S: DiagnosticSink> MobileDiagnostics<S> {
    pub const MOBILE_TOTAL_BUDGET: usize = 64 * 1024 * 1024;
    pub const SEGMENT_BYTES: usize = 16 * 1024;

    pub fn new(sink: Arc<S>, mobile_device_id: impl Into<String>) -> Self {
        Self::with_ids(
            sink,
            mobile_device_id,
            "mobile-session-v2",
            "mobile-batch-v2",
            Self::MOBILE_TOTAL_BUDGET,
            Self::SEGMENT_BYTES,
        )
    }

    pub fn with_limits(
        sink: Arc<S>,
        mobile_device_id: impl Into<String>,
        max_pending_bytes: usize,
        segment_bytes: usize,
    ) -> Self {
        Self::with_ids(
            sink,
            mobile_device_id,
            "mobile-session-v2",
            "mobile-batch-v2",
            max_pending_bytes,
            segment_bytes,
        )
    }

    pub fn with_ids(
        sink: Arc<S>,
        mobile_device_id: impl Into<String>,
        session_id: impl Into<String>,
        batch_id: impl Into<String>,
        max_pending_bytes: usize,
        segment_bytes: usize,
    ) -> Self {
        let max_pending_bytes = max_pending_bytes.min(Self::MOBILE_TOTAL_BUDGET);
        Self {
            sink,
            mobile_device_id: mobile_device_id.into(),
            session_id: session_id.into(),
            batch_id: batch_id.into(),
            next_sequence: Mutex::new(1),
            proxy_id: Mutex::new(None),
            pending: Mutex::new(Pending {
                events: Vec::new(),
                logs: Vec::new(),
                bytes: 0,
                evicted_events: 0,
            }),
            max_pending_bytes,
            segment_bytes: segment_bytes.clamp(1, Self::SEGMENT_BYTES),
        }
    }

    /// Selects the same first Ready PC used by transfer proxy routing. A
    /// switch updates unsent segments so a retry follows the current owner.
    pub fn set_peers(&self, peers: &[Peer]) -> Option<String> {
        let selected = peers
            .iter()
            .filter(|peer| eligible_proxy(peer))
            .min_by_key(|peer| (peer.relayed, peer.connection_order))
            .map(|peer| peer.id.clone());
        if let Ok(mut proxy) = self.proxy_id.lock() {
            *proxy = selected.clone();
        }
        selected
    }

    pub fn set_proxy(&self, proxy_id: Option<String>) -> Result<(), String> {
        if proxy_id.as_deref().is_some_and(str::is_empty) {
            return Err("proxy id cannot be empty".into());
        }
        self.proxy_id
            .lock()
            .map_err(|_| "diagnostic proxy lock poisoned".to_owned())
            .map(|mut proxy| *proxy = proxy_id)
    }

    /// Adds to a short in-memory window. The host can call flush on its timer
    /// or before an owner handoff; no raw payload can enter this collector.
    pub fn emit(&self, event: DiagnosticEvent) -> Result<(), String> {
        event.validate()?;
        let event_bytes = event.estimated_bytes();
        let should_flush = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| "diagnostic collector lock poisoned".to_owned())?;
            if matches!(
                event.code,
                DiagnosticCode::Packet
                    | DiagnosticCode::PacketLoss
                    | DiagnosticCode::AecBypass
                    | DiagnosticCode::AecDeadlineMiss
            ) && let Some(previous) = pending.events.iter_mut().rev().find(|previous| {
                previous.code == event.code
                    && previous.timestamp_ms / AGGREGATE_WINDOW_MS == event.timestamp_ms / AGGREGATE_WINDOW_MS
            }) {
                previous.count = previous.count.saturating_add(event.count);
                previous.metric = if matches!(event.code, DiagnosticCode::AecBypass | DiagnosticCode::AecDeadlineMiss) {
                    // Window totals: the denominator adds up like the count.
                    Some(previous.metric.unwrap_or(0).saturating_add(event.metric.unwrap_or(0)))
                } else {
                    event.metric.or(previous.metric)
                };
                return Ok(());
            }
            if !self.make_room(&mut pending, event_bytes) {
                pending.evicted_events = pending.evicted_events.saturating_add(u64::from(event.count));
                return Ok(());
            }
            pending.bytes = pending.bytes.saturating_add(event_bytes);
            pending.events.push(event);
            pending.bytes >= self.segment_bytes
        };
        if should_flush {
            self.flush()?;
        }
        Ok(())
    }

    /// Adds a redacted Rust log record (see `DiagnosticLog`). Records share
    /// the event budget and segment size, so a segment still holds at most
    /// `segment_bytes` plus the record that crossed it.
    pub fn emit_log(&self, log: DiagnosticLog) -> Result<(), String> {
        log.validate()?;
        let log_bytes = log.encoded_bytes();
        let should_flush = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| "diagnostic collector lock poisoned".to_owned())?;
            if !self.make_room(&mut pending, log_bytes) {
                pending.evicted_events = pending.evicted_events.saturating_add(1);
                return Ok(());
            }
            pending.bytes = pending.bytes.saturating_add(log_bytes);
            pending.logs.push((log, log_bytes));
            pending.bytes >= self.segment_bytes
        };
        if should_flush {
            self.flush()?;
        }
        Ok(())
    }

    /// Evicts the oldest events/records until `bytes` more fit; false when
    /// they never fit.
    fn make_room(&self, pending: &mut Pending, bytes: usize) -> bool {
        if bytes > self.max_pending_bytes {
            return false;
        }
        while pending.bytes.saturating_add(bytes) > self.max_pending_bytes {
            let event_first = match (pending.events.first(), pending.logs.first()) {
                (Some(event), Some((log, _))) => event.timestamp_ms <= log.timestamp_ms,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let (freed, count) = if event_first {
                let oldest = pending.events.remove(0);
                (oldest.estimated_bytes(), u64::from(oldest.count))
            } else {
                (pending.logs.remove(0).1, 1)
            };
            pending.bytes = pending.bytes.saturating_sub(freed);
            pending.evicted_events = pending.evicted_events.saturating_add(count);
        }
        true
    }

    pub fn flush(&self) -> Result<(), String> {
        let (events, logs, bytes, evicted_events) = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| "diagnostic collector lock poisoned".to_owned())?;
            if pending.events.is_empty() && pending.logs.is_empty() {
                return Ok(());
            }
            let events = std::mem::take(&mut pending.events);
            let logs = std::mem::take(&mut pending.logs)
                .into_iter()
                .map(|(log, _)| log)
                .collect();
            let bytes = pending.bytes;
            let evicted_events = pending.evicted_events;
            pending.bytes = 0;
            pending.evicted_events = 0;
            (events, logs, bytes, evicted_events)
        };
        let mut sequence = self
            .next_sequence
            .lock()
            .map_err(|_| "diagnostic sequence lock poisoned".to_owned())?;
        let segment = DiagnosticSegment {
            mobile_device_id: self.mobile_device_id.clone(),
            session_id: self.session_id.clone(),
            batch_id: self.batch_id.clone(),
            proxy_id: self
                .proxy_id
                .lock()
                .map_err(|_| "diagnostic proxy lock poisoned".to_owned())?
                .clone(),
            sequence: *sequence,
            offset: 0,
            events,
            logs,
            bytes,
            evicted_events,
        };
        *sequence = sequence.saturating_add(1);
        self.sink.submit_segment(segment)
    }
}

fn eligible_proxy(peer: &Peer) -> bool {
    peer.kind == PeerKind::Pc && peer.state == PeerState::Ready && peer.accepts_clipboard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Sink(Mutex<Vec<DiagnosticSegment>>);
    impl DiagnosticSink for Sink {
        fn submit_segment(&self, segment: DiagnosticSegment) -> Result<(), String> {
            self.0.lock().unwrap().push(segment);
            Ok(())
        }
    }

    #[test]
    fn collector_batches_and_caps_memory_without_disk() {
        let sink = Arc::new(Sink::default());
        let collector = MobileDiagnostics::with_limits(sink.clone(), "phone", 64, 64);
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 1,
                kind: DiagnosticKind::Status,
                code: DiagnosticCode::Started,
                metric: None,
                count: 1,
            })
            .unwrap();
        assert!(sink.0.lock().unwrap().is_empty());
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 2,
                kind: DiagnosticKind::Link,
                code: DiagnosticCode::Packet,
                metric: Some(8),
                count: 1,
            })
            .unwrap();
        let segments = sink.0.lock().unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].mobile_device_id, "phone");
        assert_eq!(segments[0].sequence, 1);
        assert_eq!(segments[0].bytes, 64);
        assert_eq!(segments[0].offset, 0);
    }

    #[test]
    fn collector_rejects_unallowlisted_code() {
        let sink = Arc::new(Sink::default());
        let collector = MobileDiagnostics::new(sink, "phone");
        assert!(
            collector
                .emit(DiagnosticEvent {
                    timestamp_ms: 1,
                    kind: DiagnosticKind::Audio,
                    code: DiagnosticCode::TransferStarted,
                    metric: None,
                    count: 1
                })
                .is_err()
        );
    }

    #[test]
    fn collector_uses_transfer_proxy_order_and_switches_owner() {
        let sink = Arc::new(Sink::default());
        let collector = MobileDiagnostics::with_ids(sink.clone(), "phone", "0f".repeat(16), "1a".repeat(16), 1024, 1);
        let peers = vec![
            Peer {
                id: "late".into(),
                kind: PeerKind::Pc,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 20,
                relayed: false,
                clipboard_pending: false,
            },
            Peer {
                id: "first".into(),
                kind: PeerKind::Pc,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 10,
                relayed: false,
                clipboard_pending: false,
            },
        ];
        assert_eq!(collector.set_peers(&peers).as_deref(), Some("first"));
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 1,
                kind: DiagnosticKind::Status,
                code: DiagnosticCode::Started,
                metric: None,
                count: 1,
            })
            .unwrap();
        assert_eq!(sink.0.lock().unwrap()[0].proxy_id.as_deref(), Some("first"));
        collector.set_proxy(Some("late".into())).unwrap();
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 2,
                kind: DiagnosticKind::Status,
                code: DiagnosticCode::Stopped,
                metric: None,
                count: 1,
            })
            .unwrap();
        assert_eq!(sink.0.lock().unwrap()[1].proxy_id.as_deref(), Some("late"));
    }

    #[test]
    fn aec_statistics_are_audio_events_summed_per_window() {
        let sink = Arc::new(Sink::default());
        let collector = MobileDiagnostics::new(sink.clone(), "phone");
        let event = DiagnosticEvent {
            timestamp_ms: 1,
            kind: DiagnosticKind::Audio,
            code: DiagnosticCode::AecBypass,
            metric: Some(100),
            count: 4,
        };
        collector.emit(event).unwrap();
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 2,
                metric: Some(50),
                count: 1,
                ..event
            })
            .unwrap();
        collector
            .emit(DiagnosticEvent {
                code: DiagnosticCode::AecDeadlineMiss,
                count: 2,
                ..event
            })
            .unwrap();
        assert!(
            collector
                .emit(DiagnosticEvent {
                    kind: DiagnosticKind::Link,
                    ..event
                })
                .is_err()
        );
        collector.flush().unwrap();
        let sent = sink.0.lock().unwrap();
        let events: Vec<_> = sent[0]
            .events
            .iter()
            .map(|event| (event.code, event.count, event.metric))
            .collect();
        assert_eq!(
            events,
            vec![
                (DiagnosticCode::AecBypass, 5, Some(150)),
                (DiagnosticCode::AecDeadlineMiss, 2, Some(100)),
            ]
        );
    }

    fn log(timestamp_ms: u64) -> DiagnosticLog {
        DiagnosticLog {
            timestamp_ms,
            level: LogLevel::Warn,
            module: "oink_network::link".into(),
            line: 42,
            fields: vec![("elapsed_ms".into(), LogValue::U64(timestamp_ms))],
        }
    }

    #[test]
    fn log_records_ride_in_segments_and_share_the_budget() {
        let sink = Arc::new(Sink::default());
        let one = log(1).encoded_bytes();
        let collector = MobileDiagnostics::with_limits(sink.clone(), "phone", one * 2 + 32, 1024);
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 1,
                kind: DiagnosticKind::Status,
                code: DiagnosticCode::Started,
                metric: None,
                count: 1,
            })
            .unwrap();
        for at in 2..=4 {
            collector.emit_log(log(at)).unwrap();
        }
        assert!(sink.0.lock().unwrap().is_empty());
        collector.flush().unwrap();
        let sent = sink.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        // The oldest entries made room: the event and the first record.
        assert!(sent[0].events.is_empty());
        assert_eq!(sent[0].logs, vec![log(3), log(4)]);
        assert_eq!(sent[0].evicted_events, 2);
        assert!(
            collector
                .emit_log(DiagnosticLog {
                    module: "a/b".into(),
                    ..log(5)
                })
                .is_err()
        );
    }

    #[test]
    fn a_segment_is_cut_once_its_records_reach_the_segment_size() {
        let sink = Arc::new(Sink::default());
        let one = log(1).encoded_bytes();
        let collector = MobileDiagnostics::with_limits(sink.clone(), "phone", 1024 * 1024, one * 3);
        for at in 1..=7 {
            collector.emit_log(log(at)).unwrap();
        }
        let sent = sink.0.lock().unwrap();
        assert_eq!(
            sent.iter().map(|segment| segment.logs.len()).collect::<Vec<_>>(),
            vec![3, 3]
        );
        assert!(sent.iter().all(|segment| segment.bytes == one * 3));
    }

    #[test]
    fn collector_aggregates_high_frequency_events_by_window() {
        let sink = Arc::new(Sink::default());
        let collector = MobileDiagnostics::new(sink.clone(), "phone");
        let event = DiagnosticEvent {
            timestamp_ms: 1,
            kind: DiagnosticKind::Link,
            code: DiagnosticCode::Packet,
            metric: Some(1),
            count: 2,
        };
        collector.emit(event).unwrap();
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: 2,
                count: 3,
                ..event
            })
            .unwrap();
        collector
            .emit(DiagnosticEvent {
                timestamp_ms: AGGREGATE_WINDOW_MS,
                ..event
            })
            .unwrap();
        collector.flush().unwrap();
        let sent = sink.0.lock().unwrap();
        assert_eq!(
            sent[0].events.iter().map(|event| event.count).collect::<Vec<_>>(),
            vec![5, 2]
        );
    }
}
