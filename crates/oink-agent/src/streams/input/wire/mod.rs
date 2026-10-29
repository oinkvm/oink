//! The input owner's v1 boundary. Transport admission precedes every DTO projection.
mod application_focus;
mod control;
mod input;
use oink_foundation::session::Boot;
use oink_protocol::v1::{
    self,
    admission::{Agreement, Context, Phase},
    frame::OpaqueFrame,
    registry::Transport,
};
use oink_routing::{Packet, control::ControlMsg, input::InputMsg};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PeerAdmission {
    pub boot: Boot,
    pub link: oink_network::link::LinkId,
    pub agreement: Arc<Agreement>,
}
impl PeerAdmission {
    pub fn application_focus(&self) -> bool {
        self.agreement.contains("oink.input.application-focus/1")
    }
    pub fn relative(&self) -> bool {
        self.agreement.contains("oink.input.relative/1")
    }
    pub fn drag(&self) -> bool {
        self.agreement.contains("oink.drag/1")
    }
    pub fn contacts(&self) -> bool {
        self.agreement.contains("oink.input.contacts/1")
    }
    pub fn opening_len(&self) -> usize {
        3 + usize::from(self.relative()) + usize::from(self.drag())
    }
    pub fn opening_index(&self, index: usize) -> Option<usize> {
        [
            Some(0),
            Some(1),
            Some(2),
            self.relative().then_some(3),
            self.drag().then_some(4),
        ]
        .into_iter()
        .flatten()
        .nth(index)
    }
    fn context(&self, datagram: bool) -> Context<'_> {
        Context {
            kind: 3,
            transport: if datagram {
                Transport::Datagram
            } else {
                Transport::Reliable
            },
            phase: Phase::Member,
            agreement: &self.agreement,
            direct: true,
            old_member_lane: false,
        }
    }
    pub fn violation(&self, type_id: Option<u16>, code: v1::ErrorCode, datagram: bool) -> v1::admission::Violation {
        let context = self.context(datagram);
        if let Some(type_id) = type_id
            && let Err(violation) = context.admit(v1::frame::FrameHeader {
                type_id,
                payload_length: 1,
            })
        {
            return violation;
        }
        let mut violation = context.payload_violation(type_id.unwrap_or(0), code);
        violation.type_id = type_id;
        violation
    }
    pub fn admits(&self, type_id: u16, datagram: bool) -> bool {
        self.context(datagram)
            .admit(v1::frame::FrameHeader {
                type_id,
                payload_length: 1,
            })
            .is_ok()
    }
}
// Keep ordinary input decoding allocation-free; deferred records are the rare path.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Decoded {
    Packet(Packet),
    /// Drag inventories and attachment declarations stay retained until their ordinary owner consumes them.
    Deferred(v1::Message),
}

pub(crate) fn decode(bytes: &[u8], peer: &PeerAdmission, datagram: bool) -> v1::Result<Decoded> {
    let maximum = if datagram {
        v1::limits::DATAGRAM
    } else {
        v1::limits::RELIABLE_FRAME
    };
    let frame = OpaqueFrame::parse_exact(bytes, maximum)?;
    peer.context(datagram)
        .admit_frame(frame)
        .map_err(|violation| v1::Error::from(violation.code))?;
    if (0x0201..=0x02ff).contains(&frame.header.type_id) {
        return input::decode(frame).map(Decoded::Packet);
    }
    let message = frame.decode()?;
    match message {
        v1::Message::DndNotice(_) => Ok(Decoded::Deferred(message)),
        v1::Message::AttachmentHead(ref head) => {
            if head.header()?.variant()? != v1::schema::Open::Known(v1::dto::AttachmentKind::Drag) {
                return Err(v1::ErrorCode::InvalidChannel.into());
            }
            Ok(Decoded::Deferred(message))
        }
        v1::Message::AttachmentResult(ref result) => {
            if result.kind()? != v1::dto::AttachmentKind::Drag {
                return Err(v1::ErrorCode::InvalidChannel.into());
            }
            Ok(Decoded::Deferred(message))
        }
        _ if (0x0100..=0x010f).contains(&frame.header.type_id) || frame.header.type_id == 0x1300 => {
            control::decode(message).map(|value| Decoded::Packet(Packet::Control(value)))
        }
        _ => crate::dnd::wire::decode(message).map(|value| Decoded::Packet(Packet::Dnd(value))),
    }
}
pub(crate) fn encode(message: &Packet, out: &mut [u8]) -> v1::Result<usize> {
    match message {
        Packet::Input(input) => input::encode(input, None, out),
        Packet::TimedInput { time, input } => input::encode(input, Some(*time), out),
        Packet::Control(value) => encode_wire(&control::encode(value)?, out),
        Packet::Dnd(value) => encode_wire(&crate::dnd::wire::encode(value)?, out),
    }
}
pub(crate) fn encode_wire(message: &v1::Message, out: &mut [u8]) -> v1::Result<usize> {
    let bytes = v1::frame::encode(message, out.len())?;
    out[..bytes.len()].copy_from_slice(&bytes);
    Ok(bytes.len())
}
pub(crate) fn supported(message: &Packet, peer: &PeerAdmission) -> bool {
    let capability = match message {
        Packet::Control(ControlMsg::ApplicationFocus(_)) => "oink.input.application-focus/1",
        Packet::Input(input) | Packet::TimedInput { input, .. } => return input_supported(input, peer),
        Packet::Control(ControlMsg::GameMode { .. } | ControlMsg::SetGameMode { .. }) => "oink.input.relative/1",
        Packet::Control(ControlMsg::CursorTrail { .. } | ControlMsg::CursorTrailHandoff { .. }) => "oink.input.trail/1",
        Packet::Dnd(_) => "oink.drag/1",
        Packet::Control(ControlMsg::MovePointer { .. }) => "oink.pointer.move/1",
        Packet::Control(ControlMsg::EntryProtection { .. }) => "oink.pointer.entry-protection/1",
        Packet::Control(ControlMsg::LockEscape { .. }) => "oink.pointer.lock-escape/1",
        _ => "oink.input/1",
    };
    peer.agreement.contains(capability)
}
fn input_supported(input: &InputMsg, peer: &PeerAdmission) -> bool {
    let capability = match input {
        InputMsg::ApplicationKey { .. } | InputMsg::ApplicationRepeat { .. } | InputMsg::ApplicationHeldSync { .. } => {
            "oink.input.application-focus/1"
        }
        InputMsg::Forwarded { msg, .. } => return input_supported(msg, peer),
        InputMsg::RelativeMotion { .. } => "oink.input.relative/1",
        InputMsg::Gesture { .. } => "oink.input.gestures/1",
        InputMsg::TouchpadContacts(_) => "oink.input.contacts/1",
        InputMsg::Scroll {
            unit: oink_routing::input::ScrollAmount::Pixels,
            ..
        } => "oink.input.scroll-pixels/1",
        InputMsg::Volume { .. } => "oink.audio.exit-volume/1",
        _ => "oink.input/1",
    };
    peer.agreement.contains(capability)
}

