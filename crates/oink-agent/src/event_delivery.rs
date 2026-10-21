//! State changes and source-owned measurement samples are separate event paths.
mod statistics;
use crate::api::{ApiEvent, EventSink, GroupView, LiveViews};
pub(crate) use statistics::{audio_state, device_audio_state, diagnostic_state, group_state};

pub(crate) struct Subscriber {
    pub id: u64,
    pub sink: EventSink,
    pub views: LiveViews,
    group: Option<GroupView>,
    permissions: Option<Vec<crate::api::PermissionItem>>,
    device_cards: Option<(String, u64)>,
    blocked_devices: Option<(String, u64)>,
    audio: Option<oink_audio::Status>,
    diagnostics: Option<crate::api::DiagnosticRecorderStatus>,
    reports: Option<crate::telemetry::ReportStatus>,
}

impl Subscriber {
    pub fn new(id: u64, sink: EventSink, views: LiveViews) -> Self {
        Self {
            id,
            sink,
            views,
            group: None,
            permissions: None,
            device_cards: None,
            blocked_devices: None,
            audio: None,
            diagnostics: None,
            reports: None,
        }
    }

    pub fn accepts(&mut self, event: &ApiEvent) -> bool {
        fn changed<T: PartialEq>(last: &mut Option<T>, value: T) -> bool {
            if last.as_ref() == Some(&value) {
                return false;
            }
            *last = Some(value);
            true
        }
        fn advanced(last: &mut Option<(String, u64)>, stream: &str, revision: u64) -> bool {
            if last.as_ref().is_some_and(|(old, n)| old == stream && *n >= revision) {
                return false;
            }
            *last = Some((stream.to_owned(), revision));
            true
        }
        match event {
            ApiEvent::ReportStatusChanged(status) => {
                if self
                    .reports
                    .as_ref()
                    .is_some_and(|old| old.consent.generation > status.consent.generation)
                {
                    return false;
                }
                changed(&mut self.reports, *status)
            }
            ApiEvent::DeviceCards(view) => advanced(&mut self.device_cards, &view.stream, view.revision.0),
            ApiEvent::BlockedDevices { snapshot } => {
                advanced(&mut self.blocked_devices, &snapshot.stream, snapshot.revision.0)
            }
            ApiEvent::ConnectionQuality(_) => self.views.group_quality,
            ApiEvent::Stats(_) | ApiEvent::InputStats(_) => self.views.stats || self.views.group_quality,
            ApiEvent::AudioStats(_) | ApiEvent::AudioQuality(_) => self.views.audio_quality,
            ApiEvent::DiagnosticsStats(_) => self.views.diagnostics,
            ApiEvent::Permissions(value) => changed(&mut self.permissions, value.clone()),
            ApiEvent::Group(value) => changed(&mut self.group, group_state(value)),
            ApiEvent::Audio(value) => changed(&mut self.audio, audio_state(value)),
            ApiEvent::DiagnosticsStatus(value) => changed(&mut self.diagnostics, diagnostic_state(value)),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn initial_permissions_filter_duplicate_sensor_delivery_but_keep_grant_changes() {
        use crate::api::{PermissionItem, PermissionKind};
        use oink_foundation::input::PermissionState;
        let mut subscriber = Subscriber::new(1, Arc::new(|_| {}), LiveViews::default());
        let mut permission = PermissionItem {
            kind: PermissionKind::InputMonitoring,
            state: PermissionState::NotGranted,
            required: true,
        };
        assert!(subscriber.accepts(&ApiEvent::Permissions(vec![permission.clone()])));
        assert!(!subscriber.accepts(&ApiEvent::Permissions(vec![permission.clone()])));
        permission.state = PermissionState::Granted;
        assert!(subscriber.accepts(&ApiEvent::Permissions(vec![permission.clone()])));
        assert!(!subscriber.accepts(&ApiEvent::Permissions(vec![permission])));
    }

    #[test]
    fn stale_initial_report_status_cannot_restore_revoked_consent() {
        use crate::telemetry::{Coverage, ReportStatus};
        let mut subscriber = Subscriber::new(1, Arc::new(|_| {}), LiveViews::default());
        let mut status = ReportStatus {
            consent: Default::default(),
            configured: false,
            region: None,
            queued_performance: 0,
            queued_crashes: 0,
            delivery_disabled: true,
            rust_crash: Coverage::Unavailable,
            native_crash: Coverage::Unavailable,
            file_provider: Coverage::Unavailable,
        };
        status.consent.generation = 2;
        assert!(subscriber.accepts(&ApiEvent::ReportStatusChanged(status)));
        status.consent.generation = 1;
        assert!(!subscriber.accepts(&ApiEvent::ReportStatusChanged(status)));
        status.consent.generation = 2;
        assert!(!subscriber.accepts(&ApiEvent::ReportStatusChanged(status)));
        status.queued_crashes = 1;
        assert!(subscriber.accepts(&ApiEvent::ReportStatusChanged(status)));
    }

    #[test]
    fn immutable_snapshot_revision_filters_initial_delivery_races() {
        let mut subscriber = Subscriber::new(1, Arc::new(|_| {}), LiveViews::default());
        let mut snapshot = crate::device_cards::BlockedDevicesSnapshot {
            stream: "a".into(),
            revision: crate::device_cards::DecimalU64(3),
            entries: vec![],
        };
        assert!(subscriber.accepts(&ApiEvent::BlockedDevices {
            snapshot: snapshot.clone()
        }));
        assert!(!subscriber.accepts(&ApiEvent::BlockedDevices {
            snapshot: snapshot.clone()
        }));
        snapshot.revision.0 = 2;
        assert!(!subscriber.accepts(&ApiEvent::BlockedDevices {
            snapshot: snapshot.clone()
        }));
        snapshot.revision.0 = 4;
        assert!(subscriber.accepts(&ApiEvent::BlockedDevices {
            snapshot: snapshot.clone()
        }));
        snapshot.stream = "b".into();
        snapshot.revision.0 = 1;
        assert!(subscriber.accepts(&ApiEvent::BlockedDevices { snapshot }));
    }

    #[test]
    fn hidden_clients_receive_state_changes_but_not_quality_only_changes() {
        let mut hidden = Subscriber::new(1, Arc::new(|_| {}), LiveViews::default());
        let mut visible = Subscriber::new(2, Arc::new(|_| {}), LiveViews::ALL);
        let mut group = test_group();
        for client in [&mut hidden, &mut visible] {
            assert!(client.accepts(&ApiEvent::Group(group.clone())));
        }
        group.members.iter_mut().find_map(|m| m.link.as_mut()).unwrap().rtt_ms = Some(2.);
        assert!(!hidden.accepts(&ApiEvent::Group(group.clone())));
        assert!(!visible.accepts(&ApiEvent::Group(group.clone())));
        group.members[0].device.name.push_str(" renamed");
        assert!(hidden.accepts(&ApiEvent::Group(group)));
        let mut audio = oink_audio::Status {
            network_rtt_ms: Some(1.),
            ..Default::default()
        };
        for client in [&mut hidden, &mut visible] {
            assert!(client.accepts(&ApiEvent::Audio(audio.clone())));
        }
        audio.network_rtt_ms = Some(2.);
        assert!(!hidden.accepts(&ApiEvent::Audio(audio.clone())));
        assert!(!visible.accepts(&ApiEvent::Audio(audio.clone())));
        audio.problem = Some(oink_audio::Problem::new(
            oink_protocol::v1::dto::UserFacingCode::DeviceUnavailable,
        ));
        assert!(hidden.accepts(&ApiEvent::Audio(audio)));
        assert!(!hidden.accepts(&ApiEvent::InputStats(Default::default())));
        assert!(visible.accepts(&ApiEvent::InputStats(Default::default())));
        let quality = ApiEvent::ConnectionQuality(crate::connection_quality::History::default().view(0));
        assert!(!hidden.accepts(&quality));
        assert!(visible.accepts(&quality));
        let audio_quality = ApiEvent::AudioQuality(crate::audio_quality::History::default().view(0));
        assert!(!hidden.accepts(&audio_quality));
        assert!(visible.accepts(&audio_quality));
        assert!(!hidden.accepts(&ApiEvent::Stats(Vec::new())));
        assert!(visible.accepts(&ApiEvent::Stats(Vec::new())));
        let mut status = crate::api::DiagnosticRecorderStatus::default();
        for client in [&mut hidden, &mut visible] {
            assert!(client.accepts(&ApiEvent::DiagnosticsStatus(status.clone())));
        }
        status.bytes = 100;
        assert!(!hidden.accepts(&ApiEvent::DiagnosticsStatus(status.clone())));
        assert!(!visible.accepts(&ApiEvent::DiagnosticsStatus(status.clone())));
        status.suspended = true;
        assert!(hidden.accepts(&ApiEvent::DiagnosticsStatus(status)));
    }
}

#[cfg(test)]
mod measurement_tests;

#[cfg(test)]
fn test_group() -> GroupView {
    use crate::api::*;
    let device = oink_foundation::DeviceId([2; 32]);
    GroupView {
        id: None,
        members: vec![Member {
            device: DeviceSummary {
                id: device.into(),
                fingerprint: device.short(),
                name: "Peer".into(),
                os: Os::Windows,
                os_version: String::new(),
                model: None,
                app_version: String::new(),
            },
            is_self: false,
            state: PeerState::Online,
            reachable: true,
            facts_fresh: true,
            keyboard_disabled: None,
            mouse_mode: None,
            keyboard_mode: None,
            input_transform: None,
            screen_disabled: None,
            screen_local_input: None,
            sharing: SharingState::Establishing,
            link: Some(LinkInfo {
                kind: LinkKind::Direct,
                remote_address: None,
                rtt_ms: Some(1.),
                loss_pct: 0.,
                bulk: None,
                bulk_unavailable: None,
            }),
            added_by: None,
            added_at_ms: 0,
            display_count: 1,
            feel: None,
        }],
    }
}
