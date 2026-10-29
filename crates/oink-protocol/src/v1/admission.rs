//! Pure endpoint admission and negotiation decisions. Transport owners execute
//! the returned cleanup/isolation plan; this module never closes a live link.
use super::{
    ErrorCode, RawValue, Result,
    dto::*,
    frame::{FrameHeader, OpaqueFrame, StreamHeader},
    limits,
    registry::{self, Transport},
    schema::{Ascii, FixedBytes, List, WireDto, WireValue},
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Agreement {
    enabled: BTreeSet<String>,
}
impl Agreement {
    /// Only ids implemented by this codec can become enabled. This constructor
    /// rejects unknown ids instead of treating an echoed offer as support.
    pub fn new(ids: impl IntoIterator<Item = String>) -> Result<Self> {
        let enabled: BTreeSet<String> = ids.into_iter().collect();
        if enabled.iter().any(|id| !registry::CAPABILITIES.contains(&id.as_str())) {
            return Err(ErrorCode::MissingCapability.into());
        }
        Ok(Self { enabled })
    }
    pub fn contains(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }
    pub fn enabled(&self) -> impl Iterator<Item = &str> {
        self.enabled.iter().map(String::as_str)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    PhysicalBootstrap,
    LogicalBootstrap,
    Admission,
    Member,
}
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    pub kind: u16,
    pub transport: Transport,
    pub phase: Phase,
    pub agreement: &'a Agreement,
    pub direct: bool,
    pub old_member_lane: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Violation {
    pub code: ErrorCode,
    pub kind: u16,
    pub type_id: Option<u16>,
    pub capability: Option<&'static str>,
    pub experimental: bool,
    pub isolation: Isolation,
    pub reply: bool,
    pub cleanup: bool,
    pub suppress_reopen: bool,
}
impl Context<'_> {
    /// Allocation-free endpoint path for the same registered input/media DTOs.
    pub fn decode_hot<'a>(&self, frame: OpaqueFrame<'a>) -> std::result::Result<super::hot::Message<'a>, Violation> {
        self.admit_frame(frame)?;
        super::hot::Message::decode(frame).map_err(|error| self.failure(error.code, Some(frame.header.type_id), None))
    }

    /// Destination entry point: admission precedes all payload allocation and a
    /// malformed ProtocolError never generates a reciprocal error response.
    pub fn decode(&self, frame: OpaqueFrame<'_>) -> std::result::Result<super::Message, Violation> {
        self.admit_frame(frame)?;
        let message = frame
            .decode()
            .map_err(|error| self.failure(error.code, Some(frame.header.type_id), None))?;
        let nodes =
            execution_nodes(&message).map_err(|error| self.failure(error.code, Some(frame.header.type_id), None))?;
        // Inventories describe the source selection, including link roots that
        // Windows will omit. Creating/serving link nodes remains capability gated.
        let declaration = matches!(
            message,
            super::Message::ClipboardOffer(_)
                | super::Message::DndNotice(_)
                | super::Message::AttachmentItems(_)
                | super::Message::AttachmentHead(_)
        );
        let links = !declaration && nodes.iter().any(|node| node.symlink_target().ok().flatten().is_some());
        let exec = nodes.iter().any(|node| node.exec_bits().ok().flatten().is_some());
        if exec && !self.agreement.contains("oink.files.exec-bits/1") {
            return Err(self.failure(
                ErrorCode::UnnegotiatedCapability,
                Some(frame.header.type_id),
                Some("oink.files.exec-bits/1"),
            ));
        }
        if links && !self.agreement.contains("oink.files.safe-symlinks/1") {
            return Err(self.failure(
                ErrorCode::UnnegotiatedCapability,
                Some(frame.header.type_id),
                Some("oink.files.safe-symlinks/1"),
            ));
        }
        Ok(message)
    }

    /// Convert a destination owner's payload or execution-semantic rejection
    /// into the same scoped failure used by structural admission.
    pub fn payload_violation(&self, type_id: u16, code: ErrorCode) -> Violation {
        self.failure(code, Some(type_id), None)
    }

    /// Optional execution-bearing key fields require their own negotiated
    /// capability even though the containing key message is baseline input.
    /// Inspect the borrowed map before allocating any retained payload tree.
    pub fn admit_frame(&self, frame: OpaqueFrame<'_>) -> std::result::Result<(), Violation> {
        self.admit(frame.header)?;
        if matches!(frame.header.type_id, 0x0202 | 0x0205 | 0x0207) {
            let present = super::hot::Map::parse(frame.payload(), limits::RELIABLE_FRAME)
                .and_then(|map| {
                    let route = map.field(103)?;
                    if frame.header.type_id == 0x0205 && route.is_none() {
                        map.field(104)
                    } else {
                        Ok(route)
                    }
                })
                .map_err(|error| self.failure(error.code, Some(frame.header.type_id), None))?
                .is_some();
            if present && !self.agreement.contains("oink.input.application-focus/1") {
                return Err(self.failure(
                    ErrorCode::UnnegotiatedCapability,
                    Some(frame.header.type_id),
                    Some("oink.input.application-focus/1"),
                ));
            }
        }
        Ok(())
    }

    fn failure(&self, code: ErrorCode, type_id: Option<u16>, capability: Option<&'static str>) -> Violation {
        Violation {
            code,
            kind: self.kind,
            type_id,
            capability,
            experimental: registry::experimental_stream(self.kind)
                || type_id.is_some_and(registry::experimental_message),
            isolation: if self.transport == Transport::Low {
                Isolation::LowOperation
            } else if self.kind == 0 {
                Isolation::PhysicalConnection
            } else if self.kind == 2 {
                Isolation::LogicalLink
            } else {
                Isolation::Stream
            },
            reply: type_id != Some(1),
            cleanup: true,
            suppress_reopen: code != ErrorCode::NotReady,
        }
    }
    pub fn admit(&self, header: FrameHeader) -> std::result::Result<(), Violation> {
        let id = header.type_id;
        let entry = registry::message(id).ok_or_else(|| self.failure(ErrorCode::UnknownMessage, Some(id), None))?;
        let route = entry
            .routes
            .iter()
            .find(|r| r.kind == self.kind && r.transport == self.transport)
            .ok_or_else(|| self.failure(ErrorCode::InvalidChannel, Some(id), None))?;
        let max = match self.transport {
            Transport::Reliable => {
                if self.kind == 2 {
                    limits::CONFIG_FRAME
                } else {
                    limits::RELIABLE_FRAME
                }
            }
            Transport::Datagram => limits::DATAGRAM - 4,
            Transport::Low => limits::LOW_FRAME,
            Transport::Tcp => {
                if id == 0xc26 {
                    limits::LOW_OUTER_FRAME
                } else {
                    limits::RELIABLE_FRAME
                }
            }
            Transport::Discovery => limits::DISCOVERY - 6,
        };
        if header.payload_length == 0 {
            return Err(self.failure(ErrorCode::MalformedFrame, Some(id), None));
        }
        let max = if (0x0d23..=0x0d2b).contains(&id) {
            max.min(limits::CONTROL_FRAME)
        } else {
            max
        };
        if u64::from(header.payload_length) + 6 > max as u64 {
            return Err(self.failure(ErrorCode::LimitExceeded, Some(id), None));
        }
        if self.transport == Transport::Discovery || id == 1 {
            return Ok(());
        }
        let bootstrap = match self.phase {
            Phase::PhysicalBootstrap => self.kind == 0 && matches!(id, 0xc00 | 2),
            Phase::LogicalBootstrap => self.kind == 2 && matches!(id, 0xd00 | 0xd01),
            _ => false,
        };
        if bootstrap {
            return Ok(());
        }
        if matches!(self.phase, Phase::PhysicalBootstrap | Phase::LogicalBootstrap) {
            return Err(self.failure(ErrorCode::NotReady, Some(id), None));
        }
        for &cap in entry.capabilities.iter().chain(route.extra_capabilities) {
            if !self.agreement.contains(cap) {
                return Err(self.failure(ErrorCode::UnnegotiatedCapability, Some(id), Some(cap)));
            }
        }
        // Pre-member C0 has only admission and the separately authorised pairing /
        // old-root proof lanes. Direct PAKE never inherits relay permission.
        let pairing = matches!(id,0xd02..=0xd04|0xd10..=0xd22);
        let recovery = matches!(id, 0xd1a..=0xd20);
        if pairing && !self.direct && !(recovery && self.old_member_lane) {
            return Err(self.failure(ErrorCode::InvalidChannel, Some(id), None));
        }
        if self.phase == Phase::Admission
            && !(self.kind == 0 || (self.kind == 2 && (matches!(id, 0xd00 | 0xd01 | 0xd05..=0xd07) || pairing)))
        {
            return Err(self.failure(ErrorCode::NotReady, Some(id), None));
        }
        Ok(())
    }
    pub fn admit_stream(&self, header: &StreamHeader) -> std::result::Result<(), Violation> {
        let actual = Self {
            kind: header.kind,
            ..*self
        };
        let entry =
            registry::stream(header.kind).ok_or_else(|| actual.failure(ErrorCode::UnsupportedKind, None, None))?;
        if header.kind != self.kind || header.priority != entry.priority {
            return Err(actual.failure(ErrorCode::InvalidChannel, None, None));
        }
        if !entry.reliable || (header.datagram_flow != 0 && !entry.datagram) {
            return Err(actual.failure(ErrorCode::InvalidChannel, None, None));
        }
        if self.phase == Phase::PhysicalBootstrap && header != &StreamHeader::bootstrap() {
            return Err(actual.failure(ErrorCode::InvalidHeader, None, None));
        }
        if (self.phase == Phase::PhysicalBootstrap && self.kind == 0)
            || (self.phase == Phase::LogicalBootstrap && self.kind == 2)
        {
            return Ok(());
        }
        for &cap in entry.capabilities {
            if !self.agreement.contains(cap) {
                return Err(actual.failure(ErrorCode::UnnegotiatedCapability, None, Some(cap)));
            }
        }
        if self.phase != Phase::Member && self.kind > 2 {
            return Err(actual.failure(ErrorCode::NotReady, None, None));
        }
        Ok(())
    }
    /// Call on the logical destination before allocating a Low lease. A relay
    /// forwards the application kind numerically and does not call this method.
    pub fn admit_low_open(&self, kind: u16) -> std::result::Result<(), Violation> {
        let context = Self {
            kind,
            transport: Transport::Low,
            ..*self
        };
        let entry = registry::stream(kind).ok_or_else(|| context.failure(ErrorCode::UnsupportedKind, None, None))?;
        if !entry.low {
            return Err(context.failure(ErrorCode::InvalidChannel, None, None));
        }
        for &cap in entry.capabilities.iter().chain(["oink.low/1"].iter()) {
            if !self.agreement.contains(cap) {
                return Err(context.failure(ErrorCode::UnnegotiatedCapability, None, Some(cap)));
            }
        }
        if self.phase != Phase::Member {
            return Err(context.failure(ErrorCode::NotReady, None, None));
        }
        Ok(())
    }
}

