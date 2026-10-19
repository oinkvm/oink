//! Measurement projections shared by the source and UI state comparators.
//! Clocks/counters never decide whether a semantic state event is published.
use crate::api::{AudioStatistics, DeviceAudioStatistics, DiagnosticRecorderStatus, DiagnosticStatistics, GroupView};
use oink_audio::DeviceAudioState;
use oink_audio::Status;

pub(crate) fn group_state(value: &GroupView) -> GroupView {
    let mut state = value.clone();
    for member in &mut state.members {
        if let Some(link) = &mut member.link {
            link.rtt_ms = None;
            link.loss_pct = 0.;
        }
    }
    state
}

pub(crate) fn device_audio_state(value: &DeviceAudioState) -> DeviceAudioState {
    let mut state = value.clone();
    state.sent = 0;
    state.send_dropped = 0;
    for output in &mut state.outputs {
        output.reported_latency_ms = None;
    }
    for receiver in &mut state.receivers {
        receiver.received = 0;
        receiver.concealed = 0;
        receiver.late = 0;
        receiver.buffered_ms = 0.;
    }
    state.outputs.sort_by(|a, b| a.output.cmp(&b.output));
    state.receivers.sort_by_key(|r| r.source);
    state
}

pub(crate) fn audio_state(value: &Status) -> Status {
    let mut state = value.clone();
    state.sent = 0;
    state.send_dropped = 0;
    state.capture_levels = None;
    state.network_rtt_ms = None;
    state.estimated_latency_ms = None;
    for (_, stats) in &mut state.sources {
        *stats = Default::default();
    }
    state.sources.sort_by_key(|(peer, _)| *peer);
    for report in &mut state.device_states {
        report.state = device_audio_state(&report.state);
        report.age_ms = 0;
    }
    state.device_states.sort_by_key(|r| r.device);
    if let Some(info) = &mut state.capture_info {
        info.diagnostics = serde_json::Value::Null;
    }
    if let Some(info) = &mut state.playback_info {
        info.reported_output_latency_ms = None;
    }
    state
}

pub(crate) fn diagnostic_state(value: &DiagnosticRecorderStatus) -> DiagnosticRecorderStatus {
    DiagnosticRecorderStatus {
        enabled: value.enabled,
        configured_enabled: value.configured_enabled,
        suspended: value.suspended,
        recovering: value.recovering,
        limit_bytes: value.limit_bytes,
        ..Default::default()
    }
}

impl From<&Status> for AudioStatistics {
    fn from(value: &Status) -> Self {
        Self {
            capture_levels: value.capture_levels,
            capture_diagnostics: value.capture_info.as_ref().map(|info| info.diagnostics.clone()),
            playback_latency_ms: value
                .playback_info
                .as_ref()
                .and_then(|info| info.reported_output_latency_ms),
            sent: value.sent,
            send_dropped: value.send_dropped,
            sources: value.sources.clone(),
            network_rtt_ms: value.network_rtt_ms,
            estimated_latency_ms: value.estimated_latency_ms,
            device_states: value
                .device_states
                .iter()
                .map(|report| DeviceAudioStatistics {
                    device: report.device,
                    age_ms: report.age_ms,
                    sent: report.state.sent,
                    send_dropped: report.state.send_dropped,
                    outputs: report.state.outputs.clone(),
                    receivers: report.state.receivers.clone(),
                })
                .collect(),
        }
    }
}
impl From<&DiagnosticRecorderStatus> for DiagnosticStatistics {
    fn from(value: &DiagnosticRecorderStatus) -> Self {
        Self {
            bytes: value.bytes,
            queued: value.queued,
            dropped: value.dropped,
            errors: value.errors,
            segments: value.segments,
            oldest_utc_us: value.oldest_utc_us,
            newest_utc_us: value.newest_utc_us,
            corrupt_segments: value.corrupt_segments,
            discarded_segments: value.discarded_segments,
            evicted_segments: value.evicted_segments,
        }
    }
}
