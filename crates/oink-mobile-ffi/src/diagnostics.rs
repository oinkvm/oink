use crate::{MobileCoreError, storage_error};
use oink_mobile_core::{
    DiagnosticCode, DiagnosticEvent, DiagnosticKind, MobileDiagnostics, NetworkDiagnosticSink, Peer,
};
use std::sync::Arc;

#[derive(uniffi::Object)]
pub struct MobileDiagnosticCollector {
    inner: MobileDiagnostics<NetworkDiagnosticSink>,
}

impl std::fmt::Debug for MobileDiagnosticCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileDiagnosticCollector").finish_non_exhaustive()
    }
}

impl MobileDiagnosticCollector {
    pub(crate) fn from_network(
        sink: NetworkDiagnosticSink,
        mobile_device_id: String,
        session_id: String,
        batch_id: String,
        max_pending_bytes: u64,
        segment_bytes: u32,
    ) -> Result<Arc<Self>, MobileCoreError> {
        let max_pending_bytes = usize::try_from(max_pending_bytes).map_err(|_| MobileCoreError::InvalidArgument {
            detail: "diagnostic pending limit exceeds platform size".into(),
        })?;
        let segment_bytes = usize::try_from(segment_bytes).map_err(|_| MobileCoreError::InvalidArgument {
            detail: "diagnostic segment limit exceeds platform size".into(),
        })?;
        let collector = Arc::new(Self {
            inner: MobileDiagnostics::with_ids(
                Arc::new(sink),
                mobile_device_id,
                session_id,
                batch_id,
                max_pending_bytes,
                segment_bytes,
            ),
        });
        // The process's Rust log records ride in this collector's segments.
        let target: Arc<dyn crate::logging::LogTarget> = collector.clone();
        crate::logging::attach(Arc::downgrade(&target));
        Ok(collector)
    }

    /// Core-owned events (e.g. AEC statistics from the audio owner).
    pub(crate) fn record(&self, event: DiagnosticEvent) -> Result<(), String> {
        self.inner.emit(event)
    }
}

impl crate::logging::LogTarget for MobileDiagnosticCollector {
    fn record_log(&self, log: oink_mobile_core::DiagnosticLog) {
        // Only a poisoned collector lock fails here; the record is lost like
        // any event of a broken collector.
        let _ = self.inner.emit_log(log);
    }
}

#[uniffi::export]
impl MobileDiagnosticCollector {
    pub fn emit(
        &self,
        timestamp_ms: u64,
        kind: String,
        code: String,
        metric: Option<u64>,
        count: u32,
    ) -> Result<(), MobileCoreError> {
        self.inner
            .emit(DiagnosticEvent {
                timestamp_ms,
                kind: parse_kind(&kind)?,
                code: parse_code(&code)?,
                metric,
                count,
            })
            .map_err(storage_error)
    }

    pub fn set_peers(&self, peers: Vec<crate::MobilePeer>) -> Result<Option<String>, MobileCoreError> {
        let peers = peers
            .into_iter()
            .map(crate::transfer::peer)
            .collect::<Result<Vec<Peer>, _>>()?;
        Ok(self.inner.set_peers(&peers))
    }

    pub fn set_proxy(&self, proxy_id: Option<String>) -> Result<(), MobileCoreError> {
        self.inner.set_proxy(proxy_id).map_err(storage_error)
    }

    pub fn flush(&self) -> Result<(), MobileCoreError> {
        self.inner.flush().map_err(storage_error)
    }
}

fn parse_kind(value: &str) -> Result<DiagnosticKind, MobileCoreError> {
    let kind = match value {
        "status" => DiagnosticKind::Status,
        "error" => DiagnosticKind::Error,
        "link" => DiagnosticKind::Link,
        "audio" => DiagnosticKind::Audio,
        "transfer" => DiagnosticKind::Transfer,
        "privileged" => DiagnosticKind::Privileged,
        _ => return Err(invalid("unknown diagnostic kind")),
    };
    Ok(kind)
}

fn parse_code(value: &str) -> Result<DiagnosticCode, MobileCoreError> {
    let code = match value {
        "started" => DiagnosticCode::Started,
        "stopped" => DiagnosticCode::Stopped,
        "failed" => DiagnosticCode::Failed,
        "retry" => DiagnosticCode::Retry,
        "permission_denied" => DiagnosticCode::PermissionDenied,
        "connected" => DiagnosticCode::Connected,
        "disconnected" => DiagnosticCode::Disconnected,
        "packet" => DiagnosticCode::Packet,
        "packet_loss" => DiagnosticCode::PacketLoss,
        "endpoint_started" => DiagnosticCode::EndpointStarted,
        "endpoint_stopped" => DiagnosticCode::EndpointStopped,
        "transfer_started" => DiagnosticCode::TransferStarted,
        "transfer_finished" => DiagnosticCode::TransferFinished,
        "privileged_enabled" => DiagnosticCode::PrivilegedEnabled,
        "privileged_disabled" => DiagnosticCode::PrivilegedDisabled,
        "aec_bypass" => DiagnosticCode::AecBypass,
        "aec_deadline_miss" => DiagnosticCode::AecDeadlineMiss,
        _ => return Err(invalid("unknown diagnostic code")),
    };
    Ok(code)
}

fn invalid(message: &str) -> MobileCoreError {
    MobileCoreError::InvalidArgument { detail: message.into() }
}
