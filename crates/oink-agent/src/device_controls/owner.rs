//! Durable option and volatile pause effects. The caller already authenticated
//! membership, capability and Boot; the owner gate covers CAS through publication.
use crate::{
    node::{Node, lock},
    threads::input::InputCommand,
};
use oink_protocol::v1::{
    Message, RawMap, dto,
    schema::{List, WireDto, WireValue},
};
type Failure = dto::ControlFailure;
fn field<T>(result: oink_protocol::v1::Result<T>) -> Result<T, Failure> {
    result.map_err(|_| Failure::InvalidValue)
}
fn current(item: &dto::DevicePreference) -> Result<dto::LocalOptionState, Failure> {
    dto::LocalOptionState::from_raw(
        [
            (1, field(item.option())?.write()),
            (2, field(item.value())?),
            (3, field(item.revision())?.write()),
        ]
        .into_iter()
        .collect(),
    )
    .map_err(|_| Failure::InvalidValue)
}
impl Node {
    pub(crate) fn edit_local_input_settings(
        &self,
        patch: &serde_json::Value,
        edit: impl FnOnce(&mut crate::config::Settings),
    ) -> crate::api::ApiResult<()> {
        // The API caller holds the owner gate across patch resolution and save.
        if lock(&self.inner.controls).safety_pending() {
            return Err(crate::api::ApiError::Unavailable {
                message: "Safety cleanup is pending".into(),
            });
        }
        let epoch = self.input_status()?.emergency_sequence;
        {
            let mut config = lock(&self.inner.config);
            let mut candidate = config.settings.clone();
            edit(&mut candidate);
            let preferences =
                config
                    .preview_preferences(&candidate)
                    .map_err(|error| crate::api::ApiError::InvalidArgument {
                        message: error.to_string(),
                    })?;
            self.preflight_owner_preferences(preferences)
                .map_err(|reason| crate::api::ApiError::InvalidArgument {
                    message: format!("{reason:?}"),
                })?;
            config
                .update_local(patch, |settings| *settings = candidate)
                .map_err(|error| crate::api::ApiError::InvalidState {
                    message: error.to_string(),
                })?;
        }
        let apply = self.apply_saved_input_owner(epoch);
        let publish = self.publish_owner_preferences();
        self.apply_hotkeys(&crate::hotkeys::configured(self.settings().hotkeys));
        self.emit(crate::api::ApiEvent::InputSettings(crate::agent_api::input_settings(
            self,
        )));
        self.emit_group();
        apply
            .and(publish.map(|_| ()))
            .map_err(|reason| crate::api::ApiError::InvalidState {
                message: format!("Settings were saved; owner application failed: {reason:?}"),
            })
    }
    pub(crate) fn persist_keyboard_safety_release(&self) {
        {
            let mut state = lock(&self.inner.controls);
            if state.keyboard_safety_pending {
                return;
            }
            state.keyboard_safety_pending = true;
        }
        let node = self.clone();
        self.inner.rt.spawn_blocking(move || {
            let _gate = node.owner_gate();
            let saved = {
                let mut config = lock(&node.inner.config);
                config
                    .owner()
                    .revision(dto::LocalOption::KeyboardDisabled)
                    .ok_or(Failure::UnsupportedOption)
                    .and_then(|revision| config.edit_owner(dto::LocalOption::KeyboardDisabled, revision, false.write()))
            };
            let published = node.publish_owner_preferences();
            if saved.is_ok() {
                lock(&node.inner.controls).keyboard_safety_pending = false;
            }
            if saved.is_err() || published.is_err() {
                tracing::warn!("keyboard safety release persistence/publication failed");
            }
            node.emit(crate::api::ApiEvent::Pause(node.pause_status()));
            node.refresh_device_cards();
        });
    }
    pub(crate) fn seed_live_screen_preferences(
        &self,
        displays: &[oink_foundation::display::DisplayInfo],
    ) -> Result<(), Failure> {
        let _gate = self.owner_gate();
        let changed = {
            let mut config = lock(&self.inner.config);
            let upserts: Vec<_> = displays
                .iter()
                .filter(|display| {
                    !config
                        .settings
                        .screen_preferences
                        .screens
                        .iter()
                        .any(|screen| screen.display == display.key)
                })
                .map(|display| {
                    oink_protocol::v1::RawValue::Map(
                        [
                            (1, oink_protocol::v1::RawValue::Text(display.key.clone())),
                            (2, 1f64.write()),
                            (3, (!display.is_virtual).write()),
                        ]
                        .into(),
                    )
                })
                .collect();
            if upserts.is_empty() {
                false
            } else {
                let patch = oink_protocol::v1::RawValue::Map(
                    [(
                        1,
                        oink_protocol::v1::RawValue::Map([(1, oink_protocol::v1::RawValue::Array(upserts))].into()),
                    )]
                    .into(),
                );
                let revision = config
                    .owner()
                    .revision(dto::LocalOption::ScreenPreferences)
                    .ok_or(Failure::UnsupportedOption)?;
                let mut candidate = config.owner().clone();
                candidate.edit(dto::LocalOption::ScreenPreferences, revision, patch.clone())?;
                self.preflight_owner_preferences(candidate.preferences())?;
                config.edit_owner(dto::LocalOption::ScreenPreferences, revision, patch)?;
                true
            }
        };
        if changed {
            self.publish_owner_preferences()?;
        }
        Ok(())
    }
    pub(crate) fn preflight_owner_preferences(&self, preferences: Vec<dto::DevicePreference>) -> Result<(), Failure> {
        let config = self.link_config().map_err(|_| Failure::Offline)?;
        let mut facts = (*config.local_facts()).clone();
        // Name is both a durable option and a top-level display hint. Account
        // for both copies before the atomic settings transaction can commit.
        let name = preferences
            .iter()
            .find(|item| item.option() == Ok(dto::LocalOption::DeviceName))
            .and_then(|item| item.value().ok());
        facts
            .edit(|raw| {
                raw.insert(18, List::<dto::DevicePreference, 32>(preferences).write());
                if let Some(name) = name {
                    raw.insert(6, name);
                }
            })
            .map_err(|_| Failure::InvalidValue)?;
        facts.encode(128 * 1024).map_err(|_| Failure::InvalidValue)?;
        Ok(())
    }
    pub(crate) fn publish_owner_preferences(&self) -> Result<u64, Failure> {
        let preferences = lock(&self.inner.config).owner().preferences();
        let pause = lock(&self.inner.controls).pause.state();
        self.link_config()
            .map_err(|_| Failure::Offline)?
            .publish_local_owner(preferences, pause)
            .map_err(|_| Failure::ApplyFailed)
    }
    pub(crate) fn apply_saved_input_owner(&self, expected_emergency_sequence: u64) -> Result<(), Failure> {
        let settings = self.settings();
        let input = self.input_thread().map_err(|_| Failure::ApplyFailed)?;
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        input.send(InputCommand::ApplyOwnerPreferences {
            routing: settings.routing.clone(),
            keyboard_disabled: settings.keyboard_disabled,
            accept_contacts: settings.accept_contacts,
            input_transform: settings.input_transform.clone(),
            expected_emergency_sequence,
            reply,
        });
        let applied = result
            .recv_timeout(std::time::Duration::from_secs(3))
            .map_err(|_| Failure::ApplyFailed)??;
        if applied.cleanup_pending {
            return Err(Failure::ApplyFailed);
        }
        if let Some(clip) = self.clipboard() {
            clip.set_enabled(settings.clipboard_enabled);
        }
        self.publish_local_options();
        Ok(())
    }
    pub(crate) fn apply_option_request(
        &self,
        requester: super::transactions::Requester,
        group: oink_foundation::GroupId,
        request: dto::SetLocalOption,
    ) -> Result<Message, Failure> {
        let option = field(request.option())?;
        let expected = field(request.expected_revision())?;
        let patch = field(request.value())?;
        let (item, outcome, reason, sequence, persisted) = if matches!(
            option,
            dto::LocalOption::ForwardingEnabled
                | dto::LocalOption::ForwardingControls
                | dto::LocalOption::AudioExitClaim
        ) {
            let audio = self.inner.audio.get().ok_or(Failure::Offline)?;
            let item = if option == dto::LocalOption::AudioExitClaim {
                let value = bool::read(&patch).map_err(|_| Failure::InvalidValue)?;
                self.inner
                    .rt
                    .block_on(audio.set_claim(super::runtime::boot(requester), group, value, expected))?
            } else {
                self.inner.rt.block_on(audio.set_option(
                    super::runtime::boot(requester),
                    group,
                    option,
                    expected,
                    patch,
                ))?
            };
            (
                item.current,
                item.outcome,
                item.reason,
                item.fact_sequence,
                item.persisted,
            )
        } else {
            let _gate = self.owner_gate();
            self.owner_authority(requester, group, &Message::ConfigSetLocalOption(request.clone()))?;
            let emergency_sequence = self
                .input_status()
                .map_err(|_| Failure::ApplyFailed)?
                .emergency_sequence;
            let item = {
                let mut config = lock(&self.inner.config);
                let mut candidate = config.owner().clone();
                candidate.edit(option, expected, patch.clone())?;
                self.preflight_owner_preferences(candidate.preferences())?;
                config.edit_owner(option, expected, patch)?
            };
            // Both Shared and Disabled leave the screen's Locked menu mode.
            // The command already reached the screen owner; release that owner's
            // pin even when the durable preference is unchanged. The input fence
            // publishes the unlock tombstone before the typed owner receipt.
            let released = if option == dto::LocalOption::ScreenShared {
                self.input_thread().map_err(|_| Failure::ApplyFailed).and_then(|input| {
                    let (reply, result) = std::sync::mpsc::sync_channel(1);
                    input.send(InputCommand::ReleaseOwnedFocusLock { reply });
                    let applied = result
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .map_err(|_| Failure::ApplyFailed)??;
                    if applied.cleanup_pending {
                        Err(Failure::ApplyFailed)
                    } else {
                        Ok(())
                    }
                })
            } else {
                Ok(())
            };
            let applied = released.and_then(|()| self.apply_saved_input_owner(emergency_sequence));
            let publication = self.publish_owner_preferences();
            let failure = applied.err().or_else(|| publication.as_ref().err().copied());
            let outcome = if failure.is_some() {
                dto::ControlOutcome::Failed
            } else if field(item.revision())? == expected {
                dto::ControlOutcome::NoChange
            } else {
                dto::ControlOutcome::Applied
            };
            (item, outcome, failure.unwrap_or(Failure::None), publication.ok(), true)
        };
        let target = super::requests::wire_boot(crate::device_cards::OwnerVersion {
            group: oink_foundation::GroupId([0; 16]).into(),
            device: self.id().into(),
            boot_nonce: self.inner.boot.nonce,
            revision: 1,
        });
        let raw: RawMap = [
            (1, field(request.request_id())?.write()),
            (2, target.write()),
            (3, outcome.write()),
            (4, reason.write()),
            (5, current(&item)?.write()),
            (6, sequence.write()),
            (7, persisted.write()),
        ]
        .into_iter()
        .collect();
        dto::LocalEditResult::from_raw(raw)
            .map(Message::ConfigLocalEditResult)
            .map_err(|_| Failure::InvalidValue)
    }
    pub(crate) fn apply_pause_request(
        &self,
        requester: super::transactions::Requester,
        group: oink_foundation::GroupId,
        request: dto::PauseDevice,
    ) -> Result<Message, Failure> {
        let _gate = self.owner_gate();
        self.owner_authority(requester, group, &Message::ConfigPauseDevice(request.clone()))?;
        let remote = requester.device.0 != self.id();
        let paused = field(request.paused())?;
        self.consume_local_emergency();
        let observed = self.input_status().map_err(|_| Failure::ApplyFailed)?;
        let mut candidate = lock(&self.inner.controls).pause.clone();
        let previous = candidate.revision();
        let outcome = candidate.command(field(request.expected_revision())?, paused, remote)?;
        let input = self.input_thread().map_err(|_| Failure::ApplyFailed)?;
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        input.send(InputCommand::ApplyOwnerPause {
            paused,
            expected_emergency_sequence: observed.emergency_sequence,
            reply,
        });
        let applied = result
            .recv_timeout(std::time::Duration::from_secs(3))
            .map_err(|_| Failure::OutcomeUnknown)??;
        self.consume_local_emergency();
        {
            let mut state = lock(&self.inner.controls);
            if state.pause.revision() != previous || applied.emergency_sequence != observed.emergency_sequence {
                return Err(Failure::StaleRevision);
            }
            state.pause = candidate;
        }
        if let Some(clip) = self.clipboard() {
            clip.set_paused(paused);
        }
        if let Some(diagnostics) = self.inner.diagnostics.get() {
            diagnostics.recorder.set_suspended(paused);
        }
        let publication = self.publish_owner_preferences();
        let failure = if applied.cleanup_pending {
            Some(Failure::ApplyFailed)
        } else {
            None
        }
        .or_else(|| publication.as_ref().err().copied());
        let raw: RawMap = [
            (1, field(request.request_id())?.write()),
            (2, field(request.target())?.write()),
            (
                3,
                if failure.is_some() {
                    dto::ControlOutcome::Failed
                } else {
                    outcome
                }
                .write(),
            ),
            (4, failure.unwrap_or(Failure::None).write()),
            (5, lock(&self.inner.controls).pause.state().write()),
            (6, publication.ok().write()),
        ]
        .into_iter()
        .collect();
        dto::PauseDeviceResult::from_raw(raw)
            .map(Message::ConfigPauseDeviceResult)
            .map_err(|_| Failure::InvalidValue)
    }
}
