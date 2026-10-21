//! Projects one coherent set of accepted owner observations. The caller supplies
//! membership and live Boots from the authenticated configuration actor.
use super::*;
use crate::api::{DeviceKey, GroupKey};
use oink_protocol::v1::{RawValue, dto};
use std::{collections::BTreeMap, sync::Arc};
use unicode_casefold::UnicodeCaseFold;

#[derive(Clone, Debug)]
pub struct CardMember {
    pub device: DeviceKey,
    pub name: String,
    pub os: String,
    pub is_self: bool,
    pub ready: bool,
    pub offline_reason: OfflineKind,
    pub last_seen_ms: Option<u64>,
    pub rtt_ms: Option<f64>,
    pub live_boot: Option<[u8; 16]>,
    pub facts: Option<Arc<dto::DeviceFacts>>,
    /// Negotiated logical end-to-end capabilities, never a relay's capabilities.
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct FocusObservation {
    pub participating: bool,
    pub valid: bool,
    pub independent: bool,
    pub device: Option<DeviceKey>,
    pub locked_device: Option<DeviceKey>,
}
#[derive(Clone, Debug)]
pub struct CardInputs {
    pub group: Option<GroupKey>,
    pub members: Vec<CardMember>,
    pub focus: FocusObservation,
    /// None with baseline=false is unknown; None with baseline=true means no claim.
    pub exit_baseline: bool,
    /// The local owner has retained every current member's audio claim baseline.
    /// Historical resolution may still be unknown until an explicit selection.
    pub local_exit_selection_ready: bool,
    pub selected_exit: Option<DeviceKey>,
    pub exit_claim: Option<(u64, DeviceKey)>,
    pub emergency_delivery: Option<EmergencyDeliveryCard>,
}

#[derive(Debug)]
pub struct Projector {
    tokens: ActionTokens,
    current: Option<Arc<DeviceCardsView>>,
}
impl Projector {
    pub fn new() -> Result<Self, getrandom::Error> {
        Ok(Self {
            tokens: ActionTokens::new()?,
            current: None,
        })
    }
    pub fn tokens(&self) -> &ActionTokens {
        &self.tokens
    }
    pub fn snapshot(&self) -> Option<Arc<DeviceCardsView>> {
        self.current.clone()
    }
    /// Equal projections retain the exact timestamp and revision. A single
    /// replacement covers both sides of focus/exit handoffs and group changes.
    pub fn update(&mut self, input: &CardInputs, now_ms: u64) -> Option<Arc<DeviceCardsView>> {
        let names: BTreeMap<_, _> = input.members.iter().map(|m| (m.device, m.name.clone())).collect();
        let mut cards: Vec<_> = input.members.iter().map(|m| self.card(input, m, &names)).collect();
        cards.sort_by_cached_key(|card| {
            let section = match card.section {
                CardSection::ThisDevice => 0,
                CardSection::Online => 1,
                CardSection::Offline => 2,
            };
            (section, card.name.case_fold().collect::<String>(), card.device)
        });
        let mut next = DeviceCardsView {
            schema: 1,
            stream: self.tokens.stream().to_owned(),
            revision: DecimalU64(1),
            group: input.group,
            generated_at_ms: now_ms,
            exit_volume: self.exit_volume(input, &names),
            emergency_delivery: input.emergency_delivery.clone(),
            cards,
        };
        if let Some(previous) = &self.current {
            next.revision = previous.revision;
            next.generated_at_ms = previous.generated_at_ms;
            if **previous == next {
                return None;
            }
            next.revision = DecimalU64(previous.revision.0.checked_add(1).expect("card revision exhausted"));
            next.generated_at_ms = now_ms;
        }
        let next = Arc::new(next);
        self.current = Some(next.clone());
        Some(next)
    }

