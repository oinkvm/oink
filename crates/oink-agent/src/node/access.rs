use super::*;
use oink_protocol::v1::{
    dto,
    schema::{WireDto, WireValue},
};
#[derive(Clone, PartialEq)]
pub(super) struct InputObservation(
    dto::SharingMode,
    dto::SharingMode,
    dto::ResourceFaults,
    dto::LocalOptionsSummary,
    Option<dto::GameObservation>,
);

fn device_game_observation(
    local: DeviceId,
    preference: oink_routing::GameMode,
    status: &crate::threads::input::InputStatus,
) -> dto::GameObservation {
    // The routing status follows the group focus holder, including on sources.
    // Facts describe this publisher only. A nonfocus owner's saved On remains
    // enabled; Auto uses only this owner's current capture evidence. Capture
    // evidence itself is focus-scoped to exclude our source-side cursor freeze.
    let own = status.game_status.as_ref().filter(|game| game.holder == local);
    let active = !status.paused && own.map_or(preference == oink_routing::GameMode::On, |game| game.active);
    let detected = own.is_some_and(|game| game.detected);
    dto::GameObservation::from_raw([(1, active.write()), (2, detected.write())].into()).expect("typed game observation")
}

impl Node {
    pub fn low_status(&self, peer: DeviceId) -> Option<oink_network::TcpBulkStatus> {
        let links = self.links().ok()?;
        let link = links.links().by_peer(peer)?.link;
        links.low_status(link)
    }
    pub fn low_pacing(&self, peer: DeviceId) -> Option<oink_network::LowPacingStatus> {
        self.low_status(peer)?.pacing
    }

