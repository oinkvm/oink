//! Speaker-exit M0 admission and native M1 playback for the current output.
use oink_audio::{
    idle::Bursts,
    media::{self, Media},
    negotiation::{self, Profile},
    session::Receiver,
    sink::{NativeSources, Packet, Source},
};
use oink_foundation::session::Boot;
use oink_network::{
    link::{DatagramRecv, StreamId},
    wire_identity::boot,
};
use oink_protocol::v2::{Message, dto};
use std::{collections::HashMap, sync::Arc, time::Instant};

const MAX_PREPARED: usize = 16;

struct Incoming {
    session: Receiver,
    packets: DatagramRecv,
    source: Option<Source>,
}

pub(crate) struct SpeakerTransport {
    sources: Arc<NativeSources>,
    route: Option<(dto::AudioRouteVersion, dto::ReceiverOutputBinding)>,
    incoming: HashMap<Boot, Incoming>,
    last_activity: Option<Instant>,
}

impl SpeakerTransport {
    pub(crate) fn new(sources: Arc<NativeSources>) -> Self {
        Self {
            sources,
            route: None,
            incoming: HashMap::new(),
            last_activity: None,
        }
    }

    /// The actor supplies the exit route version (while this phone is the
    /// group exit) and current native
    /// output binding. Retiring either revokes all prepared decoders.
    pub(crate) fn set_route(&mut self, route: Option<(dto::AudioRouteVersion, dto::ReceiverOutputBinding)>) {
        if self.route != route {
            self.incoming.clear();
            self.route = route;
        }
    }

    pub(crate) fn retain_peers(&mut self, mut ready: impl FnMut(Boot, StreamId) -> bool) {
        self.incoming
            .retain(|peer, entry| ready(*peer, entry.packets.follows()));
    }

    pub(crate) fn take_activity(&mut self) -> Option<Instant> {
        let newest = self
            .incoming
            .values()
            .filter_map(|entry| entry.source.as_ref()?.latest_arrival())
            .max()?;
        if self.last_activity.is_some_and(|previous| previous >= newest) {
            return None;
        }
        self.last_activity = Some(newest);
        Some(newest)
    }

    pub(crate) fn receive(
        &mut self,
        peer: Boot,
        message: &Message,
        datagram: Option<DatagramRecv>,
        support: &[dto::AudioSupport],
    ) -> Option<Message> {
        match message {
            Message::AudioSourceRouteOffer(offer) | Message::AudioQualitySourceOffer(offer) => {
                // An offer this exit cannot take is refused with its reason, so
                // the source does not keep waiting for it (pause audit 6).
                let Some((version, output)) = self.route.as_ref() else {
                    return refuse_offer(offer, dto::ControlFailure::NotExit, None);
                };
                let Some(packets) = datagram else {
                    return refuse_offer(offer, dto::ControlFailure::LinkLost, Some(output));
                };
                let preferred = offer
                    .preferred_descriptor()
                    .ok()
                    .flatten()
                    .and_then(|descriptor| Profile::from_descriptor(&descriptor).ok())
                    .unwrap_or(Profile::Opus5ms);
                let profile = match negotiation::select(preferred, support, true, oink_protocol::v2::limits::DATAGRAM) {
                    Ok(profile) => profile,
                    Err(reason) => return refuse_offer(offer, reason, Some(output)),
                };
                if let Some(current) = self.incoming.get(&peer) {
                    if current.packets.follows() == packets.follows() {
                        if current.session.offer().source_route_generation().ok()?
                            > offer.source_route_generation().ok()?
                        {
                            return None;
                        }
                        if current.session.offer() == offer && current.session.prepared().profile == profile {
                            return Some(Message::AudioPrepared(current.session.prepared().wire.clone()));
                        }
                    }
                } else if self.incoming.len() >= MAX_PREPARED {
                    return refuse_offer(offer, dto::ControlFailure::Busy, Some(output));
                }
                let session = match Receiver::prepare(offer.clone(), &boot(peer), version, output, profile) {
                    Ok(session) => session,
                    Err(reason) => return refuse_offer(offer, reason, Some(output)),
                };
                let response = Message::AudioPrepared(session.prepared().wire.clone());
                self.incoming.insert(
                    peer,
                    Incoming {
                        session,
                        packets,
                        source: None,
                    },
                );
                Some(response)
            }
            Message::AudioPreparedAck(ack) => {
                let incoming = self.incoming.get_mut(&peer)?;
                if incoming.packets.follows() != datagram?.follows() {
                    return None;
                }
                incoming.session.acknowledge(&boot(peer), ack).ok()?;
                if incoming.source.is_none() {
                    incoming.source = bind_source(&self.sources, incoming);
                }
                None
            }
            Message::AudioRouteFault(fault) => {
                if self
                    .incoming
                    .get(&peer)
                    .is_some_and(|entry| fault.session() == Ok(entry.session.prepared().session()))
                {
                    self.incoming.remove(&peer);
                }
                None
            }
            _ => None,
        }
    }
}