#[cfg(test)]
mod scroll_pixel_tests {
    use super::*;

    #[test]
    fn source_pixel_scroll_requires_capability_even_when_forwarded() {
        let boot = Boot {
            device_id: oink_foundation::DeviceId([1; 32]),
            nonce: [1; 16],
        };
        for capable in [false, true] {
            let mut caps = vec!["oink.core/1".to_owned(), "oink.input/1".to_owned()];
            if capable {
                caps.push("oink.input.scroll-pixels/1".to_owned());
            }
            let peer = PeerAdmission {
                boot,
                link: oink_network::link::LinkId(1),
                agreement: Arc::new(Agreement::new(caps).unwrap()),
            };
            for unit in [
                oink_routing::input::ScrollAmount::Pixels,
                oink_routing::input::ScrollAmount::Reference,
            ] {
                let input = InputMsg::Scroll {
                    source: oink_foundation::input::SourceKind::Touchpad,
                    dx: 12.,
                    dy: -37.,
                    unit,
                    phase: oink_foundation::input::ScrollPhase::Began,
                };
                let allowed = capable || unit == oink_routing::input::ScrollAmount::Reference;
                assert_eq!(supported(&Packet::Input(input.clone()), &peer), allowed);
                assert_eq!(
                    supported(
                        &Packet::Input(InputMsg::Forwarded {
                            origin: boot,
                            hops: 1,
                            msg: Box::new(input),
                        }),
                        &peer
                    ),
                    allowed
                );
            }
        }
    }
}

#[cfg(test)]
mod lock_escape_tests {
    use super::*;

    #[test]
    fn escape_snapshot_roundtrips_only_with_negotiated_support() {
        for capable in [false, true] {
            let mut caps = vec!["oink.core/1".to_owned(), "oink.input/1".to_owned()];
            if capable {
                caps.push("oink.pointer.lock-escape/1".to_owned());
            }
            let peer = PeerAdmission {
                boot: Boot {
                    device_id: oink_foundation::DeviceId([1; 32]),
                    nonce: [1; 16],
                },
                link: oink_network::link::LinkId(1),
                agreement: Arc::new(Agreement::new(caps).unwrap()),
            };
            for held in [false, true] {
                let packet = Packet::Control(ControlMsg::LockEscape { held });
                assert_eq!(supported(&packet, &peer), capable);
                let mut out = [0u8; 128];
                let len = encode(&packet, &mut out).unwrap();
                let decoded = decode(&out[..len], &peer, false);
                if capable {
                    assert!(matches!(decoded.unwrap(), Decoded::Packet(value) if value == packet));
                } else {
                    assert!(decoded.is_err());
                }
            }
        }
    }
}

#[cfg(test)]
mod entry_protection_tests {
    use super::*;
    #[test]
    fn entry_protection_roundtrips_only_with_negotiated_support() {
        for capable in [false, true] {
            let mut caps = vec!["oink.core/1".to_owned(), "oink.input/1".to_owned()];
            if capable {
                caps.push("oink.pointer.entry-protection/1".to_owned());
            }
            let peer = PeerAdmission {
                boot: Boot {
                    device_id: oink_foundation::DeviceId([1; 32]),
                    nonce: [1; 16],
                },
                link: oink_network::link::LinkId(1),
                agreement: Arc::new(Agreement::new(caps).unwrap()),
            };
            let packet = Packet::Control(ControlMsg::EntryProtection {
                state: oink_routing::control::EntryProtection {
                    locked: true,
                    game: false,
                    passive_displays: vec!["negative-origin-monitor".into()],
                },
            });
            assert_eq!(supported(&packet, &peer), capable);
            let mut out = [0u8; 1024];
            let len = encode(&packet, &mut out).unwrap();
            let decoded = decode(&out[..len], &peer, false);
            if capable {
                assert!(matches!(decoded.unwrap(), Decoded::Packet(value) if value == packet));
            } else {
                assert!(decoded.is_err());
            }
        }
    }
}
