//! C0 greetings and roster admission. TLS/installed-route identity is supplied by
//! the link owner, while every state candidate is returned for durable commit.
use crate::{
    discovery::Advert,
    group_state::State,
    negotiation::{OfferProfile, Selected},
    pairing::Transcript,
    roster::{Anchor, Roster},
    wire_identity as wire,
};
use oink_foundation::{
    DeviceId,
    session::{Boot, ConnectionId},
};
use oink_protocol::v1::{
    self, Message, RawValue,
    admission::{Agreement, negotiate},
    dto, frame,
    schema::{WireDto, WireValue},
};
use std::time::{Duration, Instant};
/// C0 hello/info exchange after the link has been installed.
pub const GREETING_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Peer {
    pub greeting: dto::Greeting,
    pub negotiated: dto::Negotiated,
    pub agreement: Agreement,
    pub formats: dto::FormatSupport,
    pub audio: Vec<dto::AudioSupport>,
    pub transcript: Transcript,
}
impl Peer {
    pub fn anchor(&self) -> v1::Result<Anchor> {
        Ok(Anchor {
            group: oink_foundation::GroupId(self.greeting.advert()?.group()?.0),
            root: self.greeting.root_id()?.ok_or(v1::ErrorCode::AuthenticationFailed)?.0,
        })
    }
    pub fn hash(&self) -> v1::Result<[u8; 32]> {
        Ok(self.negotiated.offer_hash()?.0)
    }
}
#[derive(Debug)]
pub struct GreetingSession {
    local: Boot,
    peer: Boot,
    outbound: bool,
    profile: OfferProfile,
    physical: Option<Selected>,
    relay: Option<dto::RelayContext>,
    local_greeting: dto::Greeting,
    hello: Option<Vec<u8>>,
    info: Option<Vec<u8>>,
    peer_greeting: Option<dto::Greeting>,
    result: Option<Peer>,
    deadline: Instant,
}
impl GreetingSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local: Boot,
        peer: Boot,
        outbound: bool,
        profile: OfferProfile,
        physical: Option<Selected>,
        relay: Option<dto::RelayContext>,
        advert: Advert,
        anchor: Anchor,
        roster_digest: [u8; 32],
        now: Instant,
    ) -> v1::Result<(Self, Option<Vec<u8>>)> {
        if (physical.is_some()) == relay.is_some() {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        let offer = if let Some(physical) = &physical {
            offer(&physical.local_offer)?
        } else {
            profile.offer(
                local,
                ConnectionId {
                    initiator_boot: if outbound { local } else { peer },
                    random: rand::random(),
                },
                relay.clone(),
            )?
        };
        let local_greeting = dto::Greeting::from_raw(
            [
                (1, advert.to_wire()?.write()),
                (2, RawValue::from(advert.os.as_str())),
                (3, RawValue::Bytes(rand::random::<[u8; 32]>().to_vec())),
                (4, offer.write()),
                (6, RawValue::Bytes(anchor.root.to_vec())),
                (7, RawValue::Bytes(roster_digest.to_vec())),
            ]
            .into(),
        )?;
        let hello = if outbound {
            Some(frame::encode(
                &Message::ConfigHello(local_greeting.clone()),
                v1::limits::CONFIG_FRAME,
            )?)
        } else {
            None
        };
        Ok((
            Self {
                local,
                peer,
                outbound,
                profile,
                physical,
                relay,
                local_greeting,
                hello: hello.clone(),
                info: None,
                peer_greeting: None,
                result: None,
                deadline: now + GREETING_TIMEOUT,
            },
            hello,
        ))
    }
    pub fn result(&self) -> Option<&Peer> {
        self.result.as_ref()
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.result.is_none().then_some(self.deadline)
    }
    pub fn receive(&mut self, bytes: &[u8], now: Instant) -> v1::Result<Option<Vec<u8>>> {
        if self.result.is_some() || now >= self.deadline {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        let frame = frame::OpaqueFrame::parse_exact(bytes, v1::limits::CONFIG_FRAME)?;
        let greeting = match frame.decode()? {
            Message::ConfigHello(greeting) if !self.outbound => greeting,
            Message::ConfigInfo(greeting) if self.outbound => greeting,
            _ => return Err(v1::ErrorCode::InvalidChannel.into()),
        };
        let peer_offer = greeting.offer()?;
        if wire::read_device(&greeting.advert()?.id()?)? != self.peer.device_id
            || wire::read_boot(&peer_offer.boot()?)? != self.peer
            || greeting.root_id()?.is_none()
            || greeting.roster_digest()?.is_none()
        {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        let selected = if let Some(physical) = &self.physical {
            if peer_offer != offer(&physical.peer_offer)? || peer_offer.relay_context()?.is_some() {
                return Err(v1::ErrorCode::NegotiationMismatch.into());
            }
            physical.negotiated.clone()
        } else {
            if peer_offer.relay_context()? != self.relay
                || wire::read_connection(&peer_offer.connection_id()?)?.initiator_boot
                    != if self.outbound { self.local } else { self.peer }
            {
                return Err(v1::ErrorCode::NegotiationMismatch.into());
            }
            if !self.outbound {
                let mine = self.profile.offer(
                    self.local,
                    wire::read_connection(&peer_offer.connection_id()?)?,
                    self.relay.clone(),
                )?;
                let mut raw = self.local_greeting.raw().clone();
                raw.insert(4, mine.write());
                self.local_greeting = dto::Greeting::from_raw(raw)?;
            } else if self.local_greeting.offer()?.connection_id()? != peer_offer.connection_id()? {
                return Err(v1::ErrorCode::NegotiationMismatch.into());
            }
            let mine = frame::encode(
                &Message::ConnectionOffer(self.local_greeting.offer()?),
                v1::limits::RELIABLE_FRAME,
            )?;
            let theirs = frame::encode(
                &Message::ConnectionOffer(peer_offer.clone()),
                v1::limits::RELIABLE_FRAME,
            )?;
            let (a, b) = if self.outbound {
                (&mine, &theirs)
            } else {
                (&theirs, &mine)
            };
            let implemented = self.profile.capabilities.iter().map(String::as_str).collect::<Vec<_>>();
            let pairing = implemented.contains(&"oink.pairing/1")
                && peer_offer.capabilities()?.0.iter().any(|s| s.0 == "oink.pairing/1");
            negotiate(a, b, &implemented, pairing)?
        };
        let reply = if self.outbound {
            if greeting.selected()?.as_ref() != Some(&selected) {
                return Err(v1::ErrorCode::NegotiationMismatch.into());
            }
            self.info = Some(bytes.to_vec());
            None
        } else {
            if greeting.selected()?.is_some_and(|claimed| claimed != selected) {
                return Err(v1::ErrorCode::NegotiationMismatch.into());
            }
            self.hello = Some(bytes.to_vec());
            let mut raw = self.local_greeting.raw().clone();
            raw.insert(5, selected.write());
            let bytes = frame::encode(
                &Message::ConfigInfo(dto::Greeting::from_raw(raw)?),
                v1::limits::CONFIG_FRAME,
            )?;
            self.info = Some(bytes.clone());
            Some(bytes)
        };
        self.peer_greeting = Some(greeting.clone());
        self.result = Some(Peer {
            greeting,
            agreement: Agreement::new(selected.enabled()?.0.into_iter().map(|s| s.0))?,
            negotiated: selected,
            formats: peer_offer.formats()?,
            audio: peer_offer.audio()?.0,
            transcript: Transcript {
                hello: self.hello.clone().expect("hello"),
                info: self.info.clone().expect("info"),
            },
        });
        Ok(reply)
    }
}
fn offer(bytes: &[u8]) -> v1::Result<dto::ConnectionOffer> {
    match frame::OpaqueFrame::parse_exact(bytes, v1::limits::RELIABLE_FRAME)?.decode()? {
        Message::ConnectionOffer(v) => Ok(v),
        _ => Err(v1::ErrorCode::MalformedFrame.into()),
    }
}
pub fn statement(state: &State, local: DeviceId, hash: [u8; 32]) -> v1::Result<dto::Admission> {
    dto::Admission::from_raw(
        [
            (1, wire::device(local).write()),
            (2, state.wire().group()?.write()),
            (3, RawValue::Bytes(state.roster().anchor().root.to_vec())),
            (4, RawValue::Bytes(state.roster().digest()?.to_vec())),
            (5, RawValue::Bytes(hash.to_vec())),
        ]
        .into(),
    )
}
pub fn request(state: &State, local: DeviceId, hash: [u8; 32], connected: u8) -> v1::Result<Message> {
    if !state.roster().contains(local) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    Ok(Message::ConfigRequest(dto::Request::from_raw(
        [
            (2, connected.write()),
            (4, state.wire().write()),
            (7, statement(state, local, hash)?.write()),
        ]
        .into(),
    )?))
}
fn check_statement(
    admission: &dto::Admission,
    peer: DeviceId,
    anchor: Anchor,
    roster: &Roster,
    hash: [u8; 32],
) -> v1::Result<()> {
    if wire::read_device(&admission.identity()?)? != peer
        || admission.group()?.0 != anchor.group.0
        || admission.root_id()?.0 != anchor.root
        || admission.roster_digest()?.0 != roster.digest()?
    {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    if admission.negotiation_hash()?.0 != hash {
        return Err(v1::ErrorCode::NegotiationMismatch.into());
    }
    Ok(())
}
pub fn receive_request(
    current: &State,
    request: &dto::Request,
    local: DeviceId,
    peer: DeviceId,
    hash: [u8; 32],
) -> v1::Result<State> {
    current.merge(&request_state(current, request, peer, hash)?, peer, local)
}
/// A retained active peer can convey a signed removal of this device during
/// admission. The caller persists it before forming a fresh local group.
pub fn request_revocation(
    current: &State,
    request: &dto::Request,
    local: DeviceId,
    peer: DeviceId,
    hash: [u8; 32],
) -> v1::Result<Option<State>> {
    revocation(current, &request_state(current, request, peer, hash)?, local, peer)
}
fn request_state(
    current: &State,
    request: &dto::Request,
    peer: DeviceId,
    hash: [u8; 32],
) -> v1::Result<dto::GroupState> {
    // An incoming roster cannot establish trust in its own sender.
    if !current.roster().contains(peer) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let incoming = if let Some(state) = request.state()? {
        state
    } else if let Some(grant) = request.grant()? {
        if grant.group()? != grant.state()?.group()? {
            return Err(v1::ErrorCode::AuthenticationFailed.into());
        }
        grant.state()?
    } else {
        let roster = request.roster()?.ok_or(v1::ErrorCode::MalformedFrame)?;
        let mut raw = current.wire().raw().clone();
        raw.insert(3, roster.write());
        dto::GroupState::from_raw(raw)?
    };
    let roster = Roster::verify(incoming.roster()?, current.roster().anchor())?;
    check_statement(&request.admission()?, peer, current.roster().anchor(), &roster, hash)?;
    Ok(incoming)
}
pub fn reply(state: &State, local: DeviceId, hash: [u8; 32], connected: u8) -> v1::Result<Message> {
    let snapshot = dto::ReplySnapshot::from_raw([(1, state.wire().write())].into())?;
    Ok(Message::ConfigReply(dto::Reply::from_raw(
        [
            (2, connected.write()),
            (
                4,
                RawValue::Map([(1, dto::ReplyStateKind::State.write()), (2, snapshot.write())].into()),
            ),
            (7, statement(state, local, hash)?.write()),
        ]
        .into(),
    )?))
}
/// The durable union and the exact peer snapshot acknowledged by Done.
/// Local edits arriving after Request must survive without changing that acknowledgment.
#[derive(Debug)]
pub struct ReplyAcceptance {
    pub merged: State,
    pub acknowledged: State,
}
pub fn receive_reply(
    current: &State,
    sent: &State,
    reply: &dto::Reply,
    local: DeviceId,
    peer: DeviceId,
    hash: [u8; 32],
) -> v1::Result<ReplyAcceptance> {
    let incoming = reply_state(current, sent, reply, peer, hash)?;
    let merged = current.merge(&incoming, peer, local)?;
    let acknowledged = State::verify(incoming, current.roster().anchor())?;
    Ok(ReplyAcceptance { merged, acknowledged })
}
pub fn reply_revocation(
    current: &State,
    sent: &State,
    reply: &dto::Reply,
    local: DeviceId,
    peer: DeviceId,
    hash: [u8; 32],
) -> v1::Result<Option<State>> {
    revocation(current, &reply_state(current, sent, reply, peer, hash)?, local, peer)
}
fn revocation(
    current: &State,
    incoming: &dto::GroupState,
    local: DeviceId,
    peer: DeviceId,
) -> v1::Result<Option<State>> {
    if !current.roster().contains(peer) || !current.roster().contains(local) {
        return Ok(None);
    }
    let candidate = current.receive_update(incoming, peer, local)?;
    Ok((!candidate.roster().contains(local)).then_some(candidate))
}
fn reply_state(
    current: &State,
    sent: &State,
    reply: &dto::Reply,
    peer: DeviceId,
    hash: [u8; 32],
) -> v1::Result<dto::GroupState> {
    if !current.roster().contains(peer) {
        return Err(v1::ErrorCode::AuthenticationFailed.into());
    }
    let result = reply.state()?;
    let incoming = match result.variant()? {
        v1::schema::Open::Known(dto::ReplyStateKind::State) => dto::ReplySnapshot::from_raw(result.body()?)?.state()?,
        v1::schema::Open::Known(dto::ReplyStateKind::Same) => {
            if dto::ReplySame::from_raw(result.body()?)?.request_state_digest()?.0 != sent.digest()? {
                return Err(v1::ErrorCode::StateConflict.into());
            }
            if let Some(grant) = reply.grant()? {
                grant.state()?
            } else if let Some(roster) = reply.roster()? {
                let mut raw = sent.wire().raw().clone();
                raw.insert(3, roster.write());
                dto::GroupState::from_raw(raw)?
            } else {
                sent.wire().clone()
            }
        }
        _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
    };
    let roster = Roster::verify(incoming.roster()?, current.roster().anchor())?;
    check_statement(&reply.admission()?, peer, current.roster().anchor(), &roster, hash)?;
    Ok(incoming)
}
pub fn done(state: &State, hash: [u8; 32]) -> v1::Result<Message> {
    Ok(Message::ConfigDone(dto::Done::from_raw(
        [
            (1, RawValue::Bytes(state.digest()?.to_vec())),
            (2, RawValue::Bytes(hash.to_vec())),
            (3, RawValue::Bytes(state.roster().digest()?.to_vec())),
        ]
        .into(),
    )?))
}
pub fn receive_done(expected: &State, done: &dto::Done, hash: [u8; 32]) -> v1::Result<()> {
    if done.negotiation_hash()?.0 != hash {
        return Err(v1::ErrorCode::NegotiationMismatch.into());
    }
    if done.accepted_state_digest()?.0 != expected.digest()? || done.roster_digest()?.0 != expected.roster().digest()? {
        return Err(v1::ErrorCode::StateConflict.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{identity::Identity, negotiation::Bootstrap};

    fn boots() -> (Identity, Identity, Boot, Boot) {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let ab = Boot {
            device_id: a.id(),
            nonce: [3; 16],
        };
        let bb = Boot {
            device_id: b.id(),
            nonce: [4; 16],
        };
        (a, b, ab, bb)
    }
    fn physical(a: Boot, b: Boot, now: Instant) -> (Selected, Selected) {
        let connection = ConnectionId {
            initiator_boot: a,
            random: [5; 16],
        };
        let (mut left, offer) = Bootstrap::new(a, b.device_id, Some(connection), OfferProfile::default(), now).unwrap();
        let (mut right, _) = Bootstrap::new(b, a.device_id, None, OfferProfile::default(), now).unwrap();
        let replies = right.receive(&offer.unwrap(), now).unwrap();
        for reply in replies {
            for answer in left.receive(&reply, now).unwrap() {
                right.receive(&answer, now).unwrap();
            }
        }
        (left.selected().unwrap().clone(), right.selected().unwrap().clone())
    }
    fn greeting(
        local: Boot,
        peer: Boot,
        outbound: bool,
        physical: Option<Selected>,
        relay: Option<dto::RelayContext>,
        state: &State,
        now: Instant,
    ) -> (GreetingSession, Option<Vec<u8>>) {
        GreetingSession::new(
            local,
            peer,
            outbound,
            OfferProfile::default(),
            physical,
            relay,
            Advert::local(
                local.device_id,
                "test".into(),
                state.roster().anchor().group,
                2,
                44242,
                "test".into(),
            ),
            state.roster().anchor(),
            state.roster().digest().unwrap(),
            now,
        )
        .unwrap()
    }
    #[test]
    fn direct_greeting_binds_exact_physical_offers_and_transcript() {
        let (a, _, ab, bb) = boots();
        let now = Instant::now();
        let state = State::create(&a, "a", "test", 1).unwrap();
        let (pa, pb) = physical(ab, bb, now);
        let (mut left, hello) = greeting(ab, bb, true, Some(pa.clone()), None, &state, now);
        let (mut right, _) = greeting(bb, ab, false, Some(pb.clone()), None, &state, now);
        let hello = hello.unwrap();
        let info = right.receive(&hello, now).unwrap().unwrap();
        left.receive(&info, now).unwrap();
        assert_eq!(
            left.result().unwrap().hash().unwrap(),
            pa.negotiated.offer_hash().unwrap().0
        );
        assert_eq!(left.result().unwrap().transcript.hello, hello);
        assert_eq!(right.result().unwrap().transcript.info, info);
        let (mut right, _) = greeting(bb, ab, false, Some(pb), None, &state, now);
        let Message::ConfigHello(original) = frame::OpaqueFrame::parse_exact(&hello, v1::limits::CONFIG_FRAME)
            .unwrap()
            .decode()
            .unwrap()
        else {
            panic!()
        };
        let mut offer_raw = original.offer().unwrap().raw().clone();
        offer_raw.insert(8, RawValue::Bytes([9; 32].to_vec()));
        let mut raw = original.raw().clone();
        raw.insert(4, dto::ConnectionOffer::from_raw(offer_raw).unwrap().write());
        let forged = frame::encode(
            &Message::ConfigHello(dto::Greeting::from_raw(raw).unwrap()),
            v1::limits::CONFIG_FRAME,
        )
        .unwrap();
        assert!(right.receive(&forged, now).is_err());
    }
    #[test]
    fn relayed_greeting_negotiates_fresh_offers_bound_to_installed_route() {
        let (a, _, ab, bb) = boots();
        let now = Instant::now();
        let state = State::create(&a, "a", "test", 1).unwrap();
        let path = oink_foundation::session::PathId {
            boot_pair: oink_foundation::session::BootPair::new(ab, bb).unwrap(),
            coordinator_counter: 1,
        };
        let relay = dto::RelayContext::from_raw(
            [
                (1, wire::path(path).write()),
                (2, 1u64.write()),
                (3, wire::boot(ab).write()),
                (4, wire::boot(bb).write()),
            ]
            .into(),
        )
        .unwrap();
        let (mut left, hello) = greeting(ab, bb, true, None, Some(relay.clone()), &state, now);
        let (mut right, _) = greeting(bb, ab, false, None, Some(relay.clone()), &state, now);
        let info = right.receive(&hello.unwrap(), now).unwrap().unwrap();
        left.receive(&info, now).unwrap();
        assert_eq!(
            left.result().unwrap().hash().unwrap(),
            right.result().unwrap().hash().unwrap()
        );
        let mut wrong = relay.raw().clone();
        wrong.insert(2, 2u64.write());
        let (mut wrong_route, _) = greeting(
            bb,
            ab,
            false,
            None,
            Some(dto::RelayContext::from_raw(wrong).unwrap()),
            &state,
            now,
        );
        assert!(
            wrong_route
                .receive(&left.result().unwrap().transcript.hello, now)
                .is_err()
        );
    }
    #[test]
    fn admission_rejects_stale_removed_member_and_done_must_match_exact_state() {
        let (a, b, _, _) = boots();
        let initial = State::create(&a, "a", "test", 1).unwrap();
        let roster = initial.roster().add(&a, b.id(), "b", "test", 2).unwrap();
        let joined = initial.with_roster(&roster).unwrap();
        let hash = [8; 32];
        let Message::ConfigRequest(request) = request(&joined, b.id(), hash, 0).unwrap() else {
            panic!()
        };
        assert!(receive_request(&joined, &request, a.id(), b.id(), hash).is_ok());
        let removed = joined
            .with_roster(&roster.remove(&a, b.id(), None, 3).unwrap())
            .unwrap();
        assert!(receive_request(&removed, &request, a.id(), b.id(), hash).is_err());
        assert!(receive_request(&initial, &request, a.id(), b.id(), [9; 32]).is_err());
        let Message::ConfigDone(done) = done(&joined, hash).unwrap() else {
            panic!()
        };
        receive_done(&joined, &done, hash).unwrap();
        assert!(receive_done(&initial, &done, hash).is_err());
        assert!(receive_done(&joined, &done, [9; 32]).is_err());
    }
    #[test]
    fn repeated_membership_cycles_ignore_old_revocations_and_apply_the_latest_epoch() {
        let a = Identity::from_seed([1; 32]);
        let b = Identity::from_seed([2; 32]);
        let d = Identity::from_seed([3; 32]);
        let base = State::create(&a, "a", "test", 1).unwrap();
        let base = base
            .with_roster(&base.roster().add(&a, b.id(), "b", "test", 2).unwrap())
            .unwrap();
        let mut current = base;
        let hash = [8; 32];
        let mut old_removals = Vec::new();
        for (add_epoch, remove_epoch) in [(1, 2), (3, 4), (5, 6)] {
            // B reintroduces D; A's older Remove records remain in the union.
            current = current
                .with_roster(&current.roster().add(&b, d.id(), "d", "test", 3).unwrap())
                .unwrap();
            assert_eq!(current.roster().member(d.id()).unwrap().epoch, add_epoch);
            let Message::ConfigRequest(from_d) = request(&current, d.id(), hash, 0).unwrap() else {
                panic!()
            };
            assert!(receive_request(&current, &from_d, a.id(), d.id(), hash).is_ok());
            for removed in &old_removals {
                // Use the CURRENT transcript hash so this exercises the epoch
                // union, not just rejection of an old connection's transcript.
                let Message::ConfigReply(reply) = reply(removed, a.id(), hash, 0).unwrap() else {
                    panic!()
                };
                assert!(
                    reply_revocation(&current, &current, &reply, d.id(), a.id(), hash)
                        .unwrap()
                        .is_none()
                );
                let accepted = receive_reply(&current, &current, &reply, d.id(), a.id(), hash).unwrap();
                assert_eq!(accepted.merged.roster().member(d.id()).unwrap().epoch, add_epoch);
                assert!(reply_revocation(&current, &current, &reply, d.id(), a.id(), [9; 32]).is_err());
                let Message::ConfigRequest(request) = request(removed, a.id(), hash, 0).unwrap() else {
                    panic!()
                };
                assert!(
                    request_revocation(&current, &request, d.id(), a.id(), hash)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    receive_request(&current, &request, d.id(), a.id(), hash)
                        .unwrap()
                        .roster()
                        .member(d.id())
                        .unwrap()
                        .epoch,
                    add_epoch
                );
            }
            let removed = current
                .with_roster(&current.roster().remove(&a, d.id(), None, 4).unwrap())
                .unwrap();
            assert_eq!(removed.roster().next_epoch(d.id()).unwrap(), remove_epoch + 1);
            assert!(receive_request(&removed, &from_d, a.id(), d.id(), hash).is_err());
            let Message::ConfigReply(reply) = reply(&removed, a.id(), hash, 0).unwrap() else {
                panic!()
            };
            let revoked = reply_revocation(&current, &current, &reply, d.id(), a.id(), hash)
                .unwrap()
                .expect("the current removal must revoke D");
            assert!(!revoked.roster().contains(d.id()));
            assert_eq!(revoked.roster().next_epoch(d.id()).unwrap(), remove_epoch + 1);
            old_removals.push(removed.clone());
            current = removed;
        }
    }
    #[test]
    fn full_trust_admission_requires_a_locally_known_peer_before_reading_its_roster() {
        let (a, b, _, _) = boots();
        let initial = State::create(&a, "a", "test", 1).unwrap();
        let joined = initial
            .with_roster(&initial.roster().add(&a, b.id(), "b", "test", 2).unwrap())
            .unwrap();
        let hash = [8; 32];
        let Message::ConfigRequest(request) = request(&joined, b.id(), hash, 0).unwrap() else {
            panic!()
        };
        let Message::ConfigReply(reply) = reply(&joined, b.id(), hash, 0).unwrap() else {
            panic!()
        };
        // Even a well-formed, correctly signed self-including roster cannot
        // bootstrap trust. Only our retained roster or PAKE can add this peer.
        assert!(receive_request(&initial, &request, a.id(), b.id(), hash).is_err());
        assert!(request_revocation(&initial, &request, a.id(), b.id(), hash).is_err());
        assert!(receive_reply(&initial, &initial, &reply, a.id(), b.id(), hash).is_err());
        assert!(reply_revocation(&initial, &initial, &reply, a.id(), b.id(), hash).is_err());
        crate::identity::STRICT_VERIFICATIONS.with(|count| count.set(0));
        assert!(receive_request(&joined, &request, a.id(), b.id(), hash).is_ok());
        assert!(receive_reply(&joined, &joined, &reply, a.id(), b.id(), hash).is_ok());
        crate::identity::STRICT_VERIFICATIONS.with(|count| assert_eq!(count.get(), 0));
    }
    #[test]
    fn concurrent_pointer_slot_edit_does_not_change_the_reply_acknowledgment() {
        let (a, b, _, _) = boots();
        let initial = State::create(&a, "a", "test", 1).unwrap();
        let joined = initial
            .with_roster(&initial.roster().add(&a, b.id(), "b", "test", 2).unwrap())
            .unwrap();
        let hash = [8; 32];
        let Message::ConfigReply(reply) = reply(&joined, b.id(), hash, 0).unwrap() else {
            panic!()
        };
        // A retained local edit lands after Request, before the frozen Reply.
        let current = joined.reconcile_pointer_slots(a.id()).unwrap();
        assert_ne!(current.digest().unwrap(), joined.digest().unwrap());
        let accepted = receive_reply(&current, &joined, &reply, a.id(), b.id(), hash).unwrap();
        assert_eq!(accepted.merged.digest().unwrap(), current.digest().unwrap());
        assert_eq!(accepted.acknowledged.digest().unwrap(), joined.digest().unwrap());
        let Message::ConfigDone(wrong) = done(&accepted.merged, hash).unwrap() else {
            panic!()
        };
        assert!(receive_done(&joined, &wrong, hash).is_err());
        let Message::ConfigDone(done) = done(&accepted.acknowledged, hash).unwrap() else {
            panic!()
        };
        receive_done(&joined, &done, hash).unwrap();
    }
}
