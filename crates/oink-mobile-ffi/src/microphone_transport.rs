//! Demand and readiness remain fenced by the desktop's existing session type.
use oink_audio::{
    microphone::session::{SessionContext, Source},
    negotiation::Profile,
};
use oink_foundation::session::Boot;
use oink_mobile_core::MobileNetwork;
use oink_network::{
    link::{DatagramSend, StreamId},
    wire_identity::boot,
};
use oink_protocol::v2::{
    Message, dto,
    schema::{WireDto, WireValue},
};
use std::time::Instant;

#[derive(Clone)]
pub(crate) struct MicRoute {
    pub(crate) stream: u64,
    pub(crate) sender: DatagramSend,
}

impl PartialEq for MicRoute {
    fn eq(&self, other: &Self) -> bool {
        self.stream == other.stream && self.sender.follows() == other.sender.follows()
    }
}

#[derive(Default)]
pub(crate) struct MicrophoneTransport {
    route: Option<dto::AudioRouteVersion>,
    source: Source,
}

impl MicrophoneTransport {
    /// The microphone route version while this phone is the group source.
    pub(crate) fn set_route(&mut self, route: Option<dto::AudioRouteVersion>) {
        if self.route != route {
            self.source.clear();
            self.route = route;
        }
    }

    /// Return an optional M0 response and whether the lease was valid activity.
    pub(crate) fn receive(
        &mut self,
        local: Boot,
        peer: Boot,
        message: &Message,
        now: Instant,
    ) -> (Option<Message>, bool) {
        // A demand this phone cannot serve is refused at once with its reason,
        // instead of leaving the receiver to repeat it (pause audit 5).
        let refuse = |session: dto::MicrophoneSession, reason: dto::ControlFailure| {
            dto::MicrophoneStop::from_raw([(1, session.write()), (2, reason.write())].into())
                .ok()
                .map(Message::MicrophoneStop)
        };
        if let Message::MicrophoneDemand(demand) = message {
            let Ok(session) = demand.session() else {
                return (None, false);
            };
            let Some(route) = &self.route else {
                // Not (or no longer) the group microphone source.
                return (refuse(session, dto::ControlFailure::RouteChanged), false);
            };
            if !demand.formats().is_ok_and(|formats| {
                formats
                    .0
                    .iter()
                    .any(|format| Profile::from_descriptor(format) == Ok(Profile::Opus5ms))
            }) {
                return (refuse(session, dto::ControlFailure::Unsupported), false);
            }
            let context = SessionContext {
                source: &boot(local),
                receiver: &boot(peer),
                route,
            };
            return match self
                .source
                .offer(peer.device_id, session.clone(), context, Profile::Opus5ms, now)
            {
                Ok(prepared) => (Some(Message::MicrophonePrepared(prepared)), true),
                Err(reason) => (refuse(session, reason), false),
            };
        }
        if self.route.is_none() {
            return (None, false);
        }
        let device = peer.device_id;
        match message {
            Message::MicrophoneReady(ready) => {
                let accepted = matches!((ready.session(), ready.stream_id()), (Ok(session), Ok(stream))
                    if self.source.ready(device, &session, stream, now));
                (None, accepted)
            }
            Message::MicrophoneRenew(renew) => {
                let accepted = matches!((renew.session(), renew.stream_id()), (Ok(session), Ok(stream))
                    if self.source.renew(device, &session, stream, now));
                (None, accepted)
            }
            Message::MicrophoneStop(stop) => {
                if let Ok(session) = stop.session() {
                    self.source.stop(device, &session);
                }
                (None, false)
            }
            _ => (None, false),
        }
    }

    pub(crate) fn routes(
        &mut self,
        network: &MobileNetwork,
        peers: &[(Boot, StreamId)],
        now: Instant,
    ) -> Vec<MicRoute> {
        self.source.retain(now, |device, lease| {
            peers.iter().any(|(peer, _)| {
                peer.device_id == device
                    && lease
                        .prepared
                        .session()
                        .and_then(|session| session.receiver_output())
                        .and_then(|output| output.receiver())
                        == Ok(boot(*peer))
            })
        });
        self.source
            .leases()
            .filter(|(_, lease)| lease.ready)
            .filter_map(|(peer, lease)| {
                let (_, stream) = peers.iter().find(|(boot, _)| boot.device_id == peer)?;
                Some(MicRoute {
                    stream: lease.prepared.stream_id().ok()?,
                    sender: network.audio_datagram_sender(*stream)?,
                })
            })
            .collect()
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.source.leases().map(|(_, lease)| lease.expires).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use oink_protocol::v2::schema::{FixedBytes, List, Text};

    fn demand(source: Boot, receiver: Boot, formats: Vec<dto::AudioDescriptor>) -> Message {
        let output = dto::ReceiverOutputBinding::from_raw(
            [
                (1, boot(receiver).write()),
                (2, 1u64.write()),
                (3, Some(Text::<512>("virtual".into())).write()),
            ]
            .into(),
        )
        .unwrap();
        let version = oink_audio::route::Version {
            stamp: 1,
            author: source.device_id,
        }
        .to_wire();
        let session = dto::MicrophoneSession::from_raw(
            [
                (1, boot(source).write()),
                (2, output.write()),
                (3, version.write()),
                (4, FixedBytes([3; 16]).write()),
            ]
            .into(),
        )
        .unwrap();
        Message::MicrophoneDemand(
            dto::MicrophoneDemand::from_raw([(1, session.write()), (2, List::<_, 8>(formats).write())].into()).unwrap(),
        )
    }

    /// Pause audit 5: a demand this phone cannot serve is answered with a
    /// MicrophoneStop and its reason instead of being dropped.
    #[test]
    fn unserviceable_demands_are_refused_with_a_reason() {
        let phone = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let pc = Boot {
            device_id: DeviceId([2; 32]),
            nonce: [2; 16],
        };
        let now = Instant::now();
        let reason = |reply: Option<Message>| match reply {
            Some(Message::MicrophoneStop(stop)) => stop.reason().unwrap(),
            other => panic!("expected a refusal, got {other:?}"),
        };
        let mut transport = MicrophoneTransport::default();
        let opus = vec![Profile::Opus5ms.descriptor()];
        let (reply, active) = transport.receive(phone, pc, &demand(phone, pc, opus.clone()), now);
        assert!(!active);
        assert_eq!(reason(reply), dto::ControlFailure::RouteChanged, "not the source");
        let version = oink_audio::route::Version {
            stamp: 1,
            author: phone.device_id,
        }
        .to_wire();
        transport.set_route(Some(version));
        let pcm = vec![Profile::Pcm16_2p5ms.descriptor()];
        let (reply, _) = transport.receive(phone, pc, &demand(phone, pc, pcm), now);
        assert_eq!(reason(reply), dto::ControlFailure::Unsupported);
        let (reply, active) = transport.receive(phone, pc, &demand(phone, pc, opus), now);
        assert!(active && matches!(reply, Some(Message::MicrophonePrepared(_))));
    }
}
