//! The local Facts owner edits one retained tree. Resource owners replace only
//! their fields, preserving other observations and monotonically sequencing sends.
use oink_foundation::{GroupId, session::Boot};
use oink_network::{negotiation::OfferProfile, wire_identity as wire};
use oink_protocol::v2::{
    self, RawValue, dto,
    schema::{WireDto, WireValue},
};
/// Telemetry and its sequence do not change configuration, historical metadata,
/// addresses or layout diagnostics. Compare borrowed fields, including unknown
/// keys, rather than hashing/encoding/cloning the retained CBOR tree.
pub(super) fn same_configuration(before: &dto::DeviceFacts, after: &dto::DeviceFacts) -> bool {
    let configuration = |key: &&u32| !matches!(**key, 2 | 15);
    before
        .raw()
        .iter()
        .filter(|(key, _)| configuration(key))
        .eq(after.raw().iter().filter(|(key, _)| configuration(key)))
}

#[derive(Clone, Debug)]
pub(super) struct Facts {
    value: dto::DeviceFacts,
}
impl Facts {
    pub fn new(boot: Boot, group: GroupId, name: &str, os: &str, profile: &OfferProfile) -> v2::Result<Self> {
        let capabilities = dto::FactCapabilities::from_raw(
            [
                (
                    1,
                    RawValue::Array(
                        profile
                            .capabilities
                            .iter()
                            .map(|s| RawValue::from(s.as_str()))
                            .collect(),
                    ),
                ),
                (2, dto::CapturePermission::Indeterminate.write()),
                (3, RawValue::Array(profile.audio.iter().map(WireValue::write).collect())),
                (4, RawValue::Array(Vec::new())),
            ]
            .into(),
        )?;
        let options = dto::LocalOptionsSummary::from_raw(
            [
                (1, true.write()),
                (3, 1f32.write()),
                (4, false.write()),
                (5, false.write()),
                (6, false.write()),
            ]
            .into(),
        )?;
        let value = dto::DeviceFacts::from_raw(
            [
                (1, wire::boot(boot).write()),
                (2, 1u64.write()),
                (
                    3,
                    RawValue::Map([(1, v2::MAJOR.write()), (2, v2::MINOR.write())].into()),
                ),
                (4, RawValue::from(env!("OINK_RELEASE_VERSION"))),
                (5, RawValue::Bytes(group.0.to_vec())),
                (6, RawValue::from(name)),
                (7, RawValue::from(os)),
                (8, RawValue::from(crate::agent_api::local_os_version().as_str())),
                (9, RawValue::from(env!("OINK_RELEASE_VERSION"))),
                (10, RawValue::Array(Vec::new())),
                (
                    11,
                    dto::ObservedState::from_raw(
                        [
                            (1, true.write()),
                            (2, dto::RunIntent::Running.write()),
                            (4, 0u64.write()),
                        ]
                        .into(),
                    )?
                    .write(),
                ),
                (12, RawValue::Array(Vec::new())),
                (13, capabilities.write()),
                (14, options.write()),
                (15, RawValue::Array(Vec::new())),
                (25, dto::DeviceKind::Desktop.write()),
                (26, v2::MAJOR.write()),
                (27, v2::MINOR.write()),
            ]
            .into(),
        )?;
        Ok(Self { value })
    }
    pub fn value(&self) -> &dto::DeviceFacts {
        &self.value
    }
    pub fn links(&mut self, links: &[oink_network::link::LinkEntry]) -> v2::Result<bool> {
        let links = links
            .iter()
            .map(|link| {
                let (path, hop) = match link.path {
                    oink_network::link::LinkPath::Direct => (dto::FactPath::Direct, None),
                    oink_network::link::LinkPath::Relayed { via } => (dto::FactPath::Relayed, Some(via)),
                };
                let mut raw: v2::RawMap = [
                    (1, wire::device(link.peer).write()),
                    (2, path.write()),
                    (5, (super::FACTS_INTERVAL.as_millis() as u32).write()),
                    (6, link.lost_packets.write()),
                    (7, 0u64.write()),
                    (8, link.low_available.write()),
                    (9, link.route_revision.write()),
                ]
                .into();
                if let Some(hop) = hop {
                    raw.insert(3, wire::device(hop).write());
                }
                if let Some(rtt) = link.rtt {
                    raw.insert(4, (rtt.as_micros().min(u64::MAX as u128) as u64).write());
                }
                dto::LinkFact::from_raw(raw).map(|fact| fact.write())
            })
            .collect::<v2::Result<Vec<_>>>()?;
        self.edit(|raw| {
            raw.insert(15, RawValue::Array(links));
            Ok(())
        })
    }
    pub fn edit(&mut self, edit: impl FnOnce(&mut v2::RawMap) -> v2::Result<()>) -> v2::Result<bool> {
        let mut raw = self.value.raw().clone();
        edit(&mut raw)?;
        if raw == *self.value.raw() {
            return Ok(false);
        }
        raw.insert(
            2,
            self.value
                .fact_sequence()?
                .checked_add(1)
                .ok_or(v2::ErrorCode::StateConflict)?
                .write(),
        );
        let candidate = dto::DeviceFacts::from_raw(raw)?;
        candidate.encode(v2::limits::CONFIG_FRAME)?;
        self.value = candidate;
        Ok(true)
    }
    pub fn owner(&mut self, preferences: Vec<dto::DevicePreference>, pause: dto::LocalPauseState) -> v2::Result<bool> {
        self.edit(|raw| {
            let mut preferences = preferences;
            preferences.sort_by_key(|p| p.option().map(|o| o.number()).unwrap_or(u32::MAX));
            raw.insert(18, RawValue::Array(preferences.iter().map(WireValue::write).collect()));
            raw.insert(19, pause.write());
            let mut options =
                dto::LocalOptionsSummary::read(raw.get(&14).ok_or(v2::ErrorCode::MalformedFrame)?)?.into_raw();
            options.insert(1, (!pause.paused()?).write());
            for preference in &preferences {
                match preference.option()? {
                    dto::LocalOption::DeviceName => {
                        raw.insert(6, preference.value()?);
                    }
                    dto::LocalOption::ForwardingControls => {
                        let controls = dto::ForwardingControlsValue::read(&preference.value()?)?;
                        options.insert(3, (f32::from(controls.volume_percent()?) / 100.).write());
                    }
                    _ => {}
                }
            }
            raw.insert(14, RawValue::Map(options));
            Ok(())
        })
    }
    /// A new group: this device has not merged with it yet, so it publishes
    /// no audio configuration until the audio owner merged (key 33 absent).
    pub fn group(&mut self, group: GroupId, owner: &crate::config::owner::OwnerStore) -> v2::Result<bool> {
        self.edit(|raw| {
            raw.insert(5, RawValue::Bytes(group.0.to_vec()));
            raw.insert(
                18,
                RawValue::Array(owner.preferences().iter().map(WireValue::write).collect()),
            );
            raw.remove(&AUDIO_CONFIG);
            Ok(())
        })
    }
    /// Publish the global audio configuration this device executes (its own
    /// entry, its linked devices and both registers); `None` while unjoined.
    pub fn audio_config(&mut self, config: Option<&oink_audio::route::AudioConfig>) -> v2::Result<bool> {
        self.edit(|raw| {
            match config {
                Some(config) => {
                    raw.insert(AUDIO_CONFIG, config.to_wire().write());
                }
                None => {
                    raw.remove(&AUDIO_CONFIG);
                }
            }
            Ok(())
        })
    }
    pub fn clipboard(&mut self, mode: Option<dto::SharingMode>, fault: Option<dto::ResourceFault>) -> v2::Result<bool> {
        self.edit(|raw| {
            let mut resources = raw
                .get(&16)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            if let Some(mode) = mode {
                resources.insert(1, mode.write());
            } else {
                resources.remove(&1);
            }
            let mut faults = resources
                .get(&4)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            if let Some(fault) = fault {
                faults.insert(5, fault.write());
            } else {
                faults.remove(&5);
            }
            resources.insert(4, RawValue::Map(faults));
            raw.insert(16, RawValue::Map(resources));
            Ok(())
        })
    }
    pub fn microphone(&mut self, observation: dto::MicrophoneObservation) -> v2::Result<bool> {
        self.edit(|raw| {
            let mut resources = raw
                .get(&16)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            resources.insert(8, observation.write());
            raw.insert(16, RawValue::Map(resources));
            Ok(())
        })
    }
    pub fn audio(
        &mut self,
        permission: dto::CapturePermission,
        outputs: Vec<dto::AudioOutputFact>,
        speaker: dto::SpeakerObservation,
        fault: Option<dto::ResourceFault>,
        exit_volume: Option<dto::ExitVolumeState>,
    ) -> v2::Result<bool> {
        self.edit(|raw| {
            let mut capabilities =
                dto::FactCapabilities::read(raw.get(&13).ok_or(v2::ErrorCode::MalformedFrame)?)?.into_raw();
            capabilities.insert(2, permission.write());
            capabilities.insert(4, RawValue::Array(outputs.iter().map(WireValue::write).collect()));
            raw.insert(13, RawValue::Map(capabilities));
            let mut resources = raw
                .get(&16)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            resources.insert(5, speaker.write());
            let mut faults = resources
                .get(&4)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            if let Some(fault) = fault {
                faults.insert(4, fault.write());
            } else {
                faults.remove(&4);
            }
            resources.insert(4, RawValue::Map(faults));
            raw.insert(16, RawValue::Map(resources));
            if let Some(volume) = exit_volume {
                raw.insert(17, volume.write());
            } else {
                raw.remove(&17);
            }
            Ok(())
        })
    }
    pub fn input(
        &mut self,
        mouse: dto::SharingMode,
        keyboard: dto::SharingMode,
        faults: dto::ResourceFaults,
        options: dto::LocalOptionsSummary,
        game: Option<dto::GameObservation>,
        accessibility_granted: Option<bool>,
    ) -> v2::Result<bool> {
        self.edit(|raw| {
            let mut resources = raw
                .get(&16)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            if let Some(granted) = accessibility_granted {
                raw.insert(23, granted.write());
            } else {
                raw.remove(&23);
            }
            resources.insert(2, mouse.write());
            resources.insert(3, keyboard.write());
            if let Some(game) = game {
                resources.insert(7, game.write());
            } else {
                resources.remove(&7);
            }
            let mut merged = resources
                .get(&4)
                .map(RawValue::as_map)
                .transpose()?
                .cloned()
                .unwrap_or_default();
            for key in [1, 2, 3] {
                if let Some(value) = faults.raw().get(&key) {
                    merged.insert(key, value.clone());
                } else {
                    merged.remove(&key);
                }
            }
            resources.insert(4, RawValue::Map(merged));
            raw.insert(16, RawValue::Map(resources));
            let mut current =
                dto::LocalOptionsSummary::read(raw.get(&14).ok_or(v2::ErrorCode::MalformedFrame)?)?.into_raw();
            for key in [4, 5, 6] {
                current.insert(
                    key,
                    options.raw().get(&key).ok_or(v2::ErrorCode::MalformedFrame)?.clone(),
                );
            }
            // Optional input observation: absent means unlocked or unknown.
            match options.raw().get(&7) {
                Some(value) if *value != RawValue::Null => {
                    current.insert(7, value.clone());
                }
                _ => {
                    current.remove(&7);
                }
            }
            raw.insert(14, RawValue::Map(current));
            Ok(())
        })
    }
}

