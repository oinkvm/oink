//! Microphone ownership is serialized by the existing audio control worker.
use super::*;
use dto::{ControlFailure as Failure, LocalOption};

#[derive(Default)]
pub(super) struct State {
    pub native: bool,
    #[cfg(any(target_os = "macos", windows))]
    runtime: Option<Runtime>,
    group: Option<GroupId>,
    claims: Claims,
    pub resolution: Resolution,
}
impl Owner {
    fn microphone_members(&self) -> BTreeSet<DeviceId> {
        self.members()
            .into_iter()
            .filter(|peer| *peer == self.links.id() || self.group.peer_supports(*peer, "oink.audio.microphone/1"))
            .collect()
    }
    pub(super) fn apply_microphone(&mut self) -> Result<(), Failure> {
        let group = lock(&self.store).owner().active_group();
        if self.microphone.group != group {
            self.microphone.group = group;
            self.microphone.resolution = Resolution::Unknown;
            self.microphone.claims = if let Some(group) = group {
                let saved = lock(&self.store).owner().microphone_claims(group);
                let mut claims = Claims::restore(
                    saved.records.into_iter().map(|r| Record {
                        owner: r.owner,
                        revision: r.revision,
                        claim: r.claim,
                    }),
                    saved.seen_counter,
                )?;
                if let Some(watermark) = saved.watermark {
                    claims.observe_watermark(watermark)?;
                }
                claims
            } else {
                Claims::default()
            };
        }
        let Some(group) = group else { return Ok(()) };
        let members = self.microphone_members();
        let mut available = BTreeSet::new();
        let mut candidate = self.microphone.claims.clone();
        let mut changed = false;
        for member in &self.group.members().members {
            if !members.contains(&member.device) {
                continue;
            }
            let facts = if member.is_self {
                Some(self.group.local_facts())
            } else {
                member.facts.clone()
            };
            let Some(facts) = facts else { continue };
            if facts.group().ok().map(|g| g.0) != Some(group.0)
                || facts.boot().ok().as_ref() != self.current_boot(member.device).as_ref()
            {
                continue;
            }
            let Some(item) = facts.preferences().ok().flatten().and_then(|items| {
                items
                    .0
                    .into_iter()
                    .find(|p| p.option() == Ok(LocalOption::MicrophoneShareClaim))
            }) else {
                continue;
            };
            changed |= candidate.observe(
                member.device,
                Record {
                    owner: member.device,
                    revision: item.revision().map_err(|_| Failure::InvalidValue)?,
                    claim: Option::<dto::AudioExitClaim>::read(&item.value().map_err(|_| Failure::InvalidValue)?)
                        .map_err(|_| Failure::InvalidValue)?,
                },
            )?;
            if let Some(watermark) = facts.microphone_claim_watermark().map_err(|_| Failure::InvalidValue)? {
                changed |= candidate.observe_watermark(watermark)?;
            }
            if facts.observed().is_ok_and(|o| o.awake() == Ok(true))
                && facts.pause().ok().flatten().is_some_and(|p| p.paused() == Ok(false))
            {
                available.insert(member.device);
            }
        }
        let local = lock(&self.store)
            .owner()
            .item(LocalOption::MicrophoneShareClaim)
            .ok_or(Failure::StateConflict)?;
        changed |= candidate.observe(
            self.links.id(),
            Record {
                owner: self.links.id(),
                revision: local.revision().map_err(|_| Failure::InvalidValue)?,
                claim: Option::<dto::AudioExitClaim>::read(&local.value().map_err(|_| Failure::InvalidValue)?)
                    .map_err(|_| Failure::InvalidValue)?,
            },
        )?;
        if !self.paused && !self.power_state.sleeping {
            available.insert(self.links.id());
        }
        if let Some(record) = candidate.yield_to_winner(self.links.id())? {
            changed |= candidate.observe(self.links.id(), record)?;
        }
        if changed {
            self.persist_microphone_claims(candidate, false)?;
            self.publish_preferences()?;
        }
        self.microphone.resolution = self.microphone.claims.resolve(&members, &available);
        Ok(())
    }
    fn persist_microphone_claims(&mut self, claims: Claims, release: bool) -> Result<(), Failure> {
        let group = self.microphone.group.ok_or(Failure::NotMember)?;
        let records = GroupClaims {
            seen_counter: claims.seen_counter(),
            watermark: claims.watermark().cloned(),
            records: claims
                .records()
                .map(|r| ClaimRecord {
                    owner: r.owner,
                    revision: r.revision,
                    claim: r.claim.clone(),
                })
                .collect(),
        };
        let mut candidate = lock(&self.store).owner().clone();
        let commit = |owner: &mut crate::config::owner::OwnerStore| {
            if release {
                owner.release_microphone_claim(group, self.links.id(), records.clone())
            } else {
                owner.commit_microphone_claims(group, self.links.id(), records.clone())
            }
        };
        commit(&mut candidate).map_err(|_| Failure::InvalidValue)?;
        let mut facts = (*self.group.local_facts()).clone();
        facts
            .edit(|raw| {
                raw.insert(
                    18,
                    RawValue::Array(candidate.preferences().iter().map(WireValue::write).collect()),
                );
                raw.insert(24, claims.watermark().cloned().write());
            })
            .map_err(|_| Failure::InvalidValue)?;
        v1::frame::encode(&Message::ConfigFacts(facts), v1::limits::CONFIG_FRAME).map_err(|_| Failure::InvalidValue)?;
        lock(&self.store)
            .update_owner(commit)
            .map_err(|_| Failure::PersistenceFailed)?;
        self.microphone.claims = claims;
        Ok(())
    }
    pub(super) fn set_microphone_option(
        &mut self,
        option: LocalOption,
        expected: u64,
        patch: RawValue,
    ) -> Result<OwnerOptionApplied, Failure> {
        self.apply_microphone()?;
        let before = lock(&self.store)
            .owner()
            .item(option)
            .ok_or(Failure::UnsupportedOption)?;
        if before.revision() != Ok(expected) {
            return Err(Failure::StaleRevision);
        }
        if option == LocalOption::MicrophoneShareClaim {
            let claimed = bool::read(&patch).map_err(|_| Failure::InvalidValue)?;
            if claimed
                && (!self.microphone.native
                    || !matches!(
                        oink_audio::microphone::permission(),
                        oink_audio::microphone::Permission::Granted
                            | oink_audio::microphone::Permission::SystemControlled
                    ))
            {
                return Err(Failure::ApplyFailed);
            }
            if let Some(record) =
                self.microphone
                    .claims
                    .command(self.links.id(), expected, claimed, &self.microphone_members())?
            {
                let mut candidate = self.microphone.claims.clone();
                candidate.observe(self.links.id(), record)?;
                self.persist_microphone_claims(candidate, !claimed)?;
            }
        } else {
            lock(&self.store).edit_owner(option, expected, patch)?;
        }
        let published = self.publish_preferences().and_then(|()| self.apply_microphone());
        Ok(self.option_receipt(option, &before, published))
    }
}

