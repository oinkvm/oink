//! Untrusted discovery hints. Only the later TLS handshake authenticates a key.
use oink_foundation::{DeviceId, GroupId};
use oink_protocol::v1::{
    self, RawValue, dto,
    schema::{WireDto, WireValue},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolOffer {
    pub major: u16,
    pub highest_minor: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advert {
    pub id: DeviceId,
    pub name: String,
    pub group: GroupId,
    pub count: u8,
    pub port: u16,
    pub protocols: Vec<ProtocolOffer>,
    pub identity_algorithms: Vec<String>,
    pub os: String,
}

impl Advert {
    pub fn local(id: DeviceId, name: String, group: GroupId, count: u8, port: u16, os: String) -> Self {
        Self {
            id,
            name,
            group,
            count,
            port,
            protocols: vec![ProtocolOffer {
                major: v1::MAJOR,
                highest_minor: v1::MINOR,
            }],
            identity_algorithms: vec![dto::ED25519.into()],
            os,
        }
    }

    pub fn to_wire(&self) -> v1::Result<dto::Advert> {
        dto::Advert::from_raw(
            [
                (
                    1,
                    dto::DeviceId::from_raw(
                        [
                            (1, RawValue::Text(dto::ED25519.into())),
                            (2, RawValue::Bytes(self.id.0.to_vec())),
                        ]
                        .into(),
                    )?
                    .write(),
                ),
                (2, RawValue::Text(self.name.clone())),
                (3, RawValue::Bytes(self.group.0.to_vec())),
                (5, self.count.write()),
                (6, self.port.write()),
                (
                    7,
                    RawValue::Array(
                        self.protocols
                            .iter()
                            .map(|p| RawValue::Map([(1, p.major.write()), (2, p.highest_minor.write())].into()))
                            .collect(),
                    ),
                ),
                (
                    8,
                    RawValue::Array(self.identity_algorithms.iter().cloned().map(RawValue::Text).collect()),
                ),
                (9, RawValue::Text(self.os.clone())),
            ]
            .into(),
        )
    }

    pub fn from_wire(advert: &dto::Advert) -> v1::Result<Self> {
        let identity = advert.id()?;
        if identity.algorithm()?.0 != dto::ED25519 {
            return Err(v1::ErrorCode::UnsupportedValue.into());
        }
        Ok(Self {
            id: DeviceId(
                identity
                    .public_key()?
                    .0
                    .try_into()
                    .map_err(|_| v1::ErrorCode::MalformedFrame)?,
            ),
            name: advert.name()?.0,
            group: GroupId(advert.group()?.0),
            count: advert.count()?,
            port: advert.port()?,
            protocols: advert
                .protocols()?
                .0
                .into_iter()
                .map(|p| {
                    Ok(ProtocolOffer {
                        major: p.major()?,
                        highest_minor: p.highest_minor()?,
                    })
                })
                .collect::<v1::Result<_>>()?,
            identity_algorithms: advert.identity_algorithms()?.0.into_iter().map(|a| a.0).collect(),
            os: advert.os()?.0,
        })
    }
}