    fn exit_volume(&self, input: &CardInputs, names: &BTreeMap<DeviceKey, String>) -> Option<ExitVolumeCard> {
        let device = input.selected_exit?;
        let member = input.members.iter().find(|m| m.device == device)?;
        let mut card = ExitVolumeCard {
            receiver_device: device,
            receiver_name: names.get(&device)?.clone(),
            output_name: None,
            volume_percent: None,
            muted: None,
            condition: ExitVolumeCondition::StateUnknown,
            version_token: None,
            can_set: false,
            refusal: Some(ExitVolumeRefusal::StateUnknown),
        };
        if !member.is_self && !member.ready {
            card.condition = ExitVolumeCondition::Offline;
            card.refusal = Some(ExitVolumeRefusal::Offline);
            return Some(card);
        }
        if !member.capabilities.iter().any(|c| c == "oink.audio.exit-volume/1") {
            card.condition = ExitVolumeCondition::Unsupported;
            card.refusal = Some(ExitVolumeRefusal::Unsupported);
            return Some(card);
        }
        let state = member
            .facts
            .as_deref()
            .filter(|f| {
                f.boot().ok().and_then(|b| b.nonce().ok()).map(|n| n.0) == member.live_boot
                    && member.live_boot.is_some()
                    && f.group().ok().map(|g| GroupKey(oink_foundation::GroupId(g.0))) == input.group
            })
            .and_then(|f| f.exit_volume().ok().flatten());
        let Some(state) = state else {
            return Some(card);
        };
        let Some(version) = state.version().ok() else {
            return Some(card);
        };
        let Some(exit) = version.exit().ok() else {
            return Some(card);
        };
        let Some(claim) = version.exit_claim().ok() else {
            return Some(card);
        };
        let claim_counter = claim.counter().ok();
        let claim_owner = claim.owner().ok().as_ref().and_then(device_key);
        if exit.device_id().ok().as_ref().and_then(device_key) != Some(device)
            || exit.nonce().ok().map(|n| n.0) != member.live_boot
            || claim_counter.zip(claim_owner) != input.exit_claim
        {
            card.refusal = Some(ExitVolumeRefusal::StateStale);
            return Some(card);
        }
        let output = version.output_id().ok().flatten().map(|s| s.0);
        card.output_name = member
            .facts
            .as_deref()
            .and_then(|f| f.capabilities().ok())
            .and_then(|c| c.audio_outputs().ok())
            .and_then(|outputs| {
                outputs
                    .0
                    .into_iter()
                    .find(|o| o.id().ok().map(|s| s.0).as_ref() == output.as_ref())
            })
            .and_then(|o| o.name().ok().map(|s| s.0));
        card.volume_percent = state.volume_percent().ok().flatten();
        card.muted = state.muted().ok().flatten();
        let (condition, refusal) = match state.status().ok() {
            Some(dto::ExitVolumeStatus::Ready) => (ExitVolumeCondition::Ready, None),
            Some(dto::ExitVolumeStatus::Unsupported) => {
                (ExitVolumeCondition::Unsupported, Some(ExitVolumeRefusal::Unsupported))
            }
            Some(dto::ExitVolumeStatus::Unavailable) => (
                ExitVolumeCondition::Unavailable,
                Some(ExitVolumeRefusal::OutputUnavailable),
            ),
            Some(dto::ExitVolumeStatus::PermissionDenied) => (
                ExitVolumeCondition::PermissionDenied,
                Some(ExitVolumeRefusal::PermissionDenied),
            ),
            Some(dto::ExitVolumeStatus::BackendFailed) => {
                (ExitVolumeCondition::BackendFailed, Some(ExitVolumeRefusal::RemoteFault))
            }
            _ => (ExitVolumeCondition::StateUnknown, Some(ExitVolumeRefusal::StateUnknown)),
        };
        card.condition = condition;
        card.refusal = refusal;
        if condition == ExitVolumeCondition::Ready
            && card.volume_percent.is_some()
            && card.muted.is_some()
            && let (
                Some(group),
                Some(boot_nonce),
                Some(revision),
                Some(output_id),
                Some(output_generation),
                Some((claim_counter, claim_owner)),
            ) = (
                input.group,
                member.live_boot,
                version.revision().ok(),
                output,
                version.output_generation().ok(),
                input.exit_claim,
            )
        {
            card.version_token = Some(self.tokens.issue(&TokenBinding {
                owner: OwnerVersion {
                    group,
                    device,
                    boot_nonce,
                    revision,
                },
                operation: TokenOperation::ExitVolume {
                    output_id,
                    output_generation,
                    claim_counter,
                    claim_owner,
                },
            }));
            card.can_set = true;
        }
        Some(card)
    }

