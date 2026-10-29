//! Pure demand/lease state, independent of native devices and network owners.
use crate::negotiation::Profile;
use oink_foundation::DeviceId;
use oink_protocol::v1::{
    dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub const RENEW: Duration = Duration::from_millis(300);
pub const LEASE: Duration = Duration::from_millis(1500);
pub const MAX_RECEIVERS: usize = 16;
#[derive(Clone, Debug)]
pub struct Lease {
    pub prepared: dto::MicrophonePrepared,
    pub profile: Profile,
    pub ready: bool,
    pub expires: Instant,
}
/// Current owner identities and claim used to fence a microphone offer.
#[derive(Clone, Copy, Debug)]
pub struct SessionContext<'a> {
    pub source: &'a dto::Boot,
    pub receiver: &'a dto::Boot,
    pub claim: &'a dto::ExitClaimStamp,
}
#[derive(Debug, Default)]
pub struct Source {
    leases: BTreeMap<DeviceId, Lease>,
}
impl Source {
    pub fn offer(
        &mut self,
        peer: DeviceId,
        session: dto::MicrophoneSession,
        context: SessionContext<'_>,
        profile: Profile,
        now: Instant,
    ) -> Result<dto::MicrophonePrepared, dto::ControlFailure> {
        validate(&session, context.source, context.receiver, context.claim)?;
        if let Some(old) = self.leases.get(&peer) {
            if old.expires > now && old.prepared.session().as_ref() == Ok(&session) {
                return Ok(old.prepared.clone());
            }
        } else if self.leases.len() >= MAX_RECEIVERS {
            return Err(dto::ControlFailure::Busy);
        }
        let prepared = dto::MicrophonePrepared::from_raw(
            [
                (1, session.write()),
                (2, crate::session::next_generation()?.write()),
                (3, profile.descriptor().write()),
            ]
            .into(),
        )
        .map_err(|_| dto::ControlFailure::InvalidValue)?;
        self.leases.insert(
            peer,
            Lease {
                prepared: prepared.clone(),
                profile,
                ready: false,
                expires: now + LEASE,
            },
        );
        Ok(prepared)
    }
    pub fn ready(&mut self, peer: DeviceId, session: &dto::MicrophoneSession, stream: u64, now: Instant) -> bool {
        let Some(lease) = self.leases.get_mut(&peer).filter(|l| {
            l.expires > now && l.prepared.session().as_ref() == Ok(session) && l.prepared.stream_id() == Ok(stream)
        }) else {
            return false;
        };
        lease.ready = true;
        lease.expires = now + LEASE;
        true
    }
    pub fn renew(&mut self, peer: DeviceId, session: &dto::MicrophoneSession, stream: u64, now: Instant) -> bool {
        let Some(lease) = self.leases.get_mut(&peer).filter(|l| {
            l.ready
                && l.expires > now
                && l.prepared.session().as_ref() == Ok(session)
                && l.prepared.stream_id() == Ok(stream)
        }) else {
            return false;
        };
        lease.expires = now + LEASE;
        true
    }
    pub fn stop(&mut self, peer: DeviceId, session: &dto::MicrophoneSession) -> bool {
        if self
            .leases
            .get(&peer)
            .is_some_and(|l| l.prepared.session().as_ref() == Ok(session))
        {
            self.leases.remove(&peer);
            true
        } else {
            false
        }
    }
    pub fn remove(&mut self, peer: DeviceId) {
        self.leases.remove(&peer);
    }
    pub fn retain(&mut self, now: Instant, mut allowed: impl FnMut(DeviceId, &Lease) -> bool) -> Vec<DeviceId> {
        let mut removed = Vec::new();
        self.leases.retain(|peer, lease| {
            let keep = lease.expires > now && allowed(*peer, lease);
            if !keep {
                removed.push(*peer);
            }
            keep
        });
        removed
    }
    pub fn leases(&self) -> impl Iterator<Item = (DeviceId, &Lease)> {
        self.leases.iter().map(|(peer, l)| (*peer, l))
    }
    pub fn active(&self) -> bool {
        self.leases.values().any(|lease| lease.ready)
    }
    pub fn clear(&mut self) {
        self.leases.clear();
    }
}
pub fn validate(
    session: &dto::MicrophoneSession,
    source: &dto::Boot,
    receiver: &dto::Boot,
    claim: &dto::ExitClaimStamp,
) -> Result<(), dto::ControlFailure> {
    use dto::ControlFailure as Failure;
    if session.source().as_ref() != Ok(source)
        || session.receiver_output().and_then(|o| o.receiver()).as_ref() != Ok(receiver)
    {
        return Err(Failure::StaleBoot);
    }
    if session.share_claim().as_ref() != Ok(claim)
        || claim.owner() != source.device_id()
        || source.device_id() == receiver.device_id()
    {
        return Err(Failure::RouteChanged);
    }
    if session
        .receiver_output()
        .and_then(|o| o.output_id())
        .ok()
        .flatten()
        .is_none()
    {
        return Err(Failure::OutputUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{
        RawValue,
        schema::{FixedBytes, Text},
    };
    fn boot(id: u8, run: u8) -> dto::Boot {
        let device = dto::DeviceId::from_raw(
            [
                (1, RawValue::Text(dto::ED25519.into())),
                (2, RawValue::Bytes(vec![id; 32])),
            ]
            .into(),
        )
        .unwrap();
        dto::Boot::from_raw([(1, device.write()), (2, FixedBytes([run; 16]).write())].into()).unwrap()
    }
    fn context(request: u8) -> (dto::MicrophoneSession, dto::Boot, dto::Boot, dto::ExitClaimStamp) {
        let source = boot(1, 1);
        let receiver = boot(2, 1);
        let claim = dto::ExitClaimStamp::from_raw([(1, 1u64.write()), (2, source.device_id().unwrap().write())].into())
            .unwrap();
        let output = dto::ReceiverOutputBinding::from_raw(
            [
                (1, receiver.write()),
                (2, 1u64.write()),
                (3, Some(Text::<512>("virtual-feed".into())).write()),
            ]
            .into(),
        )
        .unwrap();
        let session = dto::MicrophoneSession::from_raw(
            [
                (1, source.write()),
                (2, output.write()),
                (3, claim.write()),
                (4, FixedBytes([request; 16]).write()),
            ]
            .into(),
        )
        .unwrap();
        (session, source, receiver, claim)
    }
    #[test]
    fn ready_and_live_lease_are_required_and_renew_never_creates_a_session() {
        let mut source = Source::default();
        let peer = DeviceId([2; 32]);
        let now = Instant::now();
        let (session, own, remote, claim) = context(1);
        assert!(!source.renew(peer, &session, 1, now));
        let offer = source
            .offer(
                peer,
                session.clone(),
                SessionContext {
                    source: &own,
                    receiver: &remote,
                    claim: &claim,
                },
                Profile::Opus5ms,
                now,
            )
            .unwrap();
        assert!(!source.active());
        let stream = offer.stream_id().unwrap();
        assert!(!source.renew(peer, &session, stream, now));
        assert!(source.ready(peer, &session, stream, now));
        assert!(source.active());
        assert!(!source.renew(peer, &session, stream, now + LEASE));
        assert_eq!(source.retain(now + LEASE, |_, _| true), vec![peer]);
        assert!(!source.active());
    }
    #[test]
    fn a_late_ready_or_stop_cannot_affect_a_new_consumer_lifetime() {
        let mut source = Source::default();
        let peer = DeviceId([2; 32]);
        let now = Instant::now();
        let (old, own, remote, claim) = context(1);
        let (new, _, _, _) = context(2);
        let a = source
            .offer(
                peer,
                old.clone(),
                SessionContext {
                    source: &own,
                    receiver: &remote,
                    claim: &claim,
                },
                Profile::Opus5ms,
                now,
            )
            .unwrap();
        let b = source
            .offer(
                peer,
                new.clone(),
                SessionContext {
                    source: &own,
                    receiver: &remote,
                    claim: &claim,
                },
                Profile::Opus5ms,
                now,
            )
            .unwrap();
        assert!(!source.ready(peer, &old, a.stream_id().unwrap(), now));
        assert!(!source.stop(peer, &old));
        assert!(source.ready(peer, &new, b.stream_id().unwrap(), now));
        assert!(source.active());
        assert!(source.stop(peer, &new));
        assert!(!source.active());
    }
    #[test]
    fn stale_boot_and_replaced_source_claim_are_rejected() {
        let (session, own, remote, claim) = context(1);
        assert_eq!(
            validate(&session, &boot(1, 2), &remote, &claim),
            Err(dto::ControlFailure::StaleBoot)
        );
        let other =
            dto::ExitClaimStamp::from_raw([(1, 2u64.write()), (2, own.device_id().unwrap().write())].into()).unwrap();
        assert_eq!(
            validate(&session, &own, &remote, &other),
            Err(dto::ControlFailure::RouteChanged)
        );
    }
}