/// DeviceFacts key of the global audio configuration.
const AUDIO_CONFIG: u32 = 33;

#[cfg(test)]
mod observation_tests {
    use super::*;

    #[test]
    fn telemetry_comparison_retains_unknown_and_configuration_fields() {
        let boot = Boot {
            device_id: oink_foundation::DeviceId([1; 32]),
            nonce: [2; 16],
        };
        let original = Facts::new(boot, GroupId([3; 16]), "owner", "macos", &OfferProfile::default()).unwrap();
        let mut telemetry = original.value().raw().clone();
        telemetry.insert(2, 99u64.write());
        telemetry.insert(
            15,
            RawValue::Array(vec![RawValue::Map(
                [
                    (1, wire::device(boot.device_id).write()),
                    (2, dto::FactPath::Direct.write()),
                    (4, 1000u64.write()),
                    (5, 5000u32.write()),
                    (6, 7u64.write()),
                    (7, 0u64.write()),
                    (8, true.write()),
                    (9, 1u64.write()),
                ]
                .into(),
            )]),
        );
        let telemetry = dto::DeviceFacts::from_raw(telemetry).unwrap();
        assert!(same_configuration(original.value(), &telemetry));
        for (key, value) in [
            (6, RawValue::from("new name")),
            (900, RawValue::from("unknown extension")),
        ] {
            let mut changed = telemetry.raw().clone();
            changed.insert(key, value);
            let changed = dto::DeviceFacts::from_raw(changed).unwrap();
            assert!(
                !same_configuration(original.value(), &changed),
                "field {key} cannot take the telemetry shortcut"
            );
        }
    }

    #[test]
    fn new_facts_include_required_v2_identity_fields() {
        let boot = Boot {
            device_id: oink_foundation::DeviceId([9; 32]),
            nonce: [8; 16],
        };
        let facts = Facts::new(boot, GroupId([7; 16]), "owner", "macos", &OfferProfile::default()).unwrap();
        assert_eq!(facts.value().device_kind().unwrap(), dto::DeviceKind::Desktop);
        assert_eq!(facts.value().protocol_major().unwrap(), v2::MAJOR);
        assert_eq!(facts.value().protocol_minor().unwrap(), v2::MINOR);
    }
}