#[cfg(any(target_os = "macos", windows))]
mod runtime;
#[cfg(any(target_os = "macos", windows))]
use runtime::Runtime;
impl Owner {
    pub(super) fn microphone_inputs(&self) -> Vec<oink_audio::microphone::InputDevice> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.selectable_inputs();
        }
        Vec::new()
    }
    pub(super) fn microphone_active(&self) -> bool {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.active();
        }
        false
    }
    pub(super) fn output_quality_snapshot(
        &self,
        at_ms: u64,
    ) -> [Option<oink_audio::sink::DropoutBucket>; oink_audio::sink::QUALITY_SECONDS] {
        let mut output = self.output_quality.snapshot(at_ms);
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            for bucket in runtime.output_quality(at_ms).into_iter().flatten() {
                let index = (bucket.at_ms / 1000) as usize % output.len();
                if let Some(old) = output[index].as_mut().filter(|old| old.at_ms == bucket.at_ms) {
                    old.add(bucket);
                } else {
                    output[index] = Some(bucket);
                }
            }
        }
        output
    }
    pub(super) fn microphone_quality(&self, now: Instant) -> Option<crate::audio_quality::Observation> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.quality(now);
        }
        None
    }
    pub(super) fn microphone_deadline(&self) -> Option<Instant> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.deadline();
        }
        None
    }
    pub(super) fn tick_microphone(&mut self) {
        if self.driver_change_active() {
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        if self.microphone.native {
            let mut runtime = self.microphone.runtime.take().unwrap_or_default();
            runtime.tick(self);
            self.microphone.runtime = Some(runtime);
        }
    }
    pub(super) fn refresh_microphone(&mut self) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(mut runtime) = self.microphone.runtime.take() {
            runtime.refresh(self);
            self.microphone.runtime = Some(runtime);
        }
    }
    pub(super) fn stop_microphone(&mut self) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(mut runtime) = self.microphone.runtime.take() {
            runtime.stop(self);
            self.microphone.runtime = Some(runtime);
        }
    }
    pub(super) fn microphone_message(&mut self, peer: DeviceId, message: &Message) {
        if self.driver_change_active() {
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        if self.microphone.native {
            let mut runtime = self.microphone.runtime.take().unwrap_or_default();
            runtime.message(self, peer, message);
            self.microphone.runtime = Some(runtime);
        }
    }
}
