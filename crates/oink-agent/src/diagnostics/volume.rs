//! Reviewed volume transition fields only. No raw key or endpoint identifiers.
use super::*;
use oink_routing::router::{VolumeAction, VolumeDecision, VolumeDestination, VolumeOriginKind};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeRefused {
    pub press_id: u64,
    pub reason: u32,
    pub source_alias: String,
    pub target_alias: String,
    pub action: u32,
}
impl Recorder {
    pub(crate) fn volume_refused(
        &self,
        source: DeviceId,
        target: DeviceId,
        press_id: u64,
        action: oink_routing::input::VolumeKey,
        reason: oink_protocol::v1::dto::ControlFailure,
    ) {
        use oink_routing::input::VolumeKey;
        let anon = self.anonymizer();
        self.record(Event::VolumeRefused(VolumeRefused {
            press_id,
            reason: reason.number(),
            source_alias: anon.device(source),
            target_alias: anon.device(target),
            action: match action {
                VolumeKey::Up => 0,
                VolumeKey::Down => 1,
                VolumeKey::Mute => 2,
            },
        }));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeRoutingEvent {
    pub action: VolumeAction,
    pub key_origin: VolumeOriginKind,
    pub source_kind: oink_foundation::input::SourceKind,
    pub source: String,
    pub focus: String,
    pub decided_target: VolumeDestination,
    pub target_device: String,
    pub reason: String,
    pub focus_forwards: bool,
    pub focus_keep_local_playback: Option<bool>,
    pub source_keep_local_playback: Option<bool>,
    pub windows_context: bool,
    pub recent_default_endpoint_change: bool,
    pub default_endpoint_age_ms: Option<u32>,
    pub recent_audio_arrival_removal: bool,
    pub arrival_removal_age_ms: Option<u32>,
}
pub(super) fn export(recorder: &Recorder, record: VolumeDecision) -> VolumeRoutingEvent {
    let windows = record.windows_notifications.unwrap_or_default();
    VolumeRoutingEvent {
        action: record.action,
        key_origin: record.origin_kind,
        source_kind: record.source_kind,
        source: recorder.anonymizer().device(record.source),
        focus: recorder.anonymizer().device(record.focus),
        decided_target: record.target,
        target_device: recorder.anonymizer().device(record.target_device),
        reason: record.reason().into(),
        focus_forwards: record.focus_forwards,
        focus_keep_local_playback: record.focus_keep_local_playback,
        source_keep_local_playback: record.source_keep_local_playback,
        windows_context: record.windows_notifications.is_some(),
        recent_default_endpoint_change: windows.default_endpoint_age_ms.is_some(),
        default_endpoint_age_ms: windows.default_endpoint_age_ms,
        recent_audio_arrival_removal: windows.arrival_removal_age_ms.is_some(),
        arrival_removal_age_ms: windows.arrival_removal_age_ms,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::input::SourceKind;
    #[test]
    fn typed_transition_exports_fixed_actions_and_anonymized_devices_without_key_codes() {
        let dir = tempfile::tempdir().unwrap();
        let me = DeviceId([1; 32]);
        let recorder = Recorder::start_inner(
            dir.path().into(),
            me,
            None,
            true,
            false,
            clock::Clock::system(),
            None,
            None,
            false,
        )
        .unwrap();
        let raw = VolumeDecision {
            action: VolumeAction::GainDown,
            origin_kind: VolumeOriginKind::Synthetic,
            source_kind: SourceKind::Unknown,
            source: me,
            focus: DeviceId([2; 32]),
            target: VolumeDestination::Gain,
            target_device: DeviceId([2; 32]),
            focus_forwards: true,
            focus_keep_local_playback: Some(true),
            source_keep_local_playback: Some(false),
            windows_notifications: Some(oink_foundation::input::RecentAudioNotifications {
                default_endpoint_age_ms: Some(40),
                arrival_removal_age_ms: Some(14),
            }),
        };
        let trace = input_trace::InputTrace::for_recorder(&recorder);
        use oink_routing::router::RoutingDiagnostics;
        trace.volume(raw);
        trace.drain(&recorder);
        use tracing_subscriber::prelude::*;
        let subscriber = tracing_subscriber::registry().with(super::super::trace::scoped(&recorder));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_volume_routing", "audio volume routing decision");
        });
        recorder.wait_recovery();
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        assert_eq!(
            rows.iter()
                .filter(|row| row["event"].get("VolumeRouting").is_some())
                .count(),
            1
        );
        assert!(
            !rows.iter().any(|row| row["event"].get("Log").is_some()),
            "typed record replaces generic partial tracing record"
        );
        let event = Event::VolumeRouting(export(&recorder, raw));
        let text = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let record = &parsed["VolumeRouting"];
        assert_eq!(record["action"], "gain_down");
        assert_eq!(record["key_origin"], "synthetic");
        assert_eq!(record["reason"], "focus_forwarding_gain");
        assert_eq!(record["recent_default_endpoint_change"], true);
        assert_eq!(record["default_endpoint_age_ms"], 40);
        assert_eq!(record["source"], recorder.anonymizer().device(me));
        for private in ["KeyCode", "scanCode", "vkCode", "pressed", "repeat", "key_code"] {
            assert!(!text.contains(private));
        }
        assert!(matches!(
            serde_json::from_str::<Event>(&text).unwrap(),
            Event::VolumeRouting(_)
        ));
    }
}
