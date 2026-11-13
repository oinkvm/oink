//! Route- and Boot-fenced audio preparation, independent of devices and transport.
//! The caller supplies current authenticated Boots, the resolved exit route
//! version (after checking the receiver is that route's target) and the
//! locally bound output. Cached/offline identity is never authorization here.
use crate::negotiation::Profile;
use oink_protocol::v2::{
    RawMap,
    dto::{
        AudioPrepared, AudioPreparedAck, AudioRouteVersion, AudioSessionContext, Boot, ControlFailure, ExitVolumeState,
        ReceiverOutputBinding, SourceRouteOffer, TypedResult,
    },
    schema::{WireDto, WireValue},
};
use std::sync::atomic::{AtomicU64, Ordering};

// This survives an audio-worker restart in the same process/Boot. Reusing an
// output, offer or stream generation could otherwise revive queued old media.
static GENERATION: AtomicU64 = AtomicU64::new(1);
pub(crate) fn next_generation() -> Result<u64, ControlFailure> {
    GENERATION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
        .map_err(|_| ControlFailure::StateConflict)
}
/// Physical volume keys have their own context, independent of source capture
/// and M0. A self-exit context is legal here because it never captures its mix.
pub fn volume_context(source: &Boot, state: &ExitVolumeState) -> Result<AudioSessionContext, ControlFailure> {
    let version = state.version().map_err(|_| ControlFailure::InvalidValue)?;
    let output = version.output_id().map_err(|_| ControlFailure::InvalidValue)?;
    if output.is_none() {
        return Err(ControlFailure::OutputUnavailable);
    }
    let binding = ReceiverOutputBinding::from_raw(
        [
            (1, version.exit().map_err(|_| ControlFailure::InvalidValue)?.write()),
            (
                2,
                version
                    .output_generation()
                    .map_err(|_| ControlFailure::InvalidValue)?
                    .write(),
            ),
            (3, output.write()),
        ]
        .into(),
    )
    .map_err(|_| ControlFailure::InvalidValue)?;
    AudioSessionContext::from_raw(
        [
            (1, source.write()),
            (2, next_generation()?.write()),
            (3, binding.write()),
            (
                4,
                version
                    .route_version()
                    .map_err(|_| ControlFailure::InvalidValue)?
                    .write(),
            ),
        ]
        .into(),
    )
    .map_err(|_| ControlFailure::InvalidValue)
}
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub profile: Profile,
    pub wire: AudioPrepared,
}
impl Prepared {
    pub fn stream_id(&self) -> u64 {
        self.wire.stream_id().expect("validated preparation")
    }
    pub fn session(&self) -> AudioSessionContext {
        self.wire.session().expect("validated preparation")
    }
}
#[derive(Debug)]
pub struct Source {
    boot: Boot,
    offer: Option<SourceRouteOffer>,
    prepared: Option<Prepared>,
}
impl Source {
    pub fn new(boot: Boot) -> Self {
        Self {
            boot,
            offer: None,
            prepared: None,
        }
    }
    pub fn retire(&mut self) {
        self.offer = None;
        self.prepared = None;
    }
    pub fn offer(&self) -> Option<&SourceRouteOffer> {
        self.offer.as_ref()
    }
    pub fn prepared(&self) -> Option<&Prepared> {
        self.prepared.as_ref()
    }
    pub fn begin(&mut self, receiver: Boot, version: AudioRouteVersion) -> Result<&SourceRouteOffer, ControlFailure> {
        if receiver.device_id() == self.boot.device_id() {
            return Err(ControlFailure::NotExit);
        }
        let offer = SourceRouteOffer::from_raw(RawMap::from([
            (1, self.boot.write()),
            (2, next_generation()?.write()),
            (3, receiver.write()),
            (4, version.write()),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)?;
        self.prepared = None;
        self.offer = Some(offer);
        Ok(self.offer.as_ref().expect("installed offer"))
    }
    pub fn set_profile(&mut self, profile: Profile) -> Result<&SourceRouteOffer, ControlFailure> {
        let offer = self.offer.as_mut().ok_or(ControlFailure::RouteChanged)?;
        offer
            .edit(|raw| {
                raw.insert(5, profile.descriptor().write());
            })
            .map_err(|_| ControlFailure::InvalidValue)?;
        Ok(offer)
    }
    pub fn accept(&mut self, sender: &Boot, wire: AudioPrepared) -> Result<AudioPreparedAck, ControlFailure> {
        let offer = self.offer.as_ref().ok_or(ControlFailure::RouteChanged)?;
        if offer.receiver().as_ref() != Ok(sender) {
            return Err(ControlFailure::StaleBoot);
        }
        let session = wire.session().map_err(|_| ControlFailure::InvalidValue)?;
        let output = session.receiver_output().map_err(|_| ControlFailure::InvalidValue)?;
        if session.source() != offer.source()
            || session.source_route_generation() != offer.source_route_generation()
            || session.route_version() != offer.route_version()
        {
            return Err(ControlFailure::RouteChanged);
        }
        if output.receiver().as_ref() != Ok(sender) {
            return Err(ControlFailure::StaleBoot);
        }
        if output.output_id().map_err(|_| ControlFailure::InvalidValue)?.is_none() {
            return Err(ControlFailure::OutputUnavailable);
        }
        let profile = Profile::from_descriptor(&wire.descriptor().map_err(|_| ControlFailure::InvalidValue)?)?;
        let requested = offer
            .preferred_descriptor()
            .map_err(|_| ControlFailure::InvalidValue)?
            .map(|descriptor| Profile::from_descriptor(&descriptor))
            .transpose()?
            .unwrap_or(Profile::Opus5ms);
        if profile != requested && profile != Profile::Opus5ms {
            return Err(ControlFailure::Unsupported);
        }

        let stream = wire.stream_id().map_err(|_| ControlFailure::InvalidValue)?;
        if stream == 0 {
            return Err(ControlFailure::InvalidValue);
        }
        if let Some(previous) = &self.prepared {
            if stream < previous.stream_id() {
                return Err(ControlFailure::RouteChanged);
            }
            if stream == previous.stream_id() && previous.wire != wire {
                return Err(ControlFailure::StateConflict);
            }
            if output.generation().map_err(|_| ControlFailure::InvalidValue)?
                < previous
                    .session()
                    .receiver_output()
                    .map_err(|_| ControlFailure::InvalidValue)?
                    .generation()
                    .map_err(|_| ControlFailure::InvalidValue)?
            {
                return Err(ControlFailure::OutputChanged);
            }
        }
        self.prepared = Some(Prepared { profile, wire });
        AudioPreparedAck::from_raw(RawMap::from([
            (1, stream.write()),
            (
                2,
                TypedResult::from_raw([(1, 0_u64.write())].into())
                    .expect("success result")
                    .write(),
            ),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)
    }
}
#[derive(Debug)]
pub struct Receiver {
    offer: SourceRouteOffer,
    prepared: Prepared,
    acknowledged: bool,
}
impl Receiver {
    pub fn prepare(
        offer: SourceRouteOffer,
        source: &Boot,
        version: &AudioRouteVersion,
        output: &ReceiverOutputBinding,
        profile: Profile,
    ) -> Result<Self, ControlFailure> {
        if offer.source().as_ref() != Ok(source) {
            return Err(ControlFailure::StaleBoot);
        }
        if offer.route_version().as_ref() != Ok(version) {
            return Err(ControlFailure::RouteChanged);
        }
        let receiver = output.receiver().map_err(|_| ControlFailure::InvalidValue)?;
        if offer.receiver() != Ok(receiver.clone()) {
            return Err(ControlFailure::StaleBoot);
        }
        if receiver.device_id() == source.device_id() {
            return Err(ControlFailure::NotExit);
        }
        if output.output_id().map_err(|_| ControlFailure::InvalidValue)?.is_none() {
            return Err(ControlFailure::OutputUnavailable);
        }
        let session = AudioSessionContext::from_raw(RawMap::from([
            (1, source.write()),
            (
                2,
                offer
                    .source_route_generation()
                    .map_err(|_| ControlFailure::InvalidValue)?
                    .write(),
            ),
            (3, output.write()),
            (4, version.write()),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)?;
        let wire = AudioPrepared::from_raw(RawMap::from([
            (1, session.write()),
            (2, next_generation()?.write()),
            (3, profile.descriptor().write()),
        ]))
        .map_err(|_| ControlFailure::InvalidValue)?;
        Ok(Self {
            offer,
            prepared: Prepared { profile, wire },
            acknowledged: false,
        })
    }
    pub fn offer(&self) -> &SourceRouteOffer {
        &self.offer
    }
    pub fn prepared(&self) -> &Prepared {
        &self.prepared
    }
    pub fn ready(&self) -> bool {
        self.acknowledged
    }
    pub fn acknowledge(&mut self, source: &Boot, ack: &AudioPreparedAck) -> Result<(), ControlFailure> {
        if self.offer.source().as_ref() != Ok(source) {
            return Err(ControlFailure::StaleBoot);
        }
        if ack.stream_id() != Ok(self.prepared.stream_id()) {
            return Err(ControlFailure::RouteChanged);
        }
        if !ack.result().map_err(|_| ControlFailure::InvalidValue)?.is_success() {
            return Err(ControlFailure::Unsupported);
        }
        self.acknowledged = true;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::Version;
    use oink_foundation::DeviceId;
    fn route_version(author: u8, stamp: u64) -> AudioRouteVersion {
        Version {
            author: DeviceId([author; 32]),
            stamp,
        }
        .to_wire()
    }
    fn boot(owner: u8, nonce: u8) -> Boot {
        Boot::from_raw(
            [
                (1, route_version(owner, 1).author().unwrap().write()),
                (2, oink_protocol::v2::RawValue::Bytes(vec![nonce; 16])),
            ]
            .into(),
        )
        .unwrap()
    }
    fn output(owner: u8, nonce: u8, generation: u64) -> ReceiverOutputBinding {
        ReceiverOutputBinding::from_raw(
            [
                (1, boot(owner, nonce).write()),
                (2, generation.write()),
                (3, "synthetic-output".into()),
            ]
            .into(),
        )
        .unwrap()
    }
    #[test]
    fn physical_volume_context_exists_without_media_and_allows_self_exit() {
        use oink_protocol::v2::dto::{ExitVolumeStatus, ExitVolumeVersion};
        let version = ExitVolumeVersion::from_raw(
            [
                (1, boot(2, 1).write()),
                (3, "synthetic-output".into()),
                (4, 7_u64.write()),
                (5, 1_u64.write()),
                (6, route_version(2, 8).write()),
            ]
            .into(),
        )
        .unwrap();
        let state = ExitVolumeState::from_raw(
            [
                (1, version.write()),
                (2, 30_u8.write()),
                (3, false.write()),
                (4, ExitVolumeStatus::Ready.write()),
            ]
            .into(),
        )
        .unwrap();
        let first = volume_context(&boot(2, 1), &state).unwrap();
        let next = volume_context(&boot(2, 1), &state).unwrap();
        assert_eq!(first.source().unwrap(), boot(2, 1));
        assert_eq!(first.receiver_output().unwrap(), output(2, 1, 7));
        assert_eq!(first.route_version().unwrap(), route_version(2, 8));
        assert!(next.source_route_generation().unwrap() > first.source_route_generation().unwrap());
    }
    #[test]
    fn no_media_before_ack_and_aba_route_boot_and_output_generations_are_fenced() {
        let mut source = Source::new(boot(1, 1));
        let old_offer = source.begin(boot(2, 1), route_version(2, 1)).unwrap().clone();
        let mut receiver = Receiver::prepare(
            old_offer.clone(),
            &boot(1, 1),
            &route_version(2, 1),
            &output(2, 1, 1),
            Profile::Opus5ms,
        )
        .unwrap();
        assert!(!receiver.ready());
        let ack = source.accept(&boot(2, 1), receiver.prepared().wire.clone()).unwrap();
        receiver.acknowledge(&boot(1, 1), &ack).unwrap();
        assert!(receiver.ready());
        source.begin(boot(3, 1), route_version(3, 2)).unwrap();
        source.begin(boot(2, 1), route_version(2, 3)).unwrap();
        assert_eq!(
            source
                .accept(&boot(2, 1), receiver.prepared().wire.clone())
                .unwrap_err(),
            ControlFailure::RouteChanged
        );
        assert_eq!(
            Receiver::prepare(
                old_offer.clone(),
                &boot(1, 2),
                &route_version(2, 1),
                &output(2, 1, 1),
                Profile::Opus5ms
            )
            .unwrap_err(),
            ControlFailure::StaleBoot
        );
        assert_eq!(
            Receiver::prepare(
                old_offer,
                &boot(1, 1),
                &route_version(2, 1),
                &output(2, 2, 2),
                Profile::Opus5ms
            )
            .unwrap_err(),
            ControlFailure::StaleBoot
        );
        let offer = source.offer().unwrap().clone();
        let fresh = Receiver::prepare(
            offer.clone(),
            &boot(1, 1),
            &route_version(2, 3),
            &output(2, 1, 3),
            Profile::Opus5ms,
        )
        .unwrap();
        source.accept(&boot(2, 1), fresh.prepared().wire.clone()).unwrap();
        let stale = Receiver::prepare(
            offer,
            &boot(1, 1),
            &route_version(2, 3),
            &output(2, 1, 2),
            Profile::Opus5ms,
        )
        .unwrap();
        assert_eq!(
            source.accept(&boot(2, 1), stale.prepared().wire.clone()).unwrap_err(),
            ControlFailure::OutputChanged
        );
    }
}
