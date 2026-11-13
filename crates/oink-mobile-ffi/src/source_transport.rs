//! The phone is an ordinary speaker source; the exit needs no phone-specific path.
use crate::forward::ForwardRoute;
use oink_audio::{negotiation::Profile, route::Version, session::Source};
use oink_foundation::{DeviceId, session::Boot};
use oink_mobile_core::MobileNetwork;
use oink_network::{link::StreamId, wire_identity::boot};
use oink_protocol::v2::{Message, dto};

pub(crate) struct SourceTransport {
    source: Source,
    route: Option<(Boot, Version, StreamId)>,
    refused: Option<u64>,
    unavailable: bool,
}
impl SourceTransport {
    pub fn new(local: Boot) -> Self {
        Self {
            source: Source::new(boot(local)),
            route: None,
            refused: None,
            unavailable: false,
        }
    }
    pub fn reconcile(
        &mut self,
        network: &MobileNetwork,
        peers: &[(Boot, StreamId)],
        target: Option<(DeviceId, Version)>,
    ) {
        let route = target.and_then(|(target, version)| {
            peers
                .iter()
                .find(|(peer, _)| peer.device_id == target)
                .map(|(peer, stream)| (*peer, version, *stream))
        });
        if self.route != route {
            self.source.retire();
            self.refused = None;
            self.unavailable = false;
            self.route = route;
        }
        let Some((peer, version, stream)) = route else {
            return;
        };
        if self.refused.is_some_and(|sequence| {
            network
                .peer_facts(peer.device_id)
                .and_then(|facts| facts.fact_sequence().ok())
                == Some(sequence)
        }) || self.source.offer().is_some()
        {
            return;
        }
        if self.source.begin(boot(peer), version.to_wire()).is_err() {
            return;
        }
        let message = Message::AudioSourceRouteOffer(self.source.offer().expect("begun").clone());
        if network.send_audio_message(peer, stream, &message).is_err() {
            self.source.retire();
        }
    }
    pub fn receive(&mut self, network: &MobileNetwork, peer: Boot, message: &Message) -> Option<Message> {
        if self.route.is_none_or(|(current, _, _)| current != peer) {
            return None;
        }
        match message {
            Message::AudioPrepared(prepared)
                if prepared
                    .descriptor()
                    .is_ok_and(|d| Profile::from_descriptor(&d) == Ok(Profile::Opus5ms)) =>
            {
                let output = prepared.session().ok()?.receiver_output().ok()?;
                if let Some(version) = network
                    .peer_facts(peer.device_id)
                    .and_then(|facts| facts.exit_volume().ok().flatten())
                    .and_then(|state| state.version().ok())
                    && (output.receiver() != version.exit()
                        || output.generation().ok() < version.output_generation().ok()
                        || (output.generation() == version.output_generation()
                            && output.output_id() != version.output_id()))
                {
                    return None;
                }
                if let Ok(ack) = self.source.accept(&boot(peer), prepared.clone()) {
                    let (_, _, stream) = self.route?;
                    if network
                        .send_audio_message(peer, stream, &Message::AudioPreparedAck(ack))
                        .is_err()
                    {
                        self.source.retire();
                    } else {
                        self.unavailable = false;
                        self.refused = None;
                    }
                }
                None
            }
            Message::AudioPrepareRefused(refused) if refused.offer().ok().as_ref() == self.source.offer() => {
                self.unavailable = !matches!(
                    refused.reason(),
                    Ok(dto::ControlFailure::NotExit
                        | dto::ControlFailure::RouteChanged
                        | dto::ControlFailure::OutputChanged)
                );
                self.source.retire();
                self.refused = Some(
                    network
                        .peer_facts(peer.device_id)
                        .and_then(|facts| facts.fact_sequence().ok())
                        .unwrap_or(0),
                );
                None
            }
            Message::AudioRouteFault(fault)
                if self
                    .source
                    .prepared()
                    .is_some_and(|prepared| fault.session() == Ok(prepared.session())) =>
            {
                self.unavailable = true;
                self.source.retire();
                self.refused = Some(
                    network
                        .peer_facts(peer.device_id)
                        .and_then(|facts| facts.fact_sequence().ok())
                        .unwrap_or(0),
                );
                None
            }
            _ => None,
        }
    }
    /// A pending M0 stream/preparation is not a native capture failure.
    pub fn failed(&self) -> bool {
        self.unavailable
    }
    pub fn route(&self, network: &MobileNetwork) -> Option<ForwardRoute> {
        let (_, _, stream) = self.route?;
        Some(ForwardRoute {
            stream: self.source.prepared()?.stream_id(),
            sender: network.audio_datagram_sender(stream)?,
        })
    }
    pub fn session(&self) -> Option<dto::AudioSessionContext> {
        self.source.prepared().map(|p| p.session())
    }
}
