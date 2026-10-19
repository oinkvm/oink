//! Exact-frame negotiation shared by physical and relayed logical connections.
use crate::wire_identity as identity;
use oink_foundation::{
    DeviceId,
    session::{Boot, ConnectionId},
};
use oink_protocol::v1::{
    self, Message, RawValue,
    admission::{self, Agreement},
    dto, frame,
    schema::{WireDto, WireValue},
};
use std::time::{Duration, Instant};

pub const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);

/// Only capabilities and media formats implemented by the application are offered.
#[derive(Clone, Debug)]
pub struct OfferProfile {
    pub capabilities: Vec<String>,
    pub required_capabilities: Vec<String>,
    pub formats: dto::FormatSupport,
    pub audio: Vec<dto::AudioSupport>,
}
impl Default for OfferProfile {
    fn default() -> Self {
        Self {
            capabilities: v1::registry::BASELINE
                .iter()
                .copied()
                .chain(["oink.pairing/1", "oink.relay/1"])
                .map(str::to_owned)
                .collect(),
            required_capabilities: Vec::new(),
            formats: dto::FormatSupport::from_raw((1..=4).map(|k| (k, RawValue::Array(Vec::new()))).collect())
                .expect("empty formats"),
            audio: Vec::new(),
        }
    }
}
impl OfferProfile {
    pub fn offer(
        &self,
        boot: Boot,
        connection: ConnectionId,
        relay: Option<dto::RelayContext>,
    ) -> v1::Result<dto::ConnectionOffer> {
        let sorted = |strings: &[String]| {
            let mut strings = strings.to_vec();
            strings.sort();
            strings.dedup();
            RawValue::Array(strings.into_iter().map(RawValue::Text).collect())
        };
        let strings = |items: &[&str]| RawValue::Array(items.iter().map(|s| RawValue::from(*s)).collect());
        let mut raw = [
            (1, identity::boot(boot).write()),
            (2, identity::connection(connection).write()),
            (
                3,
                RawValue::Array(vec![RawValue::Map(
                    [(1, v1::MAJOR.write()), (2, v1::MINOR.write())].into(),
                )]),
            ),
            (4, sorted(&self.capabilities)),
            (5, sorted(&self.required_capabilities)),
            (6, dto::Limits::baseline().write()),
            (
                7,
                dto::CryptoOffers::from_raw(
                    [
                        (1, strings(&[dto::PAIRING_SUITE])),
                        (2, strings(&[dto::ADMISSION_SUITE])),
                        (3, strings(&[dto::ED25519])),
                    ]
                    .into(),
                )?
                .write(),
            ),
            (8, RawValue::Bytes(rand::random::<[u8; 32]>().to_vec())),
            (9, RawValue::from(env!("CARGO_PKG_VERSION"))),
            (10, self.formats.write()),
            (11, RawValue::Array(self.audio.iter().map(WireValue::write).collect())),
        ]
        .into_iter()
        .collect::<v1::RawMap>();
        if let Some(relay) = relay {
            raw.insert(12, relay.write());
        }
        dto::ConnectionOffer::from_raw(raw)
    }
}

#[derive(Clone, Debug)]
pub struct Selected {
    pub peer_boot: Boot,
    pub connection: ConnectionId,
    pub agreement: Agreement,
    pub negotiated: dto::Negotiated,
    pub local_offer: Vec<u8>,
    pub peer_offer: Vec<u8>,
}

impl Selected {
    pub fn peer_audio(&self) -> v1::Result<Vec<dto::AudioSupport>> {
        match frame::OpaqueFrame::parse_exact(&self.peer_offer, v1::limits::CONFIG_FRAME)?.decode()? {
            Message::ConnectionOffer(offer) => Ok(offer.audio()?.0),
            _ => Err(v1::ErrorCode::MalformedFrame.into()),
        }
    }

    pub fn peer_formats(&self) -> v1::Result<dto::FormatSupport> {
        match frame::OpaqueFrame::parse_exact(&self.peer_offer, v1::limits::CONFIG_FRAME)?.decode()? {
            Message::ConnectionOffer(offer) => offer.formats(),
            _ => Err(v1::ErrorCode::MalformedFrame.into()),
        }
    }
}