fn bind_source(sources: &NativeSources, incoming: &Incoming) -> Option<Source> {
    let profile = incoming.session.prepared().profile;
    let stream = incoming.session.prepared().stream_id();
    let packets = incoming.packets.clone();
    let source = sources.source(profile.config()).ok()?;
    let bursts = Arc::new(Bursts::default());
    let receive_bursts = Arc::clone(&bursts);
    source.set_sleep_detector(Arc::new(move |now| bursts.asleep(now)));
    source.set_packet_reader(Arc::new(move |budget| {
        let mut bytes = [0; media::FRAME_CAPACITY];
        while *budget != 0 {
            *budget -= 1;
            let Ok(Some((len, arrived))) = packets.take_timed_into(&mut bytes) else {
                break;
            };
            match media::decode(&bytes[..len], stream, profile) {
                Ok(Media::Sleep { sequence, burst }) => receive_bursts.sleep(burst, sequence),
                Ok(Media::Packet {
                    sequence,
                    burst,
                    source_time,
                    payload,
                    ..
                }) if receive_bursts.media(burst, sequence, arrived) => {
                    let mut packet = Packet {
                        burst,
                        sequence,
                        payload: [0; oink_audio::dsp::MAX_PACKET_BYTES],
                        len: payload.len(),
                        arrived,
                        network_read: Some(Instant::now()),
                        source_emit: Some(source_time),
                    };
                    packet.payload[..payload.len()].copy_from_slice(payload);
                    return (true, Some(packet));
                }
                _ => {}
            }
        }
        (true, None)
    }));
    Some(source)
}

/// `AudioPrepareRefused` for an offer this phone cannot take as the exit.
pub(crate) fn refuse_offer(
    offer: &dto::SourceRouteOffer,
    reason: dto::ControlFailure,
    output: Option<&dto::ReceiverOutputBinding>,
) -> Option<Message> {
    use oink_protocol::v2::schema::{WireDto, WireValue};
    let mut raw: oink_protocol::v2::RawMap = [(1, offer.write()), (2, reason.write())].into();
    if let Some(output) = output {
        raw.insert(3, output.write());
    }
    dto::AudioPrepareRefused::from_raw(raw)
        .ok()
        .map(Message::AudioPrepareRefused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use oink_protocol::v2::schema::{Text, WireDto, WireValue};

    /// Pause audit 6: an offer the phone cannot take as the exit is refused
    /// with its reason instead of left unanswered.
    #[test]
    fn offers_the_exit_cannot_take_are_refused_with_a_reason() {
        let phone = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let pc = Boot {
            device_id: DeviceId([2; 32]),
            nonce: [2; 16],
        };
        let version = oink_audio::route::Version {
            stamp: 1,
            author: pc.device_id,
        }
        .to_wire();
        let offer = Message::AudioSourceRouteOffer(
            dto::SourceRouteOffer::from_raw(
                [
                    (1, boot(pc).write()),
                    (2, 1u64.write()),
                    (3, boot(phone).write()),
                    (4, version.write()),
                ]
                .into(),
            )
            .unwrap(),
        );
        let output = dto::ReceiverOutputBinding::from_raw(
            [
                (1, boot(phone).write()),
                (2, 7u64.write()),
                (3, Some(Text::<512>("mobile:default".into())).write()),
            ]
            .into(),
        )
        .unwrap();
        let reason = |reply: Option<Message>| match reply {
            Some(Message::AudioPrepareRefused(refused)) => refused.reason().unwrap(),
            other => panic!("expected a refusal, got {other:?}"),
        };
        let (_pull, sources) = oink_audio::sink::NativePull::new(48_000);
        let mut transport = SpeakerTransport::new(sources);
        assert_eq!(
            reason(transport.receive(pc, &offer, None, &[])),
            dto::ControlFailure::NotExit
        );
        transport.set_route(Some((version, output)));
        assert_eq!(
            reason(transport.receive(pc, &offer, None, &[])),
            dto::ControlFailure::LinkLost
        );
        assert_eq!(
            reason(refuse_offer(
                &dto::SourceRouteOffer::from_raw(
                    [
                        (1, boot(pc).write()),
                        (2, 1u64.write()),
                        (3, boot(phone).write()),
                        (
                            4,
                            oink_audio::route::Version {
                                stamp: 1,
                                author: pc.device_id
                            }
                            .to_wire()
                            .write()
                        ),
                    ]
                    .into()
                )
                .unwrap(),
                dto::ControlFailure::Busy,
                None
            )),
            dto::ControlFailure::Busy
        );
    }
}
