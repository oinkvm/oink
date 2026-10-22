//! The local Facts owner edits one retained tree. Resource owners replace only
//! their fields, preserving other observations and monotonically sequencing sends.
use oink_foundation::{GroupId, session::Boot};
use oink_network::{negotiation::OfferProfile, wire_identity as wire};
use oink_protocol::v1::{
    self, RawValue, dto,
    schema::{WireDto, WireValue},
};
#[derive(Clone, Debug)]
pub(super) struct Facts {
    value: dto::DeviceFacts,
}
impl Facts {
    pub fn new(boot: Boot, group: GroupId, name: &str, os: &str, profile: &OfferProfile) -> v1::Result<Self> {
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
                (2, false.write()),
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
                    RawValue::Map([(1, v1::MAJOR.write()), (2, v1::MINOR.write())].into()),
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
            ]
            .into(),
        )?;
        Ok(Self { value })
    }
    pub fn value(&self) -> &dto::DeviceFacts {
        &self.value
    }
    pub fn links(&mut self, links: &[oink_network::link::LinkEntry]) -> v1::Result<bool> {
        let links = links
            .iter()
            .map(|link| {
                let (path, hop) = match link.path {
                    oink_network::link::LinkPath::Direct => (dto::FactPath::Direct, None),
                    oink_network::link::LinkPath::Relayed { via } => (dto::FactPath::Relayed, Some(via)),
                };
                let mut raw: v1::RawMap = [
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
            .collect::<v1::Result<Vec<_>>>()?;
        self.edit(|raw| {
            raw.insert(15, RawValue::Array(links));
            Ok(())
        })
    }
    pub fn edit(&mut self, edit: impl FnOnce(&mut v1::RawMap) -> v1::Result<()>) -> v1::Result<bool> {
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
                .ok_or(v1::ErrorCode::StateConflict)?
                .write(),
        );
        let candidate = dto::DeviceFacts::from_raw(raw)?;
        candidate.encode(v1::limits::CONFIG_FRAME)?;
        self.value = candidate;
        Ok(true)
    }
    pub fn owner(
        &mut self,
        preferences: Vec<dto::DevicePreference>,
        pause: dto::LocalPauseState,
        watermark: Option<dto::AudioExitClaim>,
    ) -> v1::Result<bool> {
        self.edit(|raw| {
            let mut preferences = preferences;
            preferences.sort_by_key(|p| p.option().map(|o| o.number()).unwrap_or(u32::MAX));
            raw.insert(18, RawValue::Array(preferences.iter().map(WireValue::write).collect()));
            raw.insert(19, pause.write());
            raw.insert(21, watermark.write());
            let mut options =
                dto::LocalOptionsSummary::read(raw.get(&14).ok_or(v1::ErrorCode::MalformedFrame)?)?.into_raw();
            options.insert(1, (!pause.paused()?).write());
            for preference in &preferences {
                match preference.option()? {
                    dto::LocalOption::AudioExitClaim => {
                        let value = preference.value()?;
                        if value == RawValue::Null {
                            raw.remove(&20);
                        } else {
                            raw.insert(20, value);
                        }
                    }
                    dto::LocalOption::DeviceName => {
                        raw.insert(6, preference.value()?);
                    }
                    dto::LocalOption::ForwardingEnabled => {
                        options.insert(2, bool::read(&preference.value()?)?.write());
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
    pub fn group(
        &mut self,
        group: GroupId,
        owner: &crate::config::owner::OwnerStore,
        device: oink_foundation::DeviceId,
    ) -> v1::Result<bool> {
        let claims = owner.claims(group);
        let watermark = claims.watermark();
        let claim = claims
            .records
            .into_iter()
            .find(|r| r.owner == device)
            .and_then(|r| r.claim);
        self.edit(|raw| {
            raw.insert(5, RawValue::Bytes(group.0.to_vec()));
            raw.insert(21, watermark.write());
            raw.insert(
                18,
                RawValue::Array(owner.preferences().iter().map(WireValue::write).collect()),
            );
            if let Some(claim) = claim {
                raw.insert(20, claim.write());
            } else {
                raw.remove(&20);
            }
            Ok(())
        })
    }
    pub fn clipboard(&mut self, mode: Option<dto::SharingMode>, fault: Option<dto::ResourceFault>) -> v1::Result<bool> {
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
    pub fn audio(
        &mut self,
        permission: dto::CapturePermission,
        outputs: Vec<dto::AudioOutputFact>,
        speaker: dto::SpeakerObservation,
        fault: Option<dto::ResourceFault>,
        exit_volume: Option<dto::ExitVolumeState>,
    ) -> v1::Result<bool> {
        self.edit(|raw| {
            let mut capabilities =
                dto::FactCapabilities::read(raw.get(&13).ok_or(v1::ErrorCode::MalformedFrame)?)?.into_raw();
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
    ) -> v1::Result<bool> {
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
                dto::LocalOptionsSummary::read(raw.get(&14).ok_or(v1::ErrorCode::MalformedFrame)?)?.into_raw();
            for key in [4, 5, 6] {
                current.insert(
                    key,
                    options.raw().get(&key).ok_or(v1::ErrorCode::MalformedFrame)?.clone(),
                );
            }
            raw.insert(14, RawValue::Map(current));
            Ok(())
        })
    }
}

/// The v1 baseline knows mouse modes 0/1 only. A disabled source is unknown to
/// those peers, never falsely advertised as enabled/local. Keep unrelated facts.
pub(super) fn for_mouse_capability(facts: dto::DeviceFacts, supported: bool) -> v1::Result<dto::DeviceFacts> {
    if supported {
        return Ok(facts);
    }
    let mut raw = facts.into_raw();
    if let Some(RawValue::Array(preferences)) = raw.get_mut(&18) {
        preferences.retain(|p| !matches!(p, RawValue::Map(v) if v.get(&1) == Some(&RawValue::Uint(1)) && v.get(&3) == Some(&RawValue::Uint(2))));
    }
    if let Some(RawValue::Map(resources)) = raw.get_mut(&16)
        && resources.get(&2) == Some(&RawValue::Uint(2))
    {
        resources.remove(&2);
    }
    dto::DeviceFacts::from_raw(raw)
}

/// The optional observation travels in the existing ConfigFacts message. Strip
/// it on every send (initial handshake and later publications) to an old peer.
pub(super) fn for_local_output_capability(facts: dto::DeviceFacts, supported: bool) -> v1::Result<dto::DeviceFacts> {
    if supported {
        return Ok(facts);
    }
    let mut raw = facts.into_raw();
    if let Some(RawValue::Map(resources)) = raw.get_mut(&16)
        && let Some(RawValue::Map(speaker)) = resources.get_mut(&5)
    {
        speaker.remove(&9);
    }
    dto::DeviceFacts::from_raw(raw)
}

/// AX is an owner observation in ConfigFacts, never inferred from resource faults.
pub(super) fn for_accessibility_capability(facts: dto::DeviceFacts, supported: bool) -> v1::Result<dto::DeviceFacts> {
    if supported {
        return Ok(facts);
    }
    let mut raw = facts.into_raw();
    raw.remove(&23);
    dto::DeviceFacts::from_raw(raw)
}

#[cfg(test)]
mod mouse_mode_tests {
    use super::*;
    #[test]
    fn accessibility_facts_are_explicit_optional_and_capability_gated() {
        let mut facts = Facts::new(
            Boot {
                device_id: oink_foundation::DeviceId([1; 32]),
                nonce: [2; 16],
            },
            GroupId([3; 16]),
            "owner",
            "macos",
            &OfferProfile::default(),
        )
        .unwrap();
        assert_eq!(facts.value().accessibility_granted().unwrap(), None);
        for granted in [false, true] {
            facts
                .edit(|raw| {
                    raw.insert(23, granted.write());
                    Ok(())
                })
                .unwrap();
            let original = facts.value().clone();
            assert_eq!(for_accessibility_capability(original.clone(), true).unwrap(), original);
            let old = for_accessibility_capability(original.clone(), false).unwrap();
            assert_eq!(old.accessibility_granted().unwrap(), None);
            let mut expected = original.into_raw();
            expected.remove(&23);
            assert_eq!(old.raw(), &expected);
            let message = v1::Message::ConfigFacts(old);
            let encoded = v1::frame::encode(&message, v1::limits::CONFIG_FRAME).unwrap();
            assert_eq!(
                v1::frame::OpaqueFrame::parse_exact(&encoded, v1::limits::CONFIG_FRAME)
                    .unwrap()
                    .decode()
                    .unwrap(),
                message
            );
        }
    }

    #[test]
    fn local_output_facts_are_capability_gated_for_old_peers() {
        let mut facts = Facts::new(
            Boot {
                device_id: oink_foundation::DeviceId([1; 32]),
                nonce: [2; 16],
            },
            GroupId([3; 16]),
            "owner",
            "test",
            &OfferProfile::default(),
        )
        .unwrap();
        facts
            .edit(|raw| {
                raw.insert(
                    16,
                    RawValue::Map(
                        [(
                            5,
                            RawValue::Map(
                                [
                                    (2, dto::SpeakerActivity::Unused.write()),
                                    (9, RawValue::Map([(1, 61u8.write()), (2, false.write())].into())),
                                ]
                                .into(),
                            ),
                        )]
                        .into(),
                    ),
                );
                Ok(())
            })
            .unwrap();
        let original = facts.value().clone();
        assert_eq!(for_local_output_capability(original.clone(), true).unwrap(), original);
        let old = for_local_output_capability(original.clone(), false).unwrap();
        let speaker = old.resources().unwrap().unwrap().speaker().unwrap().unwrap();
        assert_eq!(speaker.local_output().unwrap(), None);
        assert_eq!(speaker.activity().unwrap(), Some(dto::SpeakerActivity::Unused));
        assert_eq!(old.fact_sequence().unwrap(), original.fact_sequence().unwrap());
        // The old-peer frame is still the normal v1 ConfigFacts, with no new field.
        let frame = v1::frame::encode(&v1::Message::ConfigFacts(old.clone()), v1::limits::CONFIG_FRAME).unwrap();
        assert_eq!(
            v1::frame::OpaqueFrame::parse_exact(&frame, v1::limits::CONFIG_FRAME)
                .unwrap()
                .decode()
                .unwrap(),
            v1::Message::ConfigFacts(old)
        );
    }
    #[test]
    fn disabled_mouse_facts_are_withheld_only_from_legacy_peers() {
        let mut facts = Facts::new(
            Boot {
                device_id: oink_foundation::DeviceId([1; 32]),
                nonce: [2; 16],
            },
            GroupId([3; 16]),
            "source",
            "test",
            &OfferProfile::default(),
        )
        .unwrap();
        facts
            .edit(|raw| {
                raw.insert(16, RawValue::Map([(2, 2u32.write()), (3, 0u32.write())].into()));
                raw.insert(
                    18,
                    RawValue::Array(
                        [1u32, 2]
                            .into_iter()
                            .map(|option| {
                                RawValue::Map(
                                    [
                                        (1, option.write()),
                                        (2, 1u64.write()),
                                        (3, (if option == 1 { 2u32 } else { 0 }).write()),
                                    ]
                                    .into(),
                                )
                            })
                            .collect(),
                    ),
                );
                Ok(())
            })
            .unwrap();
        let original = facts.value().clone();
        assert_eq!(
            for_mouse_capability(original.clone(), true).unwrap().raw(),
            original.raw()
        );
        let legacy = for_mouse_capability(original.clone(), false).unwrap();
        assert_eq!(legacy.resources().unwrap().unwrap().mouse_mode().unwrap(), None);
        assert_eq!(
            legacy.resources().unwrap().unwrap().keyboard_mode().unwrap(),
            Some(dto::SharingMode::Shared)
        );
        let preferences = legacy.preferences().unwrap().unwrap();
        assert_eq!(preferences.0.len(), 1);
        assert_eq!(preferences.0[0].option().unwrap(), dto::LocalOption::KeyboardMode);
        assert_eq!(legacy.fact_sequence().unwrap(), original.fact_sequence().unwrap());
    }
}
