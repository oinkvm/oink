//! Native host projection and submission for authenticated desktop owner edits.

use super::*;
use oink_audio::route::{Mode, Role};
use oink_foundation::DeviceId;
use oink_network::wire_identity::{read_boot, read_device};
use oink_protocol::v2::{
    Message, RawValue, dto,
    schema::{FixedBytes, Text, WireDto, WireValue},
};

type OptionId = dto::LocalOption;
type Failure = dto::ControlFailure;

#[derive(Clone, Copy)]
enum Edit {
    Flag(OptionId, bool),
    Choice(OptionId, u32),
}

impl Edit {
    fn option(self) -> OptionId {
        match self {
            Self::Flag(option, _) | Self::Choice(option, _) => option,
        }
    }
    fn value(self) -> RawValue {
        match self {
            Self::Flag(_, value) => value.write(),
            Self::Choice(_, value) => value.write(),
        }
    }
    fn token_suffix(self) -> String {
        let value = match self {
            Self::Flag(_, value) => u32::from(value),
            Self::Choice(_, value) => value,
        };
        format!("set/{}/{value}", self.option().number())
    }
}

const SCREEN_SHARED: Edit = Edit::Flag(OptionId::ScreenShared, true);
const SCREEN_LOCAL: Edit = Edit::Flag(OptionId::ScreenShared, false);
const MOUSE_SHARED: Edit = Edit::Choice(OptionId::MouseMode, 0);
const MOUSE_LOCAL: Edit = Edit::Choice(OptionId::MouseMode, 1);
const MOUSE_DISABLED: Edit = Edit::Choice(OptionId::MouseMode, 2);
const KEYBOARD_SHARED: Edit = Edit::Choice(OptionId::KeyboardMode, 0);
const KEYBOARD_LOCAL: Edit = Edit::Choice(OptionId::KeyboardMode, 1);
const KEYBOARD_DISABLED: Edit = Edit::Flag(OptionId::KeyboardDisabled, true);
const KEYBOARD_ENABLED: Edit = Edit::Flag(OptionId::KeyboardDisabled, false);
const CLIPBOARD_SHARED: Edit = Edit::Flag(OptionId::ClipboardEnabled, true);
const CLIPBOARD_LOCAL: Edit = Edit::Flag(OptionId::ClipboardEnabled, false);
const GAME_AUTO: Edit = Edit::Choice(OptionId::GameModePreference, 0);
const GAME_ON: Edit = Edit::Choice(OptionId::GameModePreference, 1);
const GAME_OFF: Edit = Edit::Choice(OptionId::GameModePreference, 2);