    fn card(&self, input: &CardInputs, member: &CardMember, names: &BTreeMap<DeviceKey, String>) -> DeviceCard {
        let (os, logo) = match member.os.as_str() {
            "macos" => (CardOs::Macos, CardLogo::Macos),
            "windows" => (CardOs::Windows, CardLogo::Windows10),
            "linux" => (CardOs::Linux, CardLogo::Linux),
            _ => (CardOs::Other, CardLogo::Generic),
        };
        let online = member.is_self || member.ready;
        let section = if member.is_self {
            CardSection::ThisDevice
        } else if online {
            CardSection::Online
        } else {
            CardSection::Offline
        };
        let connection = if member.is_self {
            CardConnection::ThisDevice
        } else if online {
            CardConnection::Online {
                signal: Signal::online(member.rtt_ms),
            }
        } else {
            CardConnection::Offline {
                reason: member.offline_reason,
                signal: Signal::offline(),
            }
        };
        let facts = member.facts.as_deref().filter(|f| {
            let boot = f.boot().ok();
            boot.as_ref().and_then(|b| b.nonce().ok()).map(|n| n.0) == member.live_boot
                && boot
                    .as_ref()
                    .and_then(|b| b.device_id().ok())
                    .and_then(|d| device_key(&d))
                    == Some(member.device)
                && f.group().ok().map(|g| GroupKey(oink_foundation::GroupId(g.0))) == input.group
                && member.live_boot.is_some()
        });
        let (status, pause) = if online {
            let context = OwnerContext::new(input.group, member, facts, &self.tokens);
            let pause = context.pause();
            (Some(context.status(input, names, pause.as_ref())), pause)
        } else {
            (None, None)
        };
        DeviceCard {
            device: member.device,
            name: member.name.clone(),
            os,
            logo,
            is_self: member.is_self,
            section,
            connection,
            last_seen_ms: if online { None } else { member.last_seen_ms },
            status,
            pause,
        }
    }
}

pub(crate) fn device_key(value: &dto::DeviceId) -> Option<DeviceKey> {
    if value.algorithm().ok()?.0 != dto::ED25519 {
        return None;
    }
    Some(DeviceKey(oink_foundation::DeviceId(
        value.public_key().ok()?.0.try_into().ok()?,
    )))
}

struct OwnerContext<'a> {
    group: Option<GroupKey>,
    member: &'a CardMember,
    facts: Option<&'a dto::DeviceFacts>,
    preferences: BTreeMap<u32, dto::DevicePreference>,
    tokens: &'a ActionTokens,
}
impl<'a> OwnerContext<'a> {
    fn new(
        group: Option<GroupKey>,
        member: &'a CardMember,
        facts: Option<&'a dto::DeviceFacts>,
        tokens: &'a ActionTokens,
    ) -> Self {
        let preferences = facts
            .and_then(|f| f.preferences().ok().flatten())
            .map(|p| p.0)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.option().ok().map(|o| (o.number(), p)))
            .collect();
        Self {
            group,
            member,
            facts,
            preferences,
            tokens,
        }
    }
    fn supports(&self, capability: &str) -> bool {
        self.member.capabilities.iter().any(|c| c == capability)
    }
    fn version(&self, option: u32) -> Option<OwnerVersion> {
        Some(OwnerVersion {
            group: self.group?,
            device: self.member.device,
            boot_nonce: self.member.live_boot?,
            revision: self.preferences.get(&option)?.revision().ok()?,
        })
    }
    fn value(&self, option: u32) -> Option<RawValue> {
        self.preferences.get(&option)?.value().ok()
    }
    fn boolean(&self, option: u32) -> Option<bool> {
        match self.value(option)? {
            RawValue::Bool(v) => Some(v),
            _ => None,
        }
    }
    fn mode(&self, option: u32) -> Option<SharingMode> {
        match self.value(option)? {
            RawValue::Uint(0) => Some(SharingMode::Shared),
            RawValue::Uint(1) => Some(SharingMode::Local),
            RawValue::Uint(2) if option == 1 && self.supports("oink.input.mouse-disabled/1") => {
                Some(SharingMode::Disabled)
            }
            _ => None,
        }
    }
    fn resources(&self) -> Option<dto::ResourceFacts> {
        if !self.supports("oink.device-resources/1") {
            return None;
        }
        self.facts?.resources().ok().flatten()
    }
    fn fault(&self, option: u32) -> Option<ResourceFault> {
        let faults = self.resources()?.faults().ok().flatten()?;
        let fault = match option {
            1 => faults.mouse(),
            2 | 5 => faults.keyboard(),
            3 => faults.clipboard(),
            4 | 15 | 20 => faults.speaker(),
            6 => faults.screen(),
            _ => return None,
        }
        .ok()
        .flatten()?;
        ResourceFault::try_from(&fault).ok()
    }
    fn action(&self, edit: CardOptionEdit) -> CardAction {
        let option = edit.number();
        if matches!(edit, CardOptionEdit::MouseMode(SharingMode::Disabled))
            && !self.supports("oink.input.mouse-disabled/1")
        {
            return CardAction::unavailable(CardActionUnavailable::Unsupported);
        }
        if let Some(fault) = self.fault(option) {
            match fault.code {
                ResourceFaultCode::None => {}
                ResourceFaultCode::Unrecognized => return CardAction::unavailable(CardActionUnavailable::StateUnknown),
                _ => {
                    return if self.member.is_self {
                        CardAction::OpenPermissions
                    } else {
                        CardAction::unavailable(CardActionUnavailable::RemoteFault)
                    };
                }
            }
        } else if option != 18 {
            return CardAction::unavailable(CardActionUnavailable::StateUnknown);
        }
        if !self.supports("oink.device-options/1") {
            return CardAction::unavailable(CardActionUnavailable::Unsupported);
        }
        self.version(option)
            .map(|v| self.tokens.action(v, edit))
            .unwrap_or_else(|| CardAction::unavailable(CardActionUnavailable::StateUnknown))
    }
    fn pause(&self) -> Option<LocalPauseCard> {
        let state = self.facts?.pause().ok().flatten()?;
        let paused = state.paused().ok()?;
        let revision = state.revision().ok()?;
        let reason = match state.reason().ok()? {
            dto::PauseReason::None => PauseReason::None,
            dto::PauseReason::LocalUser => PauseReason::LocalUser,
            dto::PauseReason::RemoteUser => PauseReason::RemoteUser,
            dto::PauseReason::Emergency => PauseReason::Emergency,
            dto::PauseReason::InputBacklog => PauseReason::InputBacklog,
            dto::PauseReason::Unknown(_) => PauseReason::Unknown,
        };
        let can_resume =
            self.supports("oink.device-options/1") && self.group.is_some() && self.member.live_boot.is_some();
        let action = if can_resume {
            self.tokens.pause(
                OwnerVersion {
                    group: self.group?,
                    device: self.member.device,
                    boot_nonce: self.member.live_boot?,
                    revision,
                },
                !paused,
            )
        } else {
            CardAction::unavailable(CardActionUnavailable::Unsupported)
        };
        Some(LocalPauseCard {
            action,
            paused,
            reason,
            revision: DecimalU64(revision),
            can_resume,
        })
    }
    fn resource(
        &self,
        option: u32,
        mode: Option<SharingMode>,
        disabled: Option<bool>,
        paused: bool,
        fallback: bool,
    ) -> ResourceCard {
        let fault = self.fault(option);
        let known = mode.is_some()
            && fault
                .as_ref()
                .is_some_and(|f| f.code != ResourceFaultCode::Unrecognized);
        let faulty = fault
            .as_ref()
            .is_some_and(|f| !matches!(f.code, ResourceFaultCode::None | ResourceFaultCode::Unrecognized));
        let icon = if faulty {
            SharingIcon::Fault
        } else if !known {
            SharingIcon::Unknown
        } else if disabled == Some(true) {
            SharingIcon::Disabled
        } else {
            match mode {
                Some(SharingMode::Shared) => SharingIcon::Shared,
                Some(SharingMode::Local) => SharingIcon::Local,
                Some(SharingMode::Disabled) => SharingIcon::Disabled,
                None => SharingIcon::Unknown,
            }
        };
        let condition = if faulty {
            CardCondition::Faulted
        } else if !known {
            CardCondition::StateUnknown
        } else if paused {
            CardCondition::LocalPaused
        } else if fallback {
            CardCondition::ScreenDisabledFallback
        } else {
            CardCondition::Ready
        };
        let next_mode = mode.map(|m| {
            if m == SharingMode::Shared {
                SharingMode::Local
            } else {
                SharingMode::Shared
            }
        });
        let edit = match option {
            1 => next_mode.map(CardOptionEdit::MouseMode),
            2 if disabled == Some(true) => Some(CardOptionEdit::KeyboardDisabled(false)),
            2 => next_mode.map(CardOptionEdit::KeyboardMode),
            3 => mode.map(|m| CardOptionEdit::ClipboardEnabled(m != SharingMode::Shared)),
            6 => mode.map(|m| CardOptionEdit::ScreenShared(m != SharingMode::Shared)),
            _ => None,
        };
        let action = edit
            .map(|e| self.action(e))
            .unwrap_or_else(|| CardAction::unavailable(CardActionUnavailable::StateUnknown));
        let action = if faulty {
            if self.member.is_self {
                CardAction::OpenPermissions
            } else {
                CardAction::unavailable(CardActionUnavailable::RemoteFault)
            }
        } else {
            action
        };
        ResourceCard {
            action,
            configured_mode: mode,
            icon,
            fault,
            condition,
        }
    }
    fn status(
        &self,
        input: &CardInputs,
        names: &BTreeMap<DeviceKey, String>,
        pause: Option<&LocalPauseCard>,
    ) -> DeviceCardStatus {
        let paused = pause.is_some_and(|p| p.paused);
        let screen = self.boolean(6).map(|shared| {
            if shared {
                SharingMode::Shared
            } else {
                SharingMode::Local
            }
        });
        let clipboard = self.boolean(3).map(|shared| {
            if shared {
                SharingMode::Shared
            } else {
                SharingMode::Local
            }
        });
        let keyboard_disabled = self.boolean(5);
        let fallback = self
            .facts
            .and_then(|f| f.local_options().ok())
            .and_then(|o| o.screen_local_input().ok())
            .unwrap_or(false);
        let focus = &input.focus;
        let valid_focus = focus.participating && focus.valid && !focus.independent && !paused;
        let focus_device = if valid_focus { focus.device } else { None };
        let locked = if valid_focus && focus.locked_device == focus.device {
            focus.locked_device
        } else {
            None
        };
        let cursor = CursorCard {
            icon: if !valid_focus || focus_device.is_none() {
                CursorIcon::Unknown
            } else if locked == Some(self.member.device) {
                CursorIcon::Locked
            } else if focus_device == Some(self.member.device) {
                CursorIcon::Local
            } else {
                CursorIcon::Remote
            },
            focus_device,
            focus_name: focus_device.and_then(|d| names.get(&d).cloned()),
            locked_device: locked,
            context: if paused {
                CursorContext::LocalPaused
            } else if focus.independent {
                CursorContext::Independent
            } else if valid_focus {
                CursorContext::Group
            } else {
                CursorContext::Unknown
            },
        };
        let preference = match self.value(18) {
            Some(RawValue::Uint(0)) => Some(GamePreference::Auto),
            Some(RawValue::Uint(1)) => Some(GamePreference::On),
            Some(RawValue::Uint(2)) => Some(GamePreference::Off),
            _ => None,
        };
        let game = self.resources().and_then(|r| r.game().ok().flatten());
        DeviceCardStatus {
            actions: DeviceCardActions {
                screen_shared: self.action(CardOptionEdit::ScreenShared(true)),
                screen_local: self.action(CardOptionEdit::ScreenShared(false)),
                mouse_shared: self.action(CardOptionEdit::MouseMode(SharingMode::Shared)),
                mouse_local: self.action(CardOptionEdit::MouseMode(SharingMode::Local)),
                mouse_disabled: self.action(CardOptionEdit::MouseMode(SharingMode::Disabled)),
                keyboard_shared: self.action(CardOptionEdit::KeyboardMode(SharingMode::Shared)),
                keyboard_local: self.action(CardOptionEdit::KeyboardMode(SharingMode::Local)),
                keyboard_disabled: self.action(CardOptionEdit::KeyboardDisabled(true)),
                keyboard_enabled: self.action(CardOptionEdit::KeyboardDisabled(false)),
                clipboard_shared: self.action(CardOptionEdit::ClipboardEnabled(true)),
                clipboard_local: self.action(CardOptionEdit::ClipboardEnabled(false)),
                forwarding_enabled: self.action(CardOptionEdit::ForwardingEnabled(true)),
                forwarding_disabled: self.action(CardOptionEdit::ForwardingEnabled(false)),
            },
            cursor,
            screen: self.resource(6, screen, None, paused, fallback),
            mouse: self.resource(1, self.mode(1), None, paused, false),
            keyboard: KeyboardCard {
                resource: self.resource(2, self.mode(2), keyboard_disabled, paused, false),
                disabled: keyboard_disabled,
            },
            clipboard: self.resource(3, clipboard, None, paused, false),
            speaker: self.speaker(input, names, paused),
            game: GameCard {
                preference,
                active: game.as_ref().and_then(|g| g.active().ok()),
                detected: game.as_ref().and_then(|g| g.detected().ok()),
                actions: GameActions {
                    auto: self.action(CardOptionEdit::GameModePreference(GamePreference::Auto)),
                    on: self.action(CardOptionEdit::GameModePreference(GamePreference::On)),
                    off: self.action(CardOptionEdit::GameModePreference(GamePreference::Off)),
                },
            },
        }
    }
    fn speaker(&self, input: &CardInputs, names: &BTreeMap<DeviceKey, String>, paused: bool) -> SpeakerCard {
        let observed = self.resources().and_then(|r| r.speaker().ok().flatten());
        let enabled = self.boolean(4);
        let receiver = input.selected_exit == Some(self.member.device);
        let mode = if !input.exit_baseline {
            None
        } else if receiver {
            Some(SpeakerMode::Receiver)
        } else {
            enabled.map(|v| if v { SpeakerMode::Source } else { SpeakerMode::Unused })
        };
        let fault = self.fault(4);
        let faulty = fault
            .as_ref()
            .is_some_and(|f| !matches!(f.code, ResourceFaultCode::None | ResourceFaultCode::Unrecognized));
        let known = mode.is_some()
            && fault
                .as_ref()
                .is_some_and(|f| f.code != ResourceFaultCode::Unrecognized);
        let activity = match observed.as_ref().and_then(|o| o.activity().ok().flatten()) {
            Some(dto::SpeakerActivity::Unused | dto::SpeakerActivity::ExitReady) => SpeakerActivity::Idle,
            Some(dto::SpeakerActivity::ExitPlaying) => SpeakerActivity::Playing,
            Some(dto::SpeakerActivity::SourceForwarding) => SpeakerActivity::Forwarding,
            Some(dto::SpeakerActivity::SourceSoundSleep) => SpeakerActivity::SoundSleep,
            Some(dto::SpeakerActivity::Preparing) => SpeakerActivity::Preparing,
            Some(dto::SpeakerActivity::WaitingForExit) => SpeakerActivity::WaitingForExit,
            Some(dto::SpeakerActivity::LocalPaused) => SpeakerActivity::Paused,
            Some(dto::SpeakerActivity::SystemSleeping) => SpeakerActivity::Sleeping,
            Some(dto::SpeakerActivity::Failed) => SpeakerActivity::Failed,
            _ => SpeakerActivity::Unknown,
        };
        let activity = if paused { SpeakerActivity::Paused } else { activity };
        let source_live = mode == Some(SpeakerMode::Source)
            && matches!(activity, SpeakerActivity::Forwarding | SpeakerActivity::SoundSleep);
        let forwarding = if source_live {
            observed
                .as_ref()
                .and_then(|o| ForwardingCard::new(o.forwarding_volume_percent().ok()??, o.forwarding_muted().ok()??))
        } else {
            None
        };
        let receiver_device = if receiver {
            Some(self.member.device)
        } else {
            observed
                .as_ref()
                .and_then(|o| o.source_destination().ok().flatten())
                .and_then(|d| device_key(&d))
        };
        let action = match mode {
            Some(SpeakerMode::Receiver) => self.action(CardOptionEdit::AudioExitClaim(false)),
            Some(SpeakerMode::Source) => self.action(CardOptionEdit::ForwardingEnabled(false)),
            Some(SpeakerMode::Unused) => self.action(CardOptionEdit::ForwardingEnabled(true)),
            None => CardAction::unavailable(CardActionUnavailable::StateUnknown),
        };
        let action = if faulty {
            if self.member.is_self {
                CardAction::OpenPermissions
            } else {
                CardAction::unavailable(CardActionUnavailable::RemoteFault)
            }
        } else {
            action
        };
        let forwarding_controls_token = if self.supports("oink.device-options/1")
            && fault.as_ref().is_some_and(|f| f.code == ResourceFaultCode::None)
        {
            self.version(15).map(|owner| {
                self.tokens.issue(&TokenBinding {
                    owner,
                    operation: TokenOperation::ForwardingControls,
                })
            })
        } else {
            None
        };
        SpeakerCard {
            resource: ResourceCard {
                action,
                configured_mode: mode,
                icon: if faulty {
                    SpeakerIcon::Fault
                } else if !known {
                    SpeakerIcon::Unknown
                } else {
                    match mode {
                        Some(SpeakerMode::Receiver) => SpeakerIcon::Exit,
                        Some(SpeakerMode::Source) => SpeakerIcon::Forwarding,
                        Some(SpeakerMode::Unused) => SpeakerIcon::Unused,
                        None => SpeakerIcon::Unknown,
                    }
                },
                fault,
                condition: if faulty {
                    CardCondition::Faulted
                } else if !known {
                    CardCondition::StateUnknown
                } else if paused {
                    CardCondition::LocalPaused
                } else {
                    CardCondition::Ready
                },
            },
            make_exit_action: if self.member.is_self && !input.local_exit_selection_ready {
                CardAction::unavailable(CardActionUnavailable::StateUnknown)
            } else {
                self.action(CardOptionEdit::AudioExitClaim(true))
            },
            forwarding_controls_token,
            forwarding_enabled: enabled,
            activity,
            receiver_device,
            receiver_name: receiver_device.and_then(|d| names.get(&d).cloned()),
            active_sources: if receiver {
                observed.as_ref().and_then(|o| o.active_sources().ok().flatten())
            } else {
                None
            },
            receive_ready: if receiver {
                observed.as_ref().and_then(|o| o.receive_ready().ok().flatten())
            } else {
                None
            },
            forwarding,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{
        RawMap,
        schema::{WireDto, WireValue},
    };
    fn row(id: u8, name: &str, online: bool) -> CardMember {
        CardMember {
            device: oink_foundation::DeviceId([id; 32]).into(),
            name: name.into(),
            os: "windows".into(),
            is_self: id == 1,
            ready: online,
            offline_reason: OfflineKind::Disconnected,
            last_seen_ms: Some(123),
            rtt_ms: None,
            live_boot: Some([id; 16]),
            facts: None,
            capabilities: vec!["oink.device-options/1".into(), "oink.device-resources/1".into()],
        }
    }
    fn inputs(members: Vec<CardMember>) -> CardInputs {
        CardInputs {
            group: Some(oink_foundation::GroupId([1; 16]).into()),
            members,
            focus: FocusObservation::default(),
            exit_baseline: false,
            local_exit_selection_ready: true,
            selected_exit: None,
            exit_claim: None,
            emergency_delivery: None,
        }
    }
    fn map(items: impl IntoIterator<Item = (u32, RawValue)>) -> RawValue {
        RawValue::Map(items.into_iter().collect())
    }
    fn facts(member: &CardMember, fault: u64) -> Arc<dto::DeviceFacts> {
        let id = map([
            (1, RawValue::Text("ed25519".into())),
            (2, RawValue::Bytes(member.device.0.0.to_vec())),
        ]);
        let healthy = map([(1, RawValue::Uint(fault))]);
        let faults = map((1..=5).map(|key| (key, healthy.clone())));
        let preferences: Vec<_> = [
            (1, RawValue::Uint(0)),
            (2, RawValue::Uint(0)),
            (3, RawValue::Bool(true)),
            (4, RawValue::Bool(true)),
            (5, RawValue::Bool(false)),
            (6, RawValue::Bool(true)),
            (18, RawValue::Uint(0)),
        ]
        .into_iter()
        .map(|(option, value)| map([(1, RawValue::Uint(option)), (2, RawValue::Uint(1)), (3, value)]))
        .collect();
        let raw: RawMap = [
            (
                1,
                map([(1, id), (2, RawValue::Bytes(member.live_boot.unwrap().to_vec()))]),
            ),
            (2, RawValue::Uint(1)),
            (3, map([(1, RawValue::Uint(1)), (2, RawValue::Uint(0))])),
            (4, RawValue::Text("test".into())),
            (5, RawValue::Bytes(vec![1; 16])),
            (6, RawValue::Text(member.name.clone())),
            (7, RawValue::Text(member.os.clone())),
            (8, RawValue::Text("test".into())),
            (9, RawValue::Text("test".into())),
            (10, RawValue::Array(vec![])),
            (
                11,
                map([
                    (1, RawValue::Bool(true)),
                    (2, RawValue::Uint(1)),
                    (4, RawValue::Uint(0)),
                ]),
            ),
            (12, RawValue::Array(vec![])),
            (
                13,
                map([
                    (1, RawValue::Array(vec![])),
                    (2, RawValue::Uint(4)),
                    (3, RawValue::Array(vec![])),
                    (4, RawValue::Array(vec![])),
                ]),
            ),
            (
                14,
                map([
                    (1, RawValue::Bool(true)),
                    (2, RawValue::Bool(true)),
                    (3, 1f32.write()),
                    (4, RawValue::Bool(false)),
                    (5, RawValue::Bool(false)),
                    (6, RawValue::Bool(false)),
                ]),
            ),
            (15, RawValue::Array(vec![])),
            (16, map([(4, faults)])),
            (18, RawValue::Array(preferences)),
            (
                19,
                map([
                    (1, RawValue::Bool(false)),
                    (2, RawValue::Uint(0)),
                    (3, RawValue::Uint(1)),
                ]),
            ),
        ]
        .into_iter()
        .collect();
        Arc::new(dto::DeviceFacts::from_raw(raw).unwrap())
    }
    #[test]
    fn game_badges_project_each_owners_facts_independently_of_focus() {
        let mut members = vec![row(1, "self", true), row(2, "peer", true)];
        for member in &mut members {
            let mut raw = facts(member, 0).raw().clone();
            let RawValue::Map(resources) = raw.get_mut(&16).unwrap() else {
                unreachable!()
            };
            resources.insert(7, map([(1, true.write()), (2, true.write())]));
            // Exercise the real Facts codec, including a remote owner's game observation.
            let facts = dto::DeviceFacts::from_raw(raw).unwrap();
            member.facts = Some(Arc::new(
                dto::DeviceFacts::decode(&facts.encode(128 * 1024).unwrap(), 128 * 1024).unwrap(),
            ));
        }
        let mut input = inputs(members);
        input.focus = FocusObservation {
            participating: true,
            valid: true,
            device: Some(input.members[0].device),
            ..Default::default()
        };
        let mut projector = Projector::new().unwrap();
        let view = projector.update(&input, 1).unwrap();
        assert!(
            view.cards
                .iter()
                .all(|c| c.status.as_ref().unwrap().game.active == Some(true))
        );
        input.focus.device = Some(input.members[1].device);
        let view = projector.update(&input, 2).unwrap();
        assert!(
            view.cards
                .iter()
                .all(|c| c.status.as_ref().unwrap().game.active == Some(true))
        );
        let mut raw = input.members[0].facts.as_ref().unwrap().raw().clone();
        let RawValue::Map(resources) = raw.get_mut(&16).unwrap() else {
            unreachable!()
        };
        resources.insert(7, map([(1, false.write()), (2, false.write())]));
        input.members[0].facts = Some(Arc::new(dto::DeviceFacts::from_raw(raw).unwrap()));
        let view = projector.update(&input, 3).unwrap();
        assert_eq!(view.cards[0].status.as_ref().unwrap().game.active, Some(false));
        assert_eq!(view.cards[1].status.as_ref().unwrap().game.active, Some(true));
    }
    #[test]
    fn unicode_order_and_offline_rows_do_not_depend_on_latency_or_cached_facts() {
        let mut offline = row(9, "first", false);
        offline.facts = Some(facts(&offline, 0));
        let mut input = inputs(vec![
            row(3, "Straße", true),
            offline,
            row(1, "self", true),
            row(2, "STRASSE", true),
        ]);
        let mut projector = Projector::new().unwrap();
        let view = projector.update(&input, 10).unwrap();
        assert_eq!(
            view.cards.iter().map(|c| c.device.0.0[0]).collect::<Vec<_>>(),
            vec![1, 2, 3, 9]
        );
        assert!(view.cards[3].status.is_none() && view.cards[3].pause.is_none());
        assert_eq!(view.cards[3].last_seen_ms, Some(123));
        for reason in [
            OfflineKind::Connecting,
            OfflineKind::AddressStale,
            OfflineKind::ProtocolIncompatible,
        ] {
            input.members[1].offline_reason = reason;
            let view = projector.update(&input, 11).unwrap();
            assert!(view.cards[3].status.is_none() && view.cards[3].pause.is_none());
            assert_eq!(view.cards[3].last_seen_ms, Some(123));
        }
    }
    #[test]
    fn identical_snapshots_keep_revision_and_generation_time() {
        let mut projector = Projector::new().unwrap();
        let mut input = inputs(vec![row(1, "self", true), row(2, "peer", true)]);
        let first = projector.update(&input, 10).unwrap();
        input.members[1].last_seen_ms = Some(999);
        assert!(projector.update(&input, 20).is_none());
        assert!(Arc::ptr_eq(&first, &projector.snapshot().unwrap()));
        input.members[1].rtt_ms = Some(20.0);
        let second = projector.update(&input, 30).unwrap();
        assert_eq!(second.revision.0, 2);
        assert_eq!(second.generated_at_ms, 30);
        assert_eq!(first.generated_at_ms, 10);
    }
    #[test]
    fn faults_are_owner_specific_and_actions_have_self_remote_semantics() {
        let mut me = row(1, "self", true);
        me.facts = Some(facts(&me, 2));
        let mut peer = row(2, "peer", true);
        peer.facts = Some(facts(&peer, 2));
        let input = inputs(vec![me, peer]);
        let view = Projector::new().unwrap().update(&input, 1).unwrap();
        let me = view.cards[0].status.as_ref().unwrap();
        let peer = view.cards[1].status.as_ref().unwrap();
        assert_eq!(me.mouse.icon, SharingIcon::Fault);
        assert_eq!(me.mouse.action, CardAction::OpenPermissions);
        assert_eq!(
            peer.clipboard.action,
            CardAction::unavailable(CardActionUnavailable::RemoteFault)
        );
        assert_eq!(peer.speaker.resource.icon, SpeakerIcon::Fault);
    }
    #[test]
    fn old_boot_and_future_faults_never_become_healthy() {
        let mut peer = row(2, "peer", true);
        peer.facts = Some(facts(&peer, 900));
        let mut input = inputs(vec![peer]);
        let mut projector = Projector::new().unwrap();
        let first = projector.update(&input, 1).unwrap();
        let status = first.cards[0].status.as_ref().unwrap();
        assert_eq!(status.screen.icon, SharingIcon::Unknown);
        assert_eq!(status.screen.fault.as_ref().unwrap().unknown_code, Some(900));
        input.members[0].live_boot = Some([99; 16]);
        let next = projector.update(&input, 2).unwrap();
        let status = next.cards[0].status.as_ref().unwrap();
        assert!(status.screen.configured_mode.is_none());
        assert!(status.screen.fault.is_none());
        assert!(next.cards[0].pause.is_none());
    }
    #[test]
    fn local_exit_selection_waits_for_baselines_but_not_historical_resolution() {
        let mut me = row(1, "self", true);
        let mut peer = row(2, "peer", true);
        for member in [&mut me, &mut peer] {
            let mut raw = facts(member, 0).raw().clone();
            let RawValue::Array(preferences) = raw.get_mut(&18).unwrap() else {
                panic!()
            };
            preferences.push(map([(1, 20u32.write()), (2, 5u64.write()), (3, RawValue::Null)]));
            member.facts = Some(Arc::new(dto::DeviceFacts::from_raw(raw).unwrap()));
        }
        let mut input = inputs(vec![me, peer]);
        input.local_exit_selection_ready = false;
        let mut projector = Projector::new().unwrap();
        let before = projector.update(&input, 1).unwrap();
        assert_eq!(
            before.cards[0].status.as_ref().unwrap().speaker.make_exit_action,
            CardAction::unavailable(CardActionUnavailable::StateUnknown)
        );
        // Our retained baselines do not establish the remote owner's readiness.
        assert!(matches!(
            before.cards[1].status.as_ref().unwrap().speaker.make_exit_action,
            CardAction::Invoke { .. }
        ));
        // No local owner revision or historical resolution needs to change.
        // Completing coverage alone must publish a new card and usable token.
        input.local_exit_selection_ready = true;
        let after = projector.update(&input, 2).unwrap();
        assert_eq!(after.revision.0, before.revision.0 + 1);
        assert!(!input.exit_baseline);
        let CardAction::Invoke { token, .. } = &after.cards[0].status.as_ref().unwrap().speaker.make_exit_action else {
            panic!("complete current baselines permit an explicit takeover")
        };
        let binding = projector.tokens().read(token).unwrap();
        assert_eq!(binding.owner.revision, 5);
        assert_eq!(binding.owner.device, input.members[0].device);
    }
    #[test]
    fn volume_controls_bind_claim_boot_output_and_readback() {
        let mut peer = row(2, "exit", true);
        peer.capabilities.push("oink.audio.exit-volume/1".into());
        let mut raw = facts(&peer, 0).raw().clone();
        let boot = raw[&1].clone();
        let device = match &boot {
            RawValue::Map(m) => m[&1].clone(),
            _ => panic!(),
        };
        let claim = map([(1, 9u64.write()), (2, device)]);
        let version = map([
            (1, boot),
            (3, RawValue::Text("output".into())),
            (4, 7u64.write()),
            (5, 11u64.write()),
            (6, claim),
        ]);
        raw.insert(
            17,
            map([(1, version), (2, 42u8.write()), (3, true.write()), (4, 0u32.write())]),
        );
        peer.facts = Some(Arc::new(dto::DeviceFacts::from_raw(raw).unwrap()));
        let device = peer.device;
        let mut input = inputs(vec![peer]);
        input.exit_baseline = true;
        input.selected_exit = Some(device);
        input.exit_claim = Some((9, device));
        let mut projector = Projector::new().unwrap();
        let first = projector.update(&input, 1).unwrap();
        let volume = first.exit_volume.as_ref().unwrap();
        assert_eq!(volume.volume_percent, Some(42));
        assert_eq!(volume.muted, Some(true));
        assert!(volume.can_set);
        let token = volume.version_token.clone().unwrap();
        let binding = projector.tokens().read(&token).unwrap();
        assert_eq!(binding.owner.revision, 11);
        assert!(matches!(
            binding.operation,
            TokenOperation::ExitVolume {
                output_generation: 7,
                claim_counter: 9,
                ..
            }
        ));
        input.members[0].rtt_ms = Some(30.0);
        assert_eq!(
            projector
                .update(&input, 2)
                .unwrap()
                .exit_volume
                .as_ref()
                .unwrap()
                .version_token
                .as_ref(),
            Some(&token)
        );
        input.exit_claim = Some((10, device));
        let stale = projector.update(&input, 3).unwrap();
        assert!(!stale.exit_volume.as_ref().unwrap().can_set);
        assert_eq!(
            stale.exit_volume.as_ref().unwrap().refusal,
            Some(ExitVolumeRefusal::StateStale)
        );
        input.members[0].ready = false;
        let offline = projector.update(&input, 4).unwrap();
        assert_eq!(offline.exit_volume.as_ref().unwrap().receiver_device, device);
        assert_eq!(
            offline.exit_volume.as_ref().unwrap().condition,
            ExitVolumeCondition::Offline
        );
        assert!(offline.exit_volume.as_ref().unwrap().version_token.is_none());
    }

    #[test]
    fn accepted_focus_moves_both_rows_in_one_snapshot() {
        let mut input = inputs(vec![row(1, "self", true), row(2, "peer", true)]);
        input.focus = FocusObservation {
            participating: true,
            valid: true,
            independent: false,
            device: Some(input.members[0].device),
            locked_device: None,
        };
        let mut projector = Projector::new().unwrap();
        let first = projector.update(&input, 1).unwrap();
        assert_eq!(first.cards[0].status.as_ref().unwrap().cursor.icon, CursorIcon::Local);
        input.focus.device = Some(input.members[1].device);
        let next = projector.update(&input, 2).unwrap();
        assert_eq!(next.cards[0].status.as_ref().unwrap().cursor.icon, CursorIcon::Remote);
        assert_eq!(next.cards[1].status.as_ref().unwrap().cursor.icon, CursorIcon::Local);
        input.focus.independent = true;
        let island = projector.update(&input, 3).unwrap();
        assert!(
            island
                .cards
                .iter()
                .all(|c| c.status.as_ref().unwrap().cursor.icon == CursorIcon::Unknown)
        );
    }
}