/// A candidate is invisible to connection selection until both independent
/// computations agree. Human pairing starts after this bounded bootstrap.
#[derive(Debug)]
pub(crate) struct Bootstrap {
    local: Boot,
    peer: DeviceId,
    outbound: bool,
    profile: OfferProfile,
    local_offer: Option<Vec<u8>>,
    peer_offer: Option<Vec<u8>>,
    selected: Option<Selected>,
    complete: bool,
    pub deadline: Instant,
}
impl Bootstrap {
    pub fn new(
        local: Boot,
        peer: DeviceId,
        connection: Option<ConnectionId>,
        profile: OfferProfile,
        now: Instant,
    ) -> v1::Result<(Self, Option<Vec<u8>>)> {
        let local_offer = connection
            .map(|id| {
                profile
                    .offer(local, id, None)
                    .and_then(|o| frame::encode(&Message::ConnectionOffer(o), v1::limits::RELIABLE_FRAME))
            })
            .transpose()?;
        Ok((
            Self {
                local,
                peer,
                outbound: connection.is_some(),
                profile,
                local_offer: local_offer.clone(),
                peer_offer: None,
                selected: None,
                complete: false,
                deadline: now + BOOTSTRAP_TIMEOUT,
            },
            local_offer,
        ))
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn selected(&self) -> Option<&Selected> {
        self.complete.then_some(self.selected.as_ref()).flatten()
    }
    pub fn receive(&mut self, bytes: &[u8], now: Instant) -> v1::Result<Vec<Vec<u8>>> {
        if self.complete || now >= self.deadline {
            return Err(v1::ErrorCode::NotReady.into());
        }
        let message = frame::OpaqueFrame::parse_exact(bytes, v1::limits::RELIABLE_FRAME)?.decode()?;
        match message {
            Message::ConnectionOffer(offer) => {
                if self.peer_offer.is_some() || offer.relay_context()?.is_some() {
                    return Err(v1::ErrorCode::NegotiationMismatch.into());
                }
                let peer_boot = identity::read_boot(&offer.boot()?)?;
                if peer_boot.device_id != self.peer {
                    return Err(v1::ErrorCode::NegotiationMismatch.into());
                }
                let connection = identity::read_connection(&offer.connection_id()?)?;
                if connection.initiator_boot != if self.outbound { self.local } else { peer_boot } {
                    return Err(v1::ErrorCode::NegotiationMismatch.into());
                }
                let mut output = Vec::new();
                if self.local_offer.is_none() {
                    let local = frame::encode(
                        &Message::ConnectionOffer(self.profile.offer(self.local, connection, None)?),
                        v1::limits::RELIABLE_FRAME,
                    )?;
                    self.local_offer = Some(local.clone());
                    output.push(local);
                }
                let local = self.local_offer.as_ref().expect("local offer exists");
                let (a, b) = if self.outbound {
                    (local.as_slice(), bytes)
                } else {
                    (bytes, local.as_slice())
                };
                let implemented = self.profile.capabilities.iter().map(String::as_str).collect::<Vec<_>>();
                let peer_pairing = offer.capabilities()?.0.iter().any(|cap| cap.0 == "oink.pairing/1");
                let negotiated = admission::negotiate(
                    a,
                    b,
                    &implemented,
                    peer_pairing && implemented.contains(&"oink.pairing/1"),
                )?;
                let agreement = Agreement::new(negotiated.enabled()?.0.into_iter().map(|cap| cap.0))?;
                output.push(frame::encode(
                    &Message::Negotiated(negotiated.clone()),
                    v1::limits::RELIABLE_FRAME,
                )?);
                self.peer_offer = Some(bytes.to_vec());
                self.selected = Some(Selected {
                    peer_boot,
                    connection,
                    agreement,
                    negotiated,
                    local_offer: local.clone(),
                    peer_offer: bytes.to_vec(),
                });
                Ok(output)
            }
            Message::Negotiated(peer) => {
                let selected = self.selected.as_ref().ok_or(v1::ErrorCode::NotReady)?;
                if peer != selected.negotiated {
                    return Err(v1::ErrorCode::NegotiationMismatch.into());
                }
                self.complete = true;
                Ok(Vec::new())
            }
            _ => Err(v1::ErrorCode::NotReady.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn boot(n: u8) -> Boot {
        Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        }
    }
    #[test]
    fn both_ends_confirm_exact_offer_frames_before_ready() {
        let now = Instant::now();
        let id = ConnectionId {
            initiator_boot: boot(1),
            random: [3; 16],
        };
        let (mut a, offer) =
            Bootstrap::new(boot(1), boot(2).device_id, Some(id), OfferProfile::default(), now).unwrap();
        let (mut b, _) = Bootstrap::new(boot(2), boot(1).device_id, None, OfferProfile::default(), now).unwrap();
        let replies = b.receive(&offer.unwrap(), now).unwrap();
        let agree = a.receive(&replies[0], now).unwrap();
        assert!(!a.complete());
        assert!(!b.complete());
        a.receive(&replies[1], now).unwrap();
        b.receive(&agree[0], now).unwrap();
        assert!(a.complete() && b.complete());
        assert_eq!(a.selected().unwrap().negotiated, b.selected().unwrap().negotiated);
        assert!(a.selected().unwrap().agreement.contains("oink.pairing/1"));
        assert!(!a.selected().unwrap().agreement.contains("oink.calibration/1"));
    }
    #[test]
    fn wrong_identity_duplicate_offer_and_expired_bootstrap_are_rejected() {
        let now = Instant::now();
        let id = ConnectionId {
            initiator_boot: boot(1),
            random: [3; 16],
        };
        let (_, offer) = Bootstrap::new(boot(1), boot(2).device_id, Some(id), OfferProfile::default(), now).unwrap();
        let offer = offer.unwrap();
        let (mut wrong, _) = Bootstrap::new(boot(2), boot(3).device_id, None, OfferProfile::default(), now).unwrap();
        assert!(wrong.receive(&offer, now).is_err());
        let (mut b, _) = Bootstrap::new(boot(2), boot(1).device_id, None, OfferProfile::default(), now).unwrap();
        b.receive(&offer, now).unwrap();
        assert!(b.receive(&offer, now).is_err());
        let (mut expired, _) = Bootstrap::new(boot(2), boot(1).device_id, None, OfferProfile::default(), now).unwrap();
        assert!(expired.receive(&offer, now + BOOTSTRAP_TIMEOUT).is_err());
    }
}
