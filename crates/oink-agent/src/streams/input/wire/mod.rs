//! The input owner's v1 boundary. Transport admission precedes every DTO projection.
use oink_foundation::session::Boot;
use oink_protocol::v2::{
    self,
    admission::{Agreement, Context, Phase},
    frame::OpaqueFrame,
    registry::{Transport, type_id},
};
use oink_routing::Packet;
use oink_routing::wire::{control, input};
#[cfg(test)]
use oink_routing::{control::ControlMsg, input::InputMsg};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PeerAdmission {
    pub boot: Boot,
    pub link: oink_network::link::LinkId,
    pub agreement: Arc<Agreement>,
}
impl PeerAdmission {
    pub fn relative(&self) -> bool {
        self.agreement.permits(type_id::InputRelativeMotion)
    }
    pub fn drag(&self) -> bool {
        self.agreement.permits(type_id::DndNotice)
    }
    pub fn contacts(&self) -> bool {
        self.agreement.permits(type_id::InputTouchpadContacts)
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
    pub fn violation(&self, type_id: Option<u16>, code: v2::ErrorCode, datagram: bool) -> v2::admission::Violation {
        let context = self.context(datagram);
        if let Some(type_id) = type_id
            && let Err(violation) = context.admit(v2::frame::FrameHeader {
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
            .admit(v2::frame::FrameHeader {
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
    Deferred(v2::Message),
}

pub(crate) fn decode(bytes: &[u8], peer: &PeerAdmission, datagram: bool) -> v2::Result<Decoded> {
    let maximum = if datagram {
        v2::limits::DATAGRAM
    } else {
        v2::limits::RELIABLE_FRAME
    };
    let frame = OpaqueFrame::parse_exact(bytes, maximum)?;
    peer.context(datagram)
        .admit_frame(frame)
        .map_err(|violation| v2::Error::from(violation.code))?;
    if frame.header.type_id == 0x020d {
        return input::decode_text(frame.decode()?).map(Decoded::Packet);
    }
    if (0x0201..=0x02ff).contains(&frame.header.type_id) {
        return input::decode(frame).map(Decoded::Packet);
    }
    let message = frame.decode()?;
    match message {
        v2::Message::DndNotice(_) => Ok(Decoded::Deferred(message)),
        v2::Message::AttachmentHead(ref head) => {
            if head.header()?.variant()? != v2::schema::Open::Known(v2::dto::AttachmentKind::Drag) {
                return Err(v2::ErrorCode::InvalidChannel.into());
            }
            Ok(Decoded::Deferred(message))
        }
        v2::Message::AttachmentResult(ref result) => {
            if result.kind()? != v2::dto::AttachmentKind::Drag {
                return Err(v2::ErrorCode::InvalidChannel.into());
            }
            Ok(Decoded::Deferred(message))
        }
        _ if (0x0100..=0x010f).contains(&frame.header.type_id)
            || matches!(
                frame.header.type_id,
                0x1300 | 0x1301 | 0x1302 | 0x1303 | 0x1304 | 0x1305 | 0x0111 | 0x0112 | 0x0113 | 0x0114
            ) =>
        {
            control::decode(message).map(|value| Decoded::Packet(Packet::Control(value)))
        }
        _ => crate::dnd::wire::decode(message).map(|value| Decoded::Packet(Packet::Dnd(value))),
    }
}
pub(crate) fn encode(message: &Packet, out: &mut [u8]) -> v2::Result<usize> {
    match message {
        Packet::Input(input) => input::encode(input, None, out),
        Packet::TimedInput { time, input } => input::encode(input, Some(*time), out),
        Packet::Control(value) => encode_wire(&control::encode(value)?, out),
        Packet::Dnd(value) => encode_wire(&crate::dnd::wire::encode(value)?, out),
    }
}
pub(crate) fn encode_wire(message: &v2::Message, out: &mut [u8]) -> v2::Result<usize> {
    let bytes = v2::frame::encode(message, out.len())?;
    out[..bytes.len()].copy_from_slice(&bytes);
    Ok(bytes.len())
}
/// Whether the peer's agreement carries `message`: the registry rule the
/// send gate applies (protocol §1.1), never a capability named here.
pub(crate) fn supported(message: &Packet, peer: &PeerAdmission) -> bool {
    let type_id = match message {
        Packet::Input(input) | Packet::TimedInput { input, .. } => input::type_id(input),
        Packet::Control(value) => match control::encode(value) {
            Ok(message) => message.type_id(),
            Err(_) => return false,
        },
        Packet::Dnd(value) => match crate::dnd::wire::encode(value) {
            Ok(message) => message.type_id(),
            Err(_) => return false,
        },
    };
    peer.agreement.permits(type_id)
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
            let mut caps = vec!["oink.core/2".to_owned()];
            if capable {
                caps.push("oink.input/2".to_owned());
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
                    wheel_steps: None,
                };
                let allowed = capable;
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
            let mut caps = vec!["oink.core/2".to_owned()];
            if capable {
                caps.push("oink.input/2".to_owned());
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
            let mut caps = vec!["oink.core/2".to_owned()];
            if capable {
                caps.push("oink.input/2".to_owned());
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

#[cfg(test)]
mod windows_lock_tests {
    use super::*;
    #[test]
    fn lock_request_roundtrips_and_old_agreements_never_send_the_new_type() {
        let source = Boot {
            device_id: oink_foundation::DeviceId([1; 32]),
            nonce: [2; 16],
        };
        let target = Boot {
            device_id: oink_foundation::DeviceId([3; 32]),
            nonce: [4; 16],
        };
        let packet = Packet::Control(ControlMsg::LockRemoteWindows(
            oink_routing::control::WindowsLockRequest {
                source,
                target,
                epoch: 27,
                id: 5,
            },
        ));
        for capable in [false, true] {
            let mut caps = vec!["oink.core/2".to_owned()];
            if capable {
                caps.push("oink.input/2".into());
            }
            let peer = PeerAdmission {
                boot: source,
                link: oink_network::link::LinkId(1),
                agreement: Arc::new(Agreement::new(caps).unwrap()),
            };
            assert_eq!(supported(&packet, &peer), capable);
            let mut bytes = [0; 512];
            let n = encode(&packet, &mut bytes).unwrap();
            if capable {
                assert!(matches!(decode(&bytes[..n],&peer,false).unwrap(), Decoded::Packet(p) if p==packet));
            } else {
                assert!(decode(&bytes[..n], &peer, false).is_err());
            }
            // With the merged input domain, an older agreement has no input
            // stream at all, so ordinary input is rejected with the new type.
            let key = Packet::Input(InputMsg::Key {
                code: oink_foundation::input::KeyCode::keyboard(4),
                pressed: true,
            });
            assert_eq!(supported(&key, &peer), capable);
            let n = encode(&key, &mut bytes).unwrap();
            if capable {
                assert!(matches!(decode(&bytes[..n],&peer,false).unwrap(),Decoded::Packet(p) if p==key));
            } else {
                assert!(decode(&bytes[..n], &peer, false).is_err());
            }
        }
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    #[test]
    fn retired_input_is_discarded_and_next_ordinary_key_decodes() {
        let peer = PeerAdmission {
            boot: Boot {
                device_id: oink_foundation::DeviceId([1; 32]),
                nonce: [1; 16],
            },
            link: oink_network::link::LinkId(1),
            agreement: Arc::new(
                Agreement::new(
                    v2::registry::BASELINE
                        .iter()
                        .copied()
                        .chain(["oink.input/2"])
                        .map(str::to_owned),
                )
                .unwrap(),
            ),
        };
        assert!(Agreement::new(["oink.input.application-focus/1".into()]).is_err());
        let key = Packet::Input(InputMsg::Key {
            code: oink_foundation::input::KeyCode::CAPS_LOCK,
            pressed: true,
        });
        let mut bytes = [0; 1024];
        let n = encode(&key, &mut bytes).unwrap();
        assert!(matches!(decode(&bytes[..n], &peer, false).unwrap(), Decoded::Packet(found) if found == key));
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use oink_foundation::{DeviceId, Point};
    use oink_routing::control::{EntryProtection, FocusLock, FocusScope, GameMode, TrailContinuation};
    use std::collections::BTreeSet;

    fn samples() -> Vec<ControlMsg> {
        let device = DeviceId([1; 32]);
        let boot = Boot {
            device_id: device,
            nonce: [2; 16],
        };
        let scope = FocusScope {
            members: vec![(device, [3; 16])],
        };
        let lock = FocusLock {
            revision: 1,
            author: device,
            epoch: 1,
            holder: device,
            session: [3; 16],
            scope: scope.clone(),
            locked: true,
        };
        vec![
            ControlMsg::UnlockQuery { epoch: 1 },
            ControlMsg::SyncUnlock { epoch: 1 },
            ControlMsg::UnlockReply {
                epoch: 1,
                unlocked: true,
                paused: true,
            },
            ControlMsg::LocalClick {
                epoch: 1,
                holder: device,
                scope: scope.clone(),
                phase: 0,
            },
            ControlMsg::EntryProtection {
                state: EntryProtection::default(),
            },
            ControlMsg::LockEscape { held: true },
            ControlMsg::TextRefused {
                seq: 3,
                reason: oink_routing::control::TextRefusal::Paused,
            },
            ControlMsg::TextFocus {
                epoch: 1,
                seq: 2,
                editable: true,
                secure: false,
                revision: 3,
            },
            ControlMsg::EdgeReport {
                epoch: 1,
                state: oink_routing::control::EdgeState::Blocked,
                edge: oink_routing::control::EdgeSide::Left,
                target: Some(device),
            },
            ControlMsg::MovePointer {
                epoch: 1,
                target: device,
            },
            ControlMsg::Feel {
                profile: Default::default(),
            },
            ControlMsg::FocusEnter {
                epoch: 1,
                display: "screen".into(),
                pos: Point::new(0.5, 0.5),
                scope: Some(scope.clone()),
                pointer_source: None,
            },
            ControlMsg::FocusState {
                epoch: 1,
                holder: device,
                scope: Some(scope.clone()),
                session: Some([3; 16]),
                lock_state: Some(lock.clone()),
            },
            ControlMsg::SetFocusLock {
                epoch: 1,
                locked: Some(true),
                target: Some(device),
            },
            ControlMsg::FocusLock { state: lock },
            ControlMsg::GameMode {
                epoch: 1,
                generation: 1,
                mode: GameMode::Auto,
                detected: false,
                active: false,
                cursor_hiding_cause_available: None,
            },
            ControlMsg::SetGameMode {
                epoch: 1,
                mode: GameMode::On,
            },
            ControlMsg::FocusResume {
                epoch: 1,
                holder: device,
                scope: scope.clone(),
            },
            ControlMsg::CursorTrail { active: true },
            ControlMsg::UserActivity,
            ControlMsg::CursorTrailHandoff {
                epoch: 1,
                sources: vec![device],
                state: TrailContinuation {
                    shown_secs: 0.,
                    size_scale: 1.,
                    native_units_per_reference: 1.,
                    emit_carry: 0.,
                    heading: Point::new(1., 0.),
                    points: vec![],
                },
            },
            ControlMsg::InputCapability {
                controllable: true,
                touchpad_contacts: None,
            },
            ControlMsg::LockRemoteWindows(oink_routing::control::WindowsLockRequest {
                source: boot,
                target: boot,
                epoch: 1,
                id: 1,
            }),
            ControlMsg::MovePointerEnter {
                epoch: 1,
                display: "hidden".into(),
                pos: Point::new(-200., 350.),
                scope: Some(scope),
                pointer_source: Some(device),
            },
        ]
    }

    /// The top-level input decoder chooses the control decoder from a type-id
    /// list. A control message missing from it falls into the drag decoder and
    /// faults the stream (0x1302 once did), so every I0 control type must decode
    /// as control through the same entry point the stream uses.
    #[test]
    fn every_input_stream_control_type_dispatches_to_the_control_decoder() {
        let peer = PeerAdmission {
            boot: Boot {
                device_id: DeviceId([1; 32]),
                nonce: [1; 16],
            },
            link: oink_network::link::LinkId(1),
            agreement: Arc::new(Agreement::new(v2::registry::CAPABILITIES.iter().map(|s| s.to_string())).unwrap()),
        };
        let mut covered = BTreeSet::new();
        for sample in samples() {
            let packet = Packet::Control(sample);
            let mut out = [0u8; 4096];
            let n = encode(&packet, &mut out).unwrap();
            let type_id = OpaqueFrame::parse_exact(&out[..n], v2::limits::RELIABLE_FRAME)
                .unwrap()
                .header
                .type_id;
            covered.insert(type_id);
            let decoded = decode(&out[..n], &peer, false);
            assert!(
                matches!(
                    (&decoded, &packet),
                    (Ok(Decoded::Packet(Packet::Control(found))), Packet::Control(sent))
                        if std::mem::discriminant(found) == std::mem::discriminant(sent)
                ),
                "type {type_id:#06x} did not decode as its control message"
            );
        }
        // Every control type the registry routes on the input stream has a sample.
        let registered: BTreeSet<u16> = v2::registry::MESSAGES
            .iter()
            .filter(|m| m.routes.iter().any(|r| r.kind == 3))
            .filter(|m| !(0x0201..=0x02ff).contains(&m.id))
            .filter(|m| !m.name.starts_with("Dnd") && !m.name.starts_with("Attachment"))
            .map(|m| m.id)
            .collect();
        assert_eq!(covered, registered);
    }

    #[test]
    fn sync_unlock_is_never_sent_to_a_peer_without_the_capability() {
        let admission = |capabilities: &[&str]| PeerAdmission {
            boot: Boot {
                device_id: DeviceId([1; 32]),
                nonce: [1; 16],
            },
            link: oink_network::link::LinkId(1),
            agreement: Arc::new(Agreement::new(capabilities.iter().map(|s| s.to_string())).unwrap()),
        };
        // v2: sync unlock is part of the device domain, like its challenge
        // pair; a peer without oink.device/2 (an input-only phone) gets none.
        let base = ["oink.core/2", "oink.group/2", "oink.input/2", "oink.low/1"];
        let old = admission(&base);
        let new = admission(&[&base[..], &["oink.device/2"]].concat());
        let sync = Packet::Control(ControlMsg::SyncUnlock { epoch: 1 });
        assert!(!supported(&sync, &old));
        assert!(supported(&sync, &new));
        let query = Packet::Control(ControlMsg::UnlockQuery { epoch: 1 });
        assert_eq!(supported(&query, &old), supported(&sync, &old));
        assert_eq!(supported(&query, &new), supported(&sync, &new));
    }
}