/// SHA256 of exact Offer frame bytes in initiator/responder order. An opaque
/// relay does not reconstruct these frames or participate in endpoint selection.
pub fn offer_hash(initiator: &[u8], responder: &[u8]) -> Result<Hash32> {
    let mut hash = Sha256::new();
    hash.update(super::labels::NEGOTIATION);
    for frame in [initiator, responder] {
        let parsed = OpaqueFrame::parse_exact(frame, limits::RELIABLE_FRAME)?;
        if parsed.header.type_id != 0xc00 {
            return Err(ErrorCode::InvalidChannel.into());
        }
        let length = u32::try_from(frame.len()).map_err(|_| ErrorCode::LimitExceeded)?;
        hash.update(length.to_le_bytes());
        hash.update(frame);
    }
    Ok(FixedBytes(hash.finalize().into()))
}
/// Baseline suite selection. Implementation support is explicit: an unknown
/// offered capability is never enabled just because both peers echoed it.
pub fn negotiate(initiator: &[u8], responder: &[u8], implemented: &[&str], pairing: bool) -> Result<Negotiated> {
    let read = |bytes| -> Result<ConnectionOffer> {
        match OpaqueFrame::parse_exact(bytes, limits::RELIABLE_FRAME)?.decode()? {
            super::Message::ConnectionOffer(o) => Ok(o),
            _ => Err(ErrorCode::InvalidChannel.into()),
        }
    };
    let a = read(initiator)?;
    let b = read(responder)?;
    if a.connection_id()? != b.connection_id()?
        || a.connection_id()?.initiator_boot()? != a.boot()?
        || a.relay_context()? != b.relay_context()?
    {
        return Err(ErrorCode::NegotiationMismatch.into());
    }
    let pa = a.protocols()?.0;
    let pb = b.protocols()?.0;
    // This codec implements application major 1 only; offered future majors
    // remain DTO data until their complete decoder exists.
    let av = pa
        .iter()
        .find(|p| p.major().ok() == Some(1))
        .ok_or(ErrorCode::UnsupportedMajor)?;
    let bv = pb
        .iter()
        .find(|p| p.major().ok() == Some(1))
        .ok_or(ErrorCode::UnsupportedMajor)?;
    let ca = a.capabilities()?.0;
    let cb = b.capabilities()?.0;
    let enabled: BTreeSet<String> = ca
        .iter()
        .filter(|c| {
            cb.contains(c) && implemented.contains(&c.0.as_str()) && registry::CAPABILITIES.contains(&c.0.as_str())
        })
        .map(|c| c.0.clone())
        .collect();
    for cap in registry::BASELINE.iter().map(|s| s.to_string()).chain(
        a.required_capabilities()?
            .0
            .into_iter()
            .chain(b.required_capabilities()?.0)
            .map(|c| c.0),
    ) {
        if !enabled.contains(&cap) {
            return Err(ErrorCode::MissingCapability.into());
        }
    }
    if pairing && !enabled.contains("oink.pairing/1") {
        return Err(ErrorCode::MissingCapability.into());
    }
    for crypto in [a.crypto()?, b.crypto()?] {
        if !crypto.identity_algorithms()?.0.iter().any(|s| s.0 == ED25519)
            || !crypto.admission_suites()?.0.iter().any(|s| s.0 == ADMISSION_SUITE)
            || (pairing && !crypto.pairing_suites()?.0.iter().any(|s| s.0 == PAIRING_SUITE))
        {
            return Err(ErrorCode::UnsupportedValue.into());
        }
    }
    let mut raw = [
        (1, RawValue::Uint(1)),
        (
            2,
            RawValue::Uint(u64::from(av.highest_minor()?.min(bv.highest_minor()?))),
        ),
        (
            3,
            List::<_, 128>(enabled.into_iter().map(Ascii::<96>).collect()).write(),
        ),
        (4, a.limits()?.intersect(&b.limits()?)?.write()),
        (6, RawValue::from(ADMISSION_SUITE)),
        (7, offer_hash(initiator, responder)?.write()),
    ]
    .into_iter()
    .collect::<super::RawMap>();
    if pairing {
        raw.insert(5, RawValue::from(PAIRING_SUITE));
    }
    Negotiated::from_raw(raw)
}