const EDITS: &[Edit] = &[
    SCREEN_SHARED,
    SCREEN_LOCAL,
    MOUSE_SHARED,
    MOUSE_LOCAL,
    MOUSE_DISABLED,
    KEYBOARD_SHARED,
    KEYBOARD_LOCAL,
    KEYBOARD_DISABLED,
    KEYBOARD_ENABLED,
    CLIPBOARD_SHARED,
    CLIPBOARD_LOCAL,
    GAME_AUTO,
    GAME_ON,
    GAME_OFF,
];

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != 2 * N {
        return None;
    }
    let mut bytes = [0; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

fn request_id(value: &str) -> Option<dto::Id16> {
    if value.len() != 36 {
        return None;
    }
    let [a, b, c, d, e] = value.split('-').collect::<Vec<_>>().try_into().ok()?;
    if [a.len(), b.len(), c.len(), d.len(), e.len()] != [8, 4, 4, 4, 12] {
        return None;
    }
    decode_hex(&[a, b, c, d, e].concat()).map(FixedBytes)
}

fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pref(facts: &dto::DeviceFacts, option: OptionId) -> Option<dto::DevicePreference> {
    facts
        .preferences()
        .ok()??
        .0
        .into_iter()
        .find(|pref| pref.option() == Ok(option))
}

fn revision(facts: &dto::DeviceFacts, option: OptionId) -> Option<u64> {
    pref(facts, option).and_then(|pref| pref.revision().ok()).or_else(|| {
        facts
            .resources()
            .ok()??
            .option_revisions()
            .ok()?
            .0
            .into_iter()
            .find(|entry| entry.option() == Ok(option))?
            .revision()
            .ok()
    })
}

fn enabled(facts: &dto::DeviceFacts, option: OptionId) -> Option<bool> {
    bool::read(&pref(facts, option)?.value().ok()?).ok()
}

fn choice(facts: &dto::DeviceFacts, option: OptionId) -> Option<u32> {
    u32::read(&pref(facts, option)?.value().ok()?).ok()
}

fn sharing(mode: dto::SharingMode) -> Option<ResourceMode> {
    match mode {
        dto::SharingMode::Shared => Some(ResourceMode::Shared),
        dto::SharingMode::Local => Some(ResourceMode::Local),
        dto::SharingMode::Unknown(_) => None,
    }
}

/// Mouse: the owner preference carries Disabled (value 2), which the v1-shaped
/// resource observation cannot express; otherwise prefer the observation.
fn mouse_mode(facts: &dto::DeviceFacts, resources: Option<&dto::ResourceFacts>) -> Option<ResourceMode> {
    let preference = choice(facts, OptionId::MouseMode);
    if preference == Some(2) {
        return Some(ResourceMode::Disabled);
    }
    resources
        .and_then(|value| value.mouse_mode().ok().flatten())
        .and_then(sharing)
        .or(match preference {
            Some(0) => Some(ResourceMode::Shared),
            Some(1) => Some(ResourceMode::Local),
            _ => None,
        })
}

/// Keyboard: Disabled is the separate KeyboardDisabled flag (also summarised
/// in the published local options); the resource mode stays Shared/Local.
fn keyboard_mode(facts: &dto::DeviceFacts, resources: Option<&dto::ResourceFacts>) -> Option<ResourceMode> {
    let disabled =
        enabled(facts, OptionId::KeyboardDisabled).or_else(|| facts.local_options().ok()?.keyboard_disabled().ok());
    if disabled == Some(true) {
        return Some(ResourceMode::Disabled);
    }
    resources
        .and_then(|value| value.keyboard_mode().ok().flatten())
        .and_then(sharing)
        .or(match choice(facts, OptionId::KeyboardMode) {
            Some(0) => Some(ResourceMode::Shared),
            Some(1) => Some(ResourceMode::Local),
            _ => None,
        })
}

/// The pointer lock now belongs to the phone's own focus session (protocol
/// §14.2): the token still binds the owner's Boot; the revision is fixed.
const FOCUS_REVISION: u64 = 1;

fn focus_token(facts: &dto::DeviceFacts, locked: bool) -> Option<String> {
    token(facts, &format!("focus/{locked}"), FOCUS_REVISION)
}

/// A speaker / microphone mode chosen on this phone for a device (exit,
/// forward, local; sender, receive, local). It never leaves the process: the
/// phone authors the global audio configuration itself (`set_audio_mode`,
/// setter decides). The register stamp it was projected from only keeps the
/// token fresh.
fn audio_token(role: Role, device: DeviceId, mode: Mode, stamp: u64) -> String {
    let role = match role {
        Role::Speaker => "speaker",
        Role::Microphone => "microphone",
    };
    let mode = match mode {
        Mode::Holder => "holder",
        Mode::Shared => "shared",
        Mode::Local => "local",
    };
    format!("audio/{role}/{}/{mode}/{stamp}", device.to_hex())
}

fn read_audio_token(token: &str) -> Option<(Role, DeviceId, Mode)> {
    let [kind, role, device, mode, _stamp] = token.split('/').collect::<Vec<_>>().try_into().ok()?;
    if kind != "audio" {
        return None;
    }
    let role = match role {
        "speaker" => Role::Speaker,
        "microphone" => Role::Microphone,
        _ => return None,
    };
    let mode = match mode {
        "holder" => Mode::Holder,
        "shared" => Mode::Shared,
        "local" => Mode::Local,
        _ => return None,
    };
    Some((role, DeviceId(decode_hex(device)?), mode))
}

/// Token for this phone's own microphone sender gain. It never leaves the
/// process: `set_forwarding_controls` applies it to the local audio owner.
fn local_mic_token(device: &str, revision: u64) -> String {
    format!("local/{device}/mic/{revision}")
}

fn token(facts: &dto::DeviceFacts, suffix: &str, revision: u64) -> Option<String> {
    let owner = read_boot(&facts.boot().ok()?).ok()?;
    Some(format!(
        "{}/{}/{}/{revision}/{suffix}",
        owner.device_id.to_hex(),
        hex(&facts.group().ok()?.0),
        hex(&owner.nonce),
    ))
}

fn option_token(facts: &dto::DeviceFacts, edit: Edit) -> Option<String> {
    token(facts, &edit.token_suffix(), revision(facts, edit.option())?)
}

fn gain_token(facts: &dto::DeviceFacts, option: OptionId) -> Option<String> {
    token(facts, &format!("gain/{}", option.number()), revision(facts, option)?)
}

fn pause_token(facts: &dto::DeviceFacts, state: &dto::LocalPauseState) -> Option<String> {
    token(
        facts,
        &format!("pause/{}", !state.paused().ok()?),
        state.revision().ok()?,
    )
}

fn unavailable(reason: CardActionUnavailable) -> CardAction {
    CardAction::Unavailable { reason }
}

fn option_message(id: dto::Id16, facts: &dto::DeviceFacts, option: OptionId, value: RawValue) -> Option<Message> {
    dto::SetLocalOption::from_raw(
        [
            (1, id.write()),
            (2, facts.boot().ok()?.write()),
            (3, option.write()),
            (4, value),
            (5, revision(facts, option)?.write()),
        ]
        .into(),
    )
    .ok()
    .map(Message::ConfigSetLocalOption)
}

fn pause_message(id: dto::Id16, facts: &dto::DeviceFacts, pause: &dto::LocalPauseState) -> Option<Message> {
    dto::PauseDevice::from_raw(
        [
            (1, id.write()),
            (2, facts.boot().ok()?.write()),
            (3, (!pause.paused().ok()?).write()),
            (4, pause.revision().ok()?.write()),
        ]
        .into(),
    )
    .ok()
    .map(Message::ConfigPauseDevice)
}

fn refusal(request: String, failure: Failure) -> DeviceControlReply {
    DeviceControlReply::Completed(DeviceControlCompleted {
        request_id: request,
        result: if matches!(
            failure,
            Failure::OutcomeUnknown | Failure::ApplyFailed | Failure::PersistenceFailed
        ) {
            DeviceControlResult::Failed {
                reason: control_failure(failure),
            }
        } else {
            DeviceControlResult::Refused {
                reason: control_failure(failure),
            }
        },
    })
}

fn control_failure(reason: Failure) -> DeviceControlFailure {
    match reason {
        Failure::Offline => DeviceControlFailure::Offline,
        Failure::Unsupported => DeviceControlFailure::Unsupported,
        Failure::StaleBoot => DeviceControlFailure::StaleBoot,
        Failure::StaleRevision => DeviceControlFailure::StaleRevision,
        Failure::NotMember => DeviceControlFailure::NotMember,
        Failure::ResourceFault => DeviceControlFailure::ResourceFault,
        Failure::InvalidValue => DeviceControlFailure::InvalidValue,
        Failure::Busy => DeviceControlFailure::Busy,
        Failure::PersistenceFailed => DeviceControlFailure::PersistenceFailed,
        Failure::ApplyFailed => DeviceControlFailure::ApplyFailed,
        Failure::LinkLost => DeviceControlFailure::LinkLost,
        Failure::OutcomeUnknown => DeviceControlFailure::OutcomeUnknown,
        Failure::NotExit => DeviceControlFailure::NotExit,
        Failure::RouteChanged => DeviceControlFailure::RouteChanged,
        Failure::OutputChanged => DeviceControlFailure::OutputChanged,
        Failure::OutputUnavailable => DeviceControlFailure::OutputUnavailable,
        Failure::VolumeUnsupported => DeviceControlFailure::VolumeUnsupported,
        Failure::PermissionDenied => DeviceControlFailure::PermissionDenied,
        Failure::Superseded => DeviceControlFailure::Superseded,
        Failure::UnsupportedOption => DeviceControlFailure::UnsupportedOption,
        Failure::StateConflict => DeviceControlFailure::StateConflict,
        _ => DeviceControlFailure::Unrecognized,
    }
}

impl MobileSession {
    async fn submit_control(&self, request: String, message: Message) -> DeviceControlReply {
        match self.network.device_control(message).await {
            Ok(completed) => DeviceControlReply::Completed(DeviceControlCompleted {
                request_id: request,
                result: match completed.outcome {
                    dto::ControlOutcome::Applied => DeviceControlResult::Applied,
                    dto::ControlOutcome::NoChange => DeviceControlResult::NoChange,
                    dto::ControlOutcome::Refused => DeviceControlResult::Refused {
                        reason: control_failure(completed.reason),
                    },
                    _ => DeviceControlResult::Failed {
                        reason: control_failure(completed.reason),
                    },
                },
            }),
            Err(error) => refusal(request, error),
        }
    }

    fn token_facts(&self, token: &str) -> Result<(DeviceId, dto::DeviceFacts), Failure> {
        let owner = decode_hex(token.split('/').next().ok_or(Failure::InvalidValue)?)
            .map(DeviceId)
            .ok_or(Failure::InvalidValue)?;
        let facts = self.network.peer_facts(owner).ok_or(Failure::Offline)?;
        if facts.device_kind() != Ok(dto::DeviceKind::Desktop) {
            return Err(Failure::Unsupported);
        }
        Ok((owner, facts))
    }

    pub(super) async fn invoke_action(&self, request: String, supplied: String) -> DeviceControlReply {
        let Some(id) = request_id(&request) else {
            return refusal(request, Failure::InvalidValue);
        };
        if let Some((role, device, mode)) = read_audio_token(&supplied) {
            return match self.media.set_audio_mode(device, role, mode) {
                Ok(outcome) => DeviceControlReply::Completed(DeviceControlCompleted {
                    request_id: request,
                    result: if outcome == dto::ControlOutcome::Applied {
                        DeviceControlResult::Applied
                    } else {
                        DeviceControlResult::NoChange
                    },
                }),
                Err(reason) => refusal(request, reason),
            };
        }
        let (_, facts) = match self.token_facts(&supplied) {
            Ok(value) => value,
            Err(error) => return refusal(request, error),
        };
        for locked in [true, false] {
            if focus_token(&facts, locked).as_deref() == Some(supplied.as_str()) {
                return self.focus_lock(request, &facts, locked);
            }
        }
        let message = EDITS
            .iter()
            .find_map(|&edit| {
                (option_token(&facts, edit).as_deref() == Some(supplied.as_str()))
                    .then(|| option_message(id.clone(), &facts, edit.option(), edit.value()))
                    .flatten()
            })
            .or_else(|| {
                let pause = facts.pause().ok()??;
                (pause_token(&facts, &pause).as_deref() == Some(supplied.as_str()))
                    .then(|| pause_message(id, &facts, &pause))
                    .flatten()
            });
        match message {
            Some(message) => self.submit_control(request, message).await,
            None => refusal(request, Failure::StaleRevision),
        }
    }

    /// The device sheet's pointer lock: lock the group pointer to this desktop
    /// or release the lock, through the phone's own focus session (I0
    /// `SetFocusLock`, protocol §14.2). The holder's `FocusState` confirms it.
    fn focus_lock(&self, request: String, facts: &dto::DeviceFacts, locked: bool) -> DeviceControlReply {
        let Ok(owner) = facts.boot().and_then(|boot| read_boot(&boot)) else {
            return refusal(request, Failure::InvalidValue);
        };
        let result = if locked {
            self.input.lock_to(owner.device_id)
        } else {
            self.input.set_focus_lock(false)
        };
        match result {
            Ok(()) => DeviceControlReply::Completed(DeviceControlCompleted {
                request_id: request,
                result: DeviceControlResult::Applied,
            }),
            Err(oink_mobile_core::ControlError::Paused) => refusal(request, Failure::Busy),
            Err(_) => refusal(request, Failure::Offline),
        }
    }

    pub(super) async fn edit_gain(
        &self,
        request: String,
        supplied: String,
        volume: u16,
        muted: bool,
    ) -> DeviceControlReply {
        let Some(id) = request_id(&request).filter(|_| volume <= 200) else {
            return refusal(request, Failure::InvalidValue);
        };
        if supplied.starts_with("local/") {
            return self.edit_local_mic_gain(request, &supplied, volume, muted);
        }
        let (_, facts) = match self.token_facts(&supplied) {
            Ok(value) => value,
            Err(error) => return refusal(request, error),
        };
        let Some(option) = [OptionId::ForwardingControls, OptionId::MicrophoneControls]
            .into_iter()
            .find(|&option| gain_token(&facts, option).as_deref() == Some(supplied.as_str()))
        else {
            return refusal(request, Failure::StaleRevision);
        };
        let value = RawValue::Map([(1, volume.write()), (2, muted.write())].into());
        match option_message(id, &facts, option, value) {
            Some(message) => self.submit_control(request, message).await,
            None => refusal(request, Failure::InvalidValue),
        }
    }

    pub(super) fn focus_view(&self) -> Vec<FocusSummary> {
        self.network
            .links()
            .links
            .iter()
            .filter_map(|link| {
                let facts = self.network.peer_facts(link.peer)?;
                if facts.device_kind() != Ok(dto::DeviceKind::Desktop) {
                    return None;
                }
                let locked_device = self.input.locked_device();
                Some(FocusSummary {
                    owner: link.peer.to_hex(),
                    revision: facts.fact_sequence().ok()?,
                    epoch: 0,
                    holder: None,
                    locked_device: locked_device.map(|device| device.to_hex()),
                    valid: self
                        .network
                        .peer_permits(link.peer, oink_protocol::v2::registry::type_id::ConfigSetLocalOption),
                    version_token: focus_token(&facts, locked_device != Some(link.peer))?,
                })
            })
            .collect()
    }

    pub(super) async fn edit_focus_lock(
        &self,
        request: String,
        owner: &str,
        supplied: String,
        locked: bool,
        target: Option<&str>,
    ) -> DeviceControlReply {
        if target.is_some_and(|target| target != owner) {
            return refusal(request, Failure::InvalidValue);
        }
        let facts = match self.token_facts(&supplied) {
            Ok((device, facts)) if device.to_hex() == owner => facts,
            Ok(_) => return refusal(request, Failure::InvalidValue),
            Err(error) => return refusal(request, error),
        };
        if focus_token(&facts, locked).as_deref() != Some(supplied.as_str()) {
            return refusal(request, Failure::StaleRevision);
        }
        self.invoke_action(request, supplied).await
    }

    fn edit_local_mic_gain(&self, request: String, supplied: &str, volume: u16, muted: bool) -> DeviceControlReply {
        let current = self.media.microphone_controls();
        if supplied != local_mic_token(&self.id, current.revision) {
            return refusal(request, Failure::StaleRevision);
        }
        match self.media.set_microphone_controls(volume, muted) {
            Ok(updated) => DeviceControlReply::Completed(DeviceControlCompleted {
                request_id: request,
                result: if updated.revision == current.revision {
                    DeviceControlResult::NoChange
                } else {
                    DeviceControlResult::Applied
                },
            }),
            Err(_) => refusal(request, Failure::PersistenceFailed),
        }
    }

    pub(super) async fn edit_exit_volume(
        &self,
        request: String,
        supplied: String,
        volume: u8,
        muted: bool,
    ) -> DeviceControlReply {
        let Some(id) = request_id(&request).filter(|_| volume <= 100) else {
            return refusal(request, Failure::InvalidValue);
        };
        let Some((card, version)) = self.exit_volume_card() else {
            return refusal(request, Failure::NotExit);
        };
        if card.version_token.as_deref() != Some(supplied.as_str()) {
            return refusal(request, Failure::StaleRevision);
        }
        let Ok(request_wire) = dto::SetExitVolume::from_raw(
            [
                (1, id.write()),
                (2, version.write()),
                (3, volume.write()),
                (4, muted.write()),
            ]
            .into(),
        ) else {
            return refusal(request, Failure::InvalidValue);
        };
        self.submit_control(request, Message::ConfigSetExitVolume(request_wire))
            .await
    }

    pub(super) async fn edit_owner_setting(
        &self,
        owner: &str,
        option: OptionId,
        value: RawValue,
    ) -> Result<(), MobileError> {
        let peer = decode_hex(owner)
            .map(DeviceId)
            .ok_or_else(|| command_error(UserFacingCode::InvalidValue, "invalid device ID"))?;
        let facts = self.desktop_facts(peer)?;
        let message = option_message(FixedBytes(rand::random()), &facts, option, value).ok_or_else(|| {
            command_error(
                UserFacingCode::Unsupported,
                "device owner does not advertise this setting",
            )
        })?;
        match self.network.device_control(message).await {
            Ok(completed)
                if matches!(
                    completed.outcome,
                    dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange
                ) =>
            {
                Ok(())
            }
            Ok(completed) => Err(owner_error(completed.reason)),
            Err(reason) => Err(owner_error(reason)),
        }
    }

    pub(super) async fn edit_owner_name(&self, owner: &str, name: String) -> Result<(), MobileError> {
        if name.trim().is_empty() || name.len() > 256 {
            return Err(command_error(UserFacingCode::InvalidValue, "invalid device name"));
        }
        self.edit_owner_setting(owner, OptionId::DeviceName, Text::<256>(name).write())
            .await
    }

    pub(super) async fn edit_owner_flag(
        &self,
        owner: &str,
        option: OptionId,
        enabled: bool,
    ) -> Result<(), MobileError> {
        self.edit_owner_setting(owner, option, enabled.write()).await
    }

    pub(super) fn remove_group_member(&self, device: &str) -> Result<(), MobileError> {
        let peer = decode_hex(device)
            .map(DeviceId)
            .ok_or_else(|| command_error(UserFacingCode::InvalidValue, "invalid device ID"))?;
        self.network.remove_member(peer).map_err(membership_error)
    }

    fn desktop_facts(&self, peer: DeviceId) -> Result<dto::DeviceFacts, MobileError> {
        let facts = self
            .network
            .peer_facts(peer)
            .ok_or_else(|| command_error(UserFacingCode::LinkLost, "device owner is offline"))?;
        if facts.device_kind() != Ok(dto::DeviceKind::Desktop) {
            return Err(unsupported("this setting requires a desktop owner"));
        }
        Ok(facts)
    }

    pub(super) fn owner_settings(&self, owner: &str) -> Result<DeviceSettingsView, MobileError> {
        let peer = decode_hex(owner)
            .map(DeviceId)
            .ok_or_else(|| command_error(UserFacingCode::InvalidValue, "invalid device ID"))?;
        let facts = self.desktop_facts(peer)?;
        let preferences = pref(&facts, OptionId::ScreenPreferences)
            .and_then(|entry| dto::ScreenPreferencesValue::read(&entry.value().ok()?).ok())
            .and_then(|value| value.screens().ok())
            .map_or_else(Vec::new, |screens| screens.0);
        let displays = facts.displays().map_err(|_| owner_error(Failure::InvalidValue))?.0;
        let screens = displays
            .into_iter()
            .filter_map(|display| {
                let key = display.key().ok()?;
                let settings = preferences.iter().find(|value| value.display().as_ref() == Ok(&key));
                Some(ManagedScreen {
                    screen: ScreenKey {
                        device: owner.into(),
                        display: key.0,
                    },
                    name: display.name().ok()?.0,
                    gain: settings.and_then(|screen| screen.gain().ok()).unwrap_or(1.0),
                    remote_gain: settings.and_then(|screen| screen.remote_gain().ok()).unwrap_or(1.0),
                    disabled: settings.and_then(|screen| screen.enabled().ok()) == Some(false),
                })
            })
            .collect();
        Ok(DeviceSettingsView {
            mac_scroll_feel: enabled(&facts, OptionId::MacScrollFeel),
            native_multifinger: enabled(&facts, OptionId::NativeMultifinger),
            touchpad_injection: enabled(&facts, OptionId::TouchpadInjection),
            device: owner.into(),
            input_transform: None,
            screens,
        })
    }

    pub(super) async fn edit_screen_gain(&self, screen: ScreenKey, field: u32, gain: f64) -> Result<(), MobileError> {
        if !(0.1..=5.0).contains(&gain)
            || !self
                .owner_settings(&screen.device)?
                .screens
                .iter()
                .any(|value| value.screen.display == screen.display)
        {
            return Err(command_error(UserFacingCode::InvalidValue, "invalid screen or gain"));
        }
        let item = RawValue::Map([(1, Text::<512>(screen.display).write()), (field, gain.write())].into());
        let list_patch = RawValue::Map([(1, RawValue::Array(vec![item]))].into());
        self.edit_owner_setting(
            &screen.device,
            OptionId::ScreenPreferences,
            RawValue::Map([(1, list_patch)].into()),
        )
        .await
    }

    pub(super) fn exit_volume_card(&self) -> Option<(ExitVolumeCard, dto::ExitVolumeVersion)> {
        let (route, exit) = self
            .media
            .with_audio(|audio| Some((audio.route(Role::Speaker)?, audio.holder(Role::Speaker)?)))?;
        let owner = self.network.peer_facts(exit)?;
        let volume = owner.exit_volume().ok()??;
        let version = volume.version().ok()?;
        // Another phone can be the exit too: its system volume (Android) or
        // its playout gain (iPhone), Ready in its facts like a PC's.
        if volume.status() != Ok(dto::ExitVolumeStatus::Ready)
            || owner.boot().ok()? != version.exit().ok()?
            || route.version.to_wire() != version.route_version().ok()?
            || !self
                .network
                .peer_permits(exit, oink_protocol::v2::registry::type_id::ConfigSetExitVolume)
        {
            return None;
        }
        let suffix = format!(
            "exit/{}/{}/{}",
            route.version.stamp,
            version.output_generation().ok()?,
            hex(version.output_id().ok()??.0.as_bytes()),
        );
        Some((
            ExitVolumeCard {
                receiver_device: exit.to_hex(),
                receiver_name: owner.name().ok()?.0,
                volume_percent: volume.volume_percent().ok()?,
                muted: volume.muted().ok()?,
                version_token: token(&owner, &suffix, version.revision().ok()?),
                can_set: true,
            },
            version,
        ))
    }

    pub(super) fn desktop_control_cards(&self, peer: DeviceId) -> (Option<DeviceCardStatus>, Option<LocalPauseCard>) {
        let Some(facts) = self.network.peer_facts(peer) else {
            return (None, None);
        };
        if facts.device_kind() != Ok(dto::DeviceKind::Desktop) {
            return (None, None);
        }
        let supported = self
            .network
            .peer_permits(peer, oink_protocol::v2::registry::type_id::ConfigSetLocalOption);
        let action = |edit, command| match option_token(&facts, edit).filter(|_| supported) {
            Some(token) => CardAction::Invoke { command, token },
            None => unavailable(if supported {
                CardActionUnavailable::StateUnknown
            } else {
                CardActionUnavailable::Unsupported
            }),
        };
        let set = |edit| action(edit, CardCommand::SetLocalOption);
        let mode = |option, shared, local| match enabled(&facts, option) {
            Some(true) => set(local),
            Some(false) => set(shared),
            None => unavailable(CardActionUnavailable::StateUnknown),
        };
        let pause = facts.pause().ok().flatten().and_then(|state| {
            Some(LocalPauseCard {
                paused: state.paused().ok()?,
                action: pause_token(&facts, &state).filter(|_| supported).map_or_else(
                    || unavailable(CardActionUnavailable::Unsupported),
                    |token| CardAction::Invoke {
                        command: CardCommand::SetDevicePaused,
                        token,
                    },
                ),
            })
        });
        let condition = if pause.as_ref().is_some_and(|state| state.paused) {
            CardCondition::LocalPaused
        } else {
            CardCondition::Ready
        };
        let resource = |action, mode| ResourceCard {
            action,
            condition,
            mode,
        };
        let resources = facts.resources().ok().flatten();
        let flag_mode = |option| {
            enabled(&facts, option).map(|shared| {
                if shared {
                    ResourceMode::Shared
                } else {
                    ResourceMode::Local
                }
            })
        };
        let mouse = mouse_mode(&facts, resources.as_ref());
        let keyboard = keyboard_mode(&facts, resources.as_ref());
        let game = resources.as_ref().and_then(|value| value.game().ok().flatten());
        let locked_to = self.input.locked_device();
        let speaker = resources.as_ref().and_then(|value| value.speaker().ok().flatten());
        let microphone = resources.as_ref().and_then(|value| value.microphone().ok().flatten());
        let speaker_activity = speaker.as_ref().and_then(|value| value.activity().ok().flatten());
        let mic_receiving = microphone
            .as_ref()
            .and_then(|value| value.receiving_from().ok().flatten())
            .and_then(|source| read_device(&source).ok());
        // Modes come from the global audio configuration this phone holds.
        let (speaker_mode, microphone_mode, forward_on, receive_on, stamps) = self.media.with_audio(|audio| {
            (
                audio.mode(peer, Role::Speaker),
                audio.mode(peer, Role::Microphone),
                audio.switch(peer, Role::Speaker),
                audio.switch(peer, Role::Microphone),
                [Role::Speaker, Role::Microphone].map(|role| audio.route(role).map_or(0, |r| r.version.stamp)),
            )
        });
        let mic_sending = microphone_mode == Some(Mode::Holder);
        let audio_action = |role: Role, mode: Mode, command| CardAction::Invoke {
            command,
            token: audio_token(role, peer, mode, stamps[role.index()]),
        };
        let local_device = self.network.boot().device_id;
        // This phone as the sender: its gain is owned by the local audio owner.
        let local_mic = (mic_receiving == Some(local_device)).then(|| self.media.microphone_controls());
        let mic_gain_owner = if let Some(source) = mic_receiving {
            self.network.peer_facts(source)
        } else {
            mic_sending.then(|| facts.clone())
        };
        let gain = |facts: &dto::DeviceFacts, option| {
            let value = dto::ForwardingControlsValue::read(&pref(facts, option)?.value().ok()?).ok()?;
            Some(ForwardingCard {
                volume_percent: value.volume_percent().ok()?,
                muted: value.muted().ok()?,
            })
        };
        let audio_capable = self
            .network
            .peer_permits(peer, oink_protocol::v2::registry::type_id::ConfigSetExitVolume);
        let mic_capable = self
            .network
            .peer_permits(peer, oink_protocol::v2::registry::type_id::MicrophoneDemand);
        let mic_action = |mode| {
            if mic_capable {
                audio_action(Role::Microphone, mode, CardCommand::SetLocalOption)
            } else {
                unavailable(CardActionUnavailable::Unsupported)
            }
        };
        let speaker_action = |mode| {
            if audio_capable {
                audio_action(Role::Speaker, mode, CardCommand::SetLocalOption)
            } else {
                unavailable(CardActionUnavailable::Unsupported)
            }
        };
        let is_exit = speaker_mode == Some(Mode::Holder);
        let status = DeviceCardStatus {
            actions: DeviceCardActions {
                screen_shared: action(SCREEN_SHARED, CardCommand::SetScreenMode),
                screen_local: action(SCREEN_LOCAL, CardCommand::SetScreenMode),
                mouse_shared: set(MOUSE_SHARED),
                mouse_local: set(MOUSE_LOCAL),
                mouse_disabled: set(MOUSE_DISABLED),
                keyboard_shared: set(KEYBOARD_SHARED),
                keyboard_local: set(KEYBOARD_LOCAL),
                keyboard_disabled: set(KEYBOARD_DISABLED),
                keyboard_enabled: set(KEYBOARD_ENABLED),
                clipboard_shared: set(CLIPBOARD_SHARED),
                clipboard_local: set(CLIPBOARD_LOCAL),
                forwarding_enabled: speaker_action(Mode::Shared),
                forwarding_disabled: speaker_action(Mode::Local),
            },
            screen: resource(
                mode(OptionId::ScreenShared, SCREEN_SHARED, SCREEN_LOCAL),
                flag_mode(OptionId::ScreenShared),
            ),
            mouse: resource(
                match mouse {
                    Some(ResourceMode::Shared) => set(MOUSE_LOCAL),
                    Some(ResourceMode::Local | ResourceMode::Disabled) => set(MOUSE_SHARED),
                    None => unavailable(CardActionUnavailable::StateUnknown),
                },
                mouse,
            ),
            keyboard: KeyboardCard {
                resource: resource(
                    match keyboard {
                        Some(ResourceMode::Shared) => set(KEYBOARD_LOCAL),
                        Some(ResourceMode::Local) => set(KEYBOARD_SHARED),
                        Some(ResourceMode::Disabled) => set(KEYBOARD_ENABLED),
                        None => unavailable(CardActionUnavailable::StateUnknown),
                    },
                    keyboard,
                ),
            },
            speaker: SpeakerCard {
                is_exit,
                make_exit_action: if !audio_capable {
                    unavailable(CardActionUnavailable::Unsupported)
                } else if is_exit {
                    audio_action(Role::Speaker, Mode::Local, CardCommand::ClearAudioExit)
                } else {
                    audio_action(Role::Speaker, Mode::Holder, CardCommand::MakeAudioExit)
                },
                forwarding_controls_token: (supported && audio_capable)
                    .then(|| gain_token(&facts, OptionId::ForwardingControls))
                    .flatten(),
                forwarding: gain(&facts, OptionId::ForwardingControls),
                forwarding_enabled: forward_on,
                activity: match speaker_activity {
                    Some(dto::SpeakerActivity::ExitPlaying) => SpeakerActivity::Playing,
                    Some(dto::SpeakerActivity::SourceForwarding) => SpeakerActivity::Forwarding,
                    Some(dto::SpeakerActivity::SourceSoundSleep) => SpeakerActivity::SoundSleep,
                    Some(dto::SpeakerActivity::Preparing) => SpeakerActivity::Preparing,
                    Some(dto::SpeakerActivity::WaitingForExit) => SpeakerActivity::WaitingForExit,
                    Some(dto::SpeakerActivity::LocalPaused) => SpeakerActivity::Paused,
                    Some(dto::SpeakerActivity::SystemSleeping) => SpeakerActivity::Sleeping,
                    Some(dto::SpeakerActivity::Failed) => SpeakerActivity::Failed,
                    Some(dto::SpeakerActivity::Unused | dto::SpeakerActivity::ExitReady) => SpeakerActivity::Idle,
                    _ => SpeakerActivity::Unknown,
                },
            },
            microphone: MicrophoneCard {
                mode: if !mic_capable {
                    MicrophoneMode::Unavailable
                } else if mic_sending {
                    MicrophoneMode::Source
                } else if receive_on == Some(false) {
                    MicrophoneMode::Muted
                } else if mic_receiving.is_some() {
                    MicrophoneMode::Receiver
                } else {
                    MicrophoneMode::Idle
                },
                share_action: mic_action(Mode::Holder),
                local_action: mic_action(Mode::Local),
                receive_action: mic_action(Mode::Shared),
                controls_token: if let Some(local) = &local_mic {
                    Some(local_mic_token(&self.id, local.revision))
                } else {
                    mic_gain_owner
                        .as_ref()
                        .filter(|owner| {
                            owner.device_kind() == Ok(dto::DeviceKind::Desktop)
                                && owner
                                    .boot()
                                    .ok()
                                    .and_then(|boot| read_boot(&boot).ok())
                                    .is_some_and(|boot| {
                                        self.network.peer_permits(
                                            boot.device_id,
                                            oink_protocol::v2::registry::type_id::ConfigSetLocalOption,
                                        )
                                    })
                        })
                        .and_then(|owner| gain_token(owner, OptionId::MicrophoneControls))
                },
                forwarding: if let Some(local) = &local_mic {
                    Some(ForwardingCard {
                        volume_percent: local.volume_percent,
                        muted: local.muted,
                    })
                } else {
                    mic_gain_owner
                        .as_ref()
                        .and_then(|owner| gain(owner, OptionId::MicrophoneControls))
                },
            },
            clipboard: resource(
                mode(OptionId::ClipboardEnabled, CLIPBOARD_SHARED, CLIPBOARD_LOCAL),
                flag_mode(OptionId::ClipboardEnabled),
            ),
            game: GameCard {
                actions: GameActions {
                    auto: action(GAME_AUTO, CardCommand::SetGameMode),
                    on: action(GAME_ON, CardCommand::SetGameMode),
                    off: action(GAME_OFF, CardCommand::SetGameMode),
                },
                preference: match choice(&facts, OptionId::GameModePreference) {
                    Some(0) => Some(GameModePreference::Auto),
                    Some(1) => Some(GameModePreference::On),
                    Some(2) => Some(GameModePreference::Off),
                    _ => None,
                },
                active: game.as_ref().and_then(|value| value.active().ok()),
                detected: game.as_ref().and_then(|value| value.detected().ok()),
            },
            focus: {
                let locked = locked_to == Some(peer);
                FocusCard {
                    locked,
                    locked_elsewhere: locked_to.is_some() && !locked,
                    action: match focus_token(&facts, !locked).filter(|_| supported) {
                        Some(token) => CardAction::Invoke {
                            command: CardCommand::SetFocusLock,
                            token,
                        },
                        None => unavailable(CardActionUnavailable::Unsupported),
                    },
                }
            },
        };
        (Some(status), pause)
    }

    /// Endpoint state for a phone card. This phone uses its own audio owner;
    /// another phone uses the speaker/microphone observations in its facts.
    pub(super) fn mobile_audio_card(&self, peer: DeviceId, local: bool) -> Option<MobileAudioCard> {
        if local {
            let capabilities = self.network.platform_facts();
            let media = self.media.snapshot();
            let state = |kind: crate::MobileMediaKind, advertised: bool| {
                let bit = kind.bit();
                let index = kind.index();
                if !advertised {
                    MobileEndpointState::Unavailable
                } else if media
                    .failures
                    .get(index)
                    .is_some_and(|failure| *failure != crate::MobileMediaFailure::None)
                {
                    MobileEndpointState::Failed
                } else if media.enabled_mask & bit == 0 {
                    MobileEndpointState::Local
                } else if media.running_mask & bit != 0 {
                    MobileEndpointState::Active
                } else {
                    MobileEndpointState::Starting
                }
            };
            return Some(MobileAudioCard {
                speaker: state(crate::MobileMediaKind::Speaker, capabilities.speaker_receive),
                microphone: state(crate::MobileMediaKind::Microphone, capabilities.microphone_send),
                forward: state(crate::MobileMediaKind::Forward, capabilities.speaker_send),
            });
        }
        let facts = self.network.peer_facts(peer)?;
        if facts.device_kind() != Ok(dto::DeviceKind::Mobile) {
            return None;
        }
        let supports = |capability: &str| {
            facts
                .capabilities()
                .ok()
                .and_then(|value| value.supported().ok())
                .is_some_and(|list| list.0.iter().any(|id| id.0 == capability))
        };
        let resources = facts.resources().ok().flatten();
        let speaker = resources.as_ref().and_then(|value| value.speaker().ok().flatten());
        let microphone = resources.as_ref().and_then(|value| value.microphone().ok().flatten());
        // The modes come from the global audio configuration; the phone's own
        // observations only tell Starting from Active (a sender captures only
        // while a receiver demands it) and report failures.
        let (speaker_mode, microphone_mode) = self
            .media
            .with_audio(|audio| (audio.mode(peer, Role::Speaker), audio.mode(peer, Role::Microphone)));
        let activity = speaker.and_then(|value| value.activity().ok().flatten());
        Some(MobileAudioCard {
            forward: if !supports("oink.audio/2") {
                MobileEndpointState::Unavailable
            } else if speaker_mode != Some(Mode::Shared) {
                MobileEndpointState::Local
            } else if activity == Some(dto::SpeakerActivity::SourceForwarding) {
                MobileEndpointState::Active
            } else if activity == Some(dto::SpeakerActivity::Failed) {
                MobileEndpointState::Failed
            } else {
                MobileEndpointState::Starting
            },
            speaker: if !supports("oink.audio/2") {
                MobileEndpointState::Unavailable
            } else if activity == Some(dto::SpeakerActivity::Failed) {
                MobileEndpointState::Failed
            } else if speaker_mode != Some(Mode::Holder) {
                MobileEndpointState::Local
            } else if matches!(
                activity,
                Some(dto::SpeakerActivity::ExitReady | dto::SpeakerActivity::ExitPlaying)
            ) {
                MobileEndpointState::Active
            } else {
                MobileEndpointState::Starting
            },
            microphone: if !supports("oink.audio.microphone/1") {
                MobileEndpointState::Unavailable
            } else if microphone_mode != Some(Mode::Holder) {
                MobileEndpointState::Local
            } else if microphone.is_some_and(|value| value.capture_active() == Ok(true)) {
                MobileEndpointState::Active
            } else {
                MobileEndpointState::Starting
            },
        })
    }
}

fn command_error(code: UserFacingCode, detail: impl Into<String>) -> MobileError {
    MobileError::Command {
        code,
        detail: detail.into(),
    }
}

pub(super) fn membership_error(error: oink_mobile_core::MobileNetworkError) -> MobileError {
    let code = match &error {
        oink_mobile_core::MobileNetworkError::Start(_) => UserFacingCode::PersistenceFailed,
        _ => UserFacingCode::StateConflict,
    };
    command_error(code, error.to_string())
}

pub(super) fn unsupported(detail: &'static str) -> MobileError {
    command_error(UserFacingCode::Unsupported, detail)
}

fn owner_error(reason: Failure) -> MobileError {
    let code = match reason {
        Failure::Offline | Failure::LinkLost => UserFacingCode::LinkLost,
        Failure::OutcomeUnknown => UserFacingCode::Timeout,
        Failure::Unsupported | Failure::UnsupportedOption => UserFacingCode::Unsupported,
        Failure::InvalidValue => UserFacingCode::InvalidValue,
        Failure::PermissionDenied => UserFacingCode::PermissionDenied,
        Failure::NotMember => UserFacingCode::NotMember,
        Failure::PersistenceFailed => UserFacingCode::PersistenceFailed,
        Failure::StaleBoot | Failure::StaleRevision | Failure::StateConflict => UserFacingCode::StateConflict,
        _ => UserFacingCode::BackendFailed,
    };
    command_error(code, format!("device control: {reason:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::session::Boot;
    use oink_network::wire_identity::boot;

    fn preference(option: OptionId, value: RawValue) -> RawValue {
        dto::DevicePreference::from_raw([(1, option.write()), (2, 3u64.write()), (3, value)].into())
            .unwrap()
            .write()
    }

    fn facts(owner: Boot, locked_to: Option<DeviceId>) -> dto::DeviceFacts {
        let options = dto::LocalOptionsSummary::from_raw(
            [
                (1, true.write()),
                (3, 1f32.write()),
                (4, locked_to.is_some().write()),
                (5, true.write()),
                (6, false.write()),
            ]
            .into(),
        )
        .unwrap();
        let capabilities = dto::FactCapabilities::from_raw(
            [
                (1, RawValue::Array(vec![RawValue::from("oink.device/2")])),
                (2, dto::CapturePermission::Indeterminate.write()),
                (3, RawValue::Array(Vec::new())),
                (4, RawValue::Array(Vec::new())),
            ]
            .into(),
        )
        .unwrap();
        let game = dto::GameObservation::from_raw([(1, true.write()), (2, false.write())].into()).unwrap();
        let resources = RawValue::Map(
            [
                (2, dto::SharingMode::Shared.write()),
                (3, dto::SharingMode::Local.write()),
                (7, game.write()),
            ]
            .into(),
        );
        let observed = dto::ObservedState::from_raw(
            [
                (1, true.write()),
                (2, dto::RunIntent::Running.write()),
                (4, 0u64.write()),
            ]
            .into(),
        )
        .unwrap();
        let version = RawValue::Map(
            [
                (1, oink_protocol::v2::MAJOR.write()),
                (2, oink_protocol::v2::MINOR.write()),
            ]
            .into(),
        );
        dto::DeviceFacts::from_raw(
            [
                (1, boot(owner).write()),
                (2, 1u64.write()),
                (3, version),
                (4, RawValue::from("test")),
                (5, RawValue::Bytes(vec![4; 16])),
                (6, RawValue::from("Windows PC")),
                (7, RawValue::from("windows")),
                (8, RawValue::from("")),
                (9, RawValue::from("test")),
                (10, RawValue::Array(Vec::new())),
                (11, observed.write()),
                (12, RawValue::Array(Vec::new())),
                (13, capabilities.write()),
                (14, options.write()),
                (15, RawValue::Array(Vec::new())),
                (16, resources),
                (
                    18,
                    RawValue::Array(vec![
                        preference(OptionId::MouseMode, 2u32.write()),
                        preference(OptionId::KeyboardDisabled, true.write()),
                        preference(OptionId::GameModePreference, 1u32.write()),
                    ]),
                ),
                (25, dto::DeviceKind::Desktop.write()),
                (26, oink_protocol::v2::MAJOR.write()),
                (27, oink_protocol::v2::MINOR.write()),
            ]
            .into(),
        )
        .unwrap()
    }

    fn owner() -> Boot {
        Boot {
            device_id: DeviceId([1; 32]),
            nonce: [2; 16],
        }
    }

    /// Review M17: a PC's published pause decides whether it is a proxy
    /// candidate; facts without a pause still accept content.
    #[test]
    fn proxy_candidates_follow_the_pause_in_facts() {
        let accepts = super::super::presence::accepts_content;
        let base = facts(owner(), None);
        assert!(accepts(&base));
        let with = |key: u32, value: RawValue| {
            let mut raw = base.clone().into_raw();
            raw.insert(key, value);
            dto::DeviceFacts::from_raw(raw).unwrap()
        };
        let pause = |paused: bool| {
            RawValue::Map(
                [
                    (1, paused.write()),
                    (2, dto::PauseReason::LocalUser.write()),
                    (3, 4u64.write()),
                ]
                .into(),
            )
        };
        assert!(!accepts(&with(19, pause(true))));
        assert!(accepts(&with(19, pause(false))));
    }

    #[test]
    fn card_modes_expose_disabled_and_game_preference_from_owner_facts() {
        let facts = facts(owner(), None);
        let resources = facts.resources().unwrap();
        // Disabled lives only in the owner preferences / options summary.
        assert_eq!(mouse_mode(&facts, resources.as_ref()), Some(ResourceMode::Disabled));
        assert_eq!(keyboard_mode(&facts, resources.as_ref()), Some(ResourceMode::Disabled));
        assert_eq!(choice(&facts, OptionId::GameModePreference), Some(1));
        let game = resources.unwrap().game().unwrap().unwrap();
        assert!(game.active().unwrap());
        assert!(!game.detected().unwrap());
    }

    #[test]
    fn focus_lock_tokens_bind_owner_boot() {
        let owner = owner();
        let facts = facts(owner, Some(owner.device_id));
        let unlock = focus_token(&facts, false).unwrap();
        assert!(unlock.starts_with(&owner.device_id.to_hex()));
        assert_ne!(unlock, focus_token(&facts, true).unwrap());
    }

    #[test]
    fn local_microphone_token_names_this_device_and_revision() {
        assert_eq!(local_mic_token("ab", 4), "local/ab/mic/4");
    }

    #[test]
    fn audio_tokens_name_the_role_device_and_mode() {
        let device = DeviceId([9; 32]);
        for role in [Role::Speaker, Role::Microphone] {
            for mode in [Mode::Holder, Mode::Shared, Mode::Local] {
                let token = audio_token(role, device, mode, 42);
                assert_eq!(read_audio_token(&token), Some((role, device, mode)));
            }
        }
        assert_eq!(read_audio_token("route/speaker/00/1/2"), None);
    }
}