    pub fn boot(&self) -> oink_foundation::session::Boot {
        self.inner.boot
    }
    pub fn group_id(&self) -> Option<oink_foundation::GroupId> {
        self.inner.group.get().map(|g| g.applied().group)
    }
    pub fn group_members(&self) -> Vec<DeviceId> {
        self.inner
            .group
            .get()
            .map(|g| g.applied().members.clone())
            .unwrap_or_default()
    }
    pub fn device_facts(&self, peer: DeviceId) -> Option<dto::DeviceFacts> {
        let group = self.inner.group.get()?;
        if peer == self.id() {
            return Some((*group.local_facts()).clone());
        }
        group
            .members()
            .members
            .iter()
            .find(|m| m.device == peer)
            .and_then(|m| m.facts.as_ref().map(|f| (**f).clone()))
    }
    pub fn known_facts(&self) -> Vec<dto::DeviceFacts> {
        self.inner
            .group
            .get()
            .map(|g| {
                g.members()
                    .members
                    .iter()
                    .filter_map(|m| m.facts.as_ref().map(|f| (**f).clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn fully_paused(&self) -> bool {
        lock(&self.inner.controls).pause.paused()
    }
    pub fn pause_status(&self) -> api::PauseStatus {
        let (paused, revision, state, safety_pending) = {
            let controls = lock(&self.inner.controls);
            (
                controls.pause.paused(),
                controls.pause.revision(),
                controls.pause.state(),
                controls.safety_pending(),
            )
        };
        let author = state
            .emergency_origin()
            .ok()
            .flatten()
            .and_then(|origin| oink_network::wire_identity::read_device(&origin).ok());
        let capture_detached = self.input_backend().capture_active() == Some(false);
        api::PauseStatus {
            paused,
            scope: api::PauseScope::Local,
            revision,
            author: author.map(Into::into),
            author_name: author
                .filter(|device| self.group_members().contains(device))
                .map(|device| self.device_name(device)),
            emergency: state.reason() == Ok(dto::PauseReason::Emergency),
            reason: match state.reason().ok() {
                Some(dto::PauseReason::None) => crate::device_cards::PauseReason::None,
                Some(dto::PauseReason::LocalUser) => crate::device_cards::PauseReason::LocalUser,
                Some(dto::PauseReason::RemoteUser) => crate::device_cards::PauseReason::RemoteUser,
                Some(dto::PauseReason::Emergency) => crate::device_cards::PauseReason::Emergency,
                Some(dto::PauseReason::InputBacklog) => crate::device_cards::PauseReason::InputBacklog,
                _ => crate::device_cards::PauseReason::Unknown,
            },
            capture_detached,
            // Native pause can finish before durable safety cleanup. Resume is
            // still Busy until those owner workers finish, so keep it settling.
            settling: safety_pending
                || self.input_status().is_ok_and(|status| {
                    status.paused != paused || paused && (!capture_detached || !status.injection_detached)
                }),
        }
    }
    pub(crate) fn game_status(&self) -> oink_routing::GameModeStatus {
        self.input_status()
            .ok()
            .and_then(|s| s.game_status.clone())
            .unwrap_or(oink_routing::GameModeStatus {
                holder: self.id(),
                epoch: 0,
                mode: oink_routing::GameMode::Off,
                detected: false,
                active: false,
                cursor_hiding_cause_available: None,
            })
    }
    pub(crate) fn publish_local_options(&self) {
        let Some(group) = self.inner.group.get() else { return };
        // Serialize sensor publications and read after admission to this gate,
        // so a slower platform observer cannot overwrite a newer input sample.
        let mut previous = lock(&self.inner.input_observation);
        let Ok(status) = self.input_status() else { return };
        fn mode(mode: oink_foundation::input::InputMode) -> dto::SharingMode {
            match mode {
                oink_foundation::input::InputMode::Shared => dto::SharingMode::Shared,
                oink_foundation::input::InputMode::Local => dto::SharingMode::Local,
            }
        }
        fn fault(code: Option<dto::ResourceFaultCode>) -> Option<dto::ResourceFault> {
            code.map(|code| dto::ResourceFault::from_raw([(1, code.write())].into()).expect("typed native fault"))
        }
        let permissions = self.input_backend().permissions();
        use crate::threads::input::faults::permission_fault;
        let mouse_permission = permission_fault(&[permissions.accessibility]);
        // A live tap proves listen access even when TCC's cached preflight is stale.
        // Keep the warning in Overview, but do not fault a working keyboard source.
        let listen = if self.input_backend().capture_active() == Some(true) {
            PermissionState::Granted
        } else {
            permissions.input_monitoring
        };
        let keyboard_permission = permission_fault(&[permissions.accessibility, listen]);
        let faults = dto::ResourceFaults::from_raw(
            [
                (1, fault(status.resource_faults.screen).write()),
                (2, fault(mouse_permission.or(status.resource_faults.mouse)).write()),
                (
                    3,
                    fault(keyboard_permission.or(status.resource_faults.keyboard)).write(),
                ),
            ]
            .into(),
        )
        .expect("typed resource observations");
        let settings = self.settings();
        let options = dto::LocalOptionsSummary::from_raw(
            [
                (1, (!self.fully_paused()).write()),
                (2, settings.audio.forwarding_enabled.write()),
                (3, (f32::from(settings.audio.forwarding_volume_percent) / 100.).write()),
                (4, status.group_focus.locked_device.is_some().write()),
                (5, status.keyboard_disabled.write()),
                (6, status.screen_local_input.write()),
            ]
            .into(),
        )
        .expect("typed local input summary");
        let game = Some(device_game_observation(
            self.id(),
            settings.routing.game_mode_preference,
            &status,
        ));
        let observation = InputObservation(
            mode(status.mouse_mode),
            mode(status.keyboard_mode),
            faults,
            options,
            game,
        );
        if previous.as_ref() == Some(&observation) {
            return;
        }
        match group.publish_local_input(
            observation.0,
            observation.1,
            observation.2.clone(),
            observation.3.clone(),
            observation.4.clone(),
        ) {
            Ok(_) => *previous = Some(observation),
            Err(error) => tracing::warn!(%error,"input owner observation publication failed"),
        }
    }
    pub(crate) fn publish_local_permissions(&self) {
        // Permission state changes native resource observations, never the
        // immutable logical capability agreement admitted by C0.
        self.publish_local_options();
    }
    pub async fn device_rename(&self, name: String) -> Result<(), String> {
        let node = self.clone();
        tokio::task::spawn_blocking(move || crate::api::AgentApi::rename_device(&node, node.id().into(), name))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }
    pub async fn group_leave(&self) -> Result<(), String> {
        self.link_config()
            .map_err(|e| e.to_string())?
            .leave_group()
            .map_err(|e| e.to_string())
    }
    pub async fn group_remove(&self, peer: DeviceId) -> Result<(), String> {
        self.link_config()
            .map_err(|e| e.to_string())?
            .remove_member(peer)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod game_observation_tests {
    use super::*;
    use crate::threads::input::InputStatus;
    use oink_routing::{GameMode, GameModeStatus};

    #[test]
    fn owner_observation_does_not_copy_the_remote_holders_game_state() {
        let local = DeviceId([1; 32]);
        let mut status = InputStatus {
            game_mode: true,
            game_status: Some(GameModeStatus {
                holder: DeviceId([2; 32]),
                epoch: 1,
                mode: GameMode::On,
                active: true,
                detected: true,
                cursor_hiding_cause_available: None,
            }),
            ..Default::default()
        };
        for (preference, expected) in [(GameMode::On, true), (GameMode::Auto, false), (GameMode::Off, false)] {
            let game = device_game_observation(local, preference, &status);
            assert_eq!(game.active().unwrap(), expected);
            assert!(!game.detected().unwrap());
        }
        status.game_status.as_mut().unwrap().active = false;
        assert!(device_game_observation(local, GameMode::On, &status).active().unwrap());
        status.paused = true;
        assert!(!device_game_observation(local, GameMode::On, &status).active().unwrap());
    }

    #[test]
    fn owner_auto_reports_current_capture_and_clears_without_environment_state() {
        let local = DeviceId([1; 32]);
        let mut status = InputStatus {
            game_status: Some(GameModeStatus {
                holder: local,
                epoch: 1,
                mode: GameMode::Auto,
                active: true,
                detected: true,
                cursor_hiding_cause_available: None,
            }),
            ..Default::default()
        };
        let game = device_game_observation(local, GameMode::Auto, &status);
        assert!(game.active().unwrap() && game.detected().unwrap());
        let current = status.game_status.as_mut().unwrap();
        current.active = false;
        current.detected = false;
        let game = device_game_observation(local, GameMode::Auto, &status);
        assert!(!game.active().unwrap() && !game.detected().unwrap());
        // An epoch-local hotkey override remains visible without changing the preference.
        status.game_status.as_mut().unwrap().active = true;
        assert!(
            device_game_observation(local, GameMode::Auto, &status)
                .active()
                .unwrap()
        );
    }
}
