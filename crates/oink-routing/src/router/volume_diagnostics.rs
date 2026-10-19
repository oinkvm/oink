//! One record at a volume press's final decision, never ordinary key records.
use super::*;
use crate::input::{VolumeKey, VolumeKeyOrigin};
use oink_foundation::input::RecentAudioNotifications;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeAction {
    VolumeUp,
    VolumeDown,
    Mute,
    GainUp,
    GainDown,
    ForwardMute,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeDestination {
    Source,
    /// A non-forwarding focus's own output, when the keyboard source differs.
    Focus,
    Exit,
    Gain,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeOriginKind {
    Physical,
    Synthetic,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeDecision {
    pub action: VolumeAction,
    pub origin_kind: VolumeOriginKind,
    pub source_kind: SourceKind,
    pub source: DeviceId,
    pub focus: DeviceId,
    pub target: VolumeDestination,
    pub target_device: DeviceId,
    pub focus_forwards: bool,
    pub focus_keep_local_playback: Option<bool>,
    pub source_keep_local_playback: Option<bool>,
    pub windows_notifications: Option<RecentAudioNotifications>,
}
impl VolumeDecision {
    pub fn reason(&self) -> &'static str {
        match self.target {
            VolumeDestination::Source | VolumeDestination::Focus => "focus_not_forwarding",
            VolumeDestination::Exit => "focus_forwarding_to_exit",
            VolumeDestination::Gain => "focus_forwarding_gain",
        }
    }
}

impl Router {
    pub fn set_volume_keep_local_probe(&mut self, probe: Arc<dyn Fn() -> bool + Send + Sync>) {
        self.volume_keep_local = Some(probe);
    }
    pub fn set_volume_notifications_probe(
        &mut self,
        probe: Arc<dyn Fn() -> Option<RecentAudioNotifications> + Send + Sync>,
    ) {
        self.volume_notifications = Some(probe);
    }
    pub(super) fn capture_volume_origin(&mut self, event: &CapturedEvent) {
        self.local_volume_origin = Some(VolumeKeyOrigin {
            physical: Some(event.physical),
            source_kind: event.source,
            keep_local_playback: self.volume_keep_local.as_ref().map(|probe| probe()),
            windows_notifications: self.volume_notifications.as_ref().and_then(|probe| probe()),
        });
    }
    pub(super) fn volume_decision(&mut self, source: DeviceId, code: KeyCode, exit: Option<DeviceId>, gain: bool) {
        let Some(key) = VolumeKey::from_code(code) else {
            return;
        };
        let origin = if source == self.me() {
            self.local_volume_origin
        } else {
            self.remote_volume_origins
                .remove(&source)
                .filter(|(action, _)| *action == key)
                .map(|(_, origin)| origin)
        };
        let action = match (key, gain) {
            (VolumeKey::Up, false) => VolumeAction::VolumeUp,
            (VolumeKey::Down, false) => VolumeAction::VolumeDown,
            (VolumeKey::Mute, false) => VolumeAction::Mute,
            (VolumeKey::Up, true) => VolumeAction::GainUp,
            (VolumeKey::Down, true) => VolumeAction::GainDown,
            (VolumeKey::Mute, true) => VolumeAction::ForwardMute,
        };
        let record = VolumeDecision {
            action,
            origin_kind: match origin.and_then(|o| o.physical) {
                Some(true) => VolumeOriginKind::Physical,
                Some(false) => VolumeOriginKind::Synthetic,
                None => VolumeOriginKind::Unknown,
            },
            source_kind: origin.map_or(SourceKind::Unknown, |o| o.source_kind),
            source,
            focus: self.me(),
            target: if gain {
                VolumeDestination::Gain
            } else if exit.is_some() {
                VolumeDestination::Exit
            } else if source == self.me() {
                VolumeDestination::Source
            } else {
                VolumeDestination::Focus
            },
            target_device: if gain { self.me() } else { exit.unwrap_or(self.me()) },
            focus_forwards: exit.is_some(),
            focus_keep_local_playback: self.volume_keep_local.as_ref().map(|probe| probe()),
            source_keep_local_playback: origin.and_then(|o| o.keep_local_playback),
            windows_notifications: origin.and_then(|o| o.windows_notifications),
        };
        let windows = record.windows_notifications.unwrap_or_default();
        tracing::info!(target: "oink_volume_routing", action=?record.action, key_origin=?record.origin_kind,
            reason=record.reason(),
            source_kind=?record.source_kind, source=?record.source, focus=?record.focus,
            decided_target=?record.target, target_device=?record.target_device,
            focus_forwards=record.focus_forwards, focus_keep_local_playback=?record.focus_keep_local_playback,
            source_keep_local_playback=?record.source_keep_local_playback,
            windows_context=record.windows_notifications.is_some(),
            recent_default_endpoint_change=windows.default_endpoint_age_ms.is_some(),
            default_endpoint_age_ms=?windows.default_endpoint_age_ms,
            recent_audio_arrival_removal=windows.arrival_removal_age_ms.is_some(),
            arrival_removal_age_ms=?windows.arrival_removal_age_ms,
            "audio volume routing decision");
        if let Some(sink) = &self.diagnostics {
            sink.volume(record);
        }
    }
}