/// Local diagnostic context, never another cross-device Message. Collection
/// replaces identities with collection-local aliases before export.
#[derive(Clone, Debug, PartialEq)]
pub struct ProtocolViolation {
    pub code: ErrorCode,
    pub scope: ErrorScope,
    pub peer: DeviceId,
    pub boot: Boot,
    pub path: Option<PathId>,
    pub kind: Option<u16>,
    pub type_id: Option<u16>,
    pub capability: Option<String>,
    pub experimental: bool,
    pub isolation: Isolation,
    pub at_us: u64,
    pub suppressed: u64,
}

/// Execution metadata can appear in granted pages/roots and retained attachment heads.
fn execution_nodes(message: &super::Message) -> Result<Vec<FileNode>> {
    use super::{Message, schema::Open};
    Ok(match message {
        Message::ContentDirectory(value) => value.page()?.entries()?.0,
        Message::ContentFileSelected(value) => vec![value.entry()?],
        Message::ContentFileVersion(value) => vec![value.entry()?],
        Message::CloudLinkOffer(value) => value.roots()?.0,
        Message::ClipboardOffer(value) => value.inventory()?.roots()?.0,
        Message::DndNotice(value) => value.inventory()?.roots()?.0,
        Message::AttachmentItems(value) => value.items()?.roots()?.0,
        Message::AttachmentHead(value) => {
            let header = value.header()?;
            match header.variant()? {
                Open::Known(AttachmentKind::Offer) => {
                    OfferAttachment::from_raw(header.body()?)?
                        .offer()?
                        .inventory()?
                        .roots()?
                        .0
                }
                Open::Known(AttachmentKind::Drag) => {
                    DragAttachment::from_raw(header.body()?)?
                        .notice()?
                        .inventory()?
                        .roots()?
                        .0
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    })
}
