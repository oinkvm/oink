//! Ordinary stream owners decode against the current authenticated logical link.
use oink_network::link::{LinkId, LinkLayer, LinkPath, LinkState};
use oink_protocol::{
    frame::{StreamCode, StreamKind},
    v1::{
        self, Message,
        admission::{Context, Phase},
        registry::Transport,
    },
};

pub(super) fn decode<T>(
    links: &LinkLayer,
    link: LinkId,
    kind: StreamKind,
    bytes: &[u8],
    convert: impl FnOnce(Message) -> v1::Result<T>,
) -> Result<T, StreamCode> {
    let table = links.links();
    let entry = table
        .links
        .iter()
        .find(|entry| entry.link == link && entry.state == LinkState::Ready)
        .ok_or(StreamCode::NotReady)?;
    let context = Context {
        kind: kind as u16,
        transport: Transport::Reliable,
        phase: Phase::Member,
        agreement: &entry.agreement,
        direct: entry.path == LinkPath::Direct,
        old_member_lane: false,
    };
    let frame = v1::frame::OpaqueFrame::parse_exact(bytes, v1::limits::RELIABLE_FRAME)
        .map_err(|error| StreamCode::from_wire(error.code.number()))?;
    let type_id = frame.header.type_id;
    context
        .decode(frame)
        .and_then(|message| convert(message).map_err(|error| context.payload_violation(type_id, error.code)))
        .map_err(|violation| {
            let code = StreamCode::from_wire(violation.code.number());
            links.report_protocol_violation(link, violation);
            code
        })
}
pub(crate) fn report_low_payload(links: &LinkLayer, link: LinkId, kind: StreamKind, type_id: u16, code: v1::ErrorCode) {
    let table = links.links();
    if let Some(entry) = table.links.iter().find(|entry| entry.link == link) {
        let context = Context {
            kind: kind as u16,
            transport: Transport::Low,
            phase: Phase::Member,
            agreement: &entry.agreement,
            direct: entry.path == LinkPath::Direct,
            old_member_lane: false,
        };
        links.report_protocol_violation(link, context.payload_violation(type_id, code));
    }
}
