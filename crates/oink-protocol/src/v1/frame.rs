//! Universal framing; opaque forwarding never invokes a product/debug decoder.
use super::registry;
use super::{Error, ErrorCode, Message, Result, cbor, limits};

pub const FRAME_HEADER: usize = 6;
pub const STREAM_HEADER: usize = 32;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameHeader {
    pub type_id: u16,
    pub payload_length: u32,
}
impl FrameHeader {
    pub fn parse(bytes: &[u8], maximum: usize) -> Result<Option<Self>> {
        if bytes.len() < FRAME_HEADER {
            return Ok(None);
        }
        let header = Self {
            type_id: u16::from_le_bytes(bytes[..2].try_into().unwrap()),
            payload_length: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
        };
        if matches!(header.type_id, 0 | 0xffff) || header.payload_length == 0 {
            return Err(ErrorCode::MalformedFrame.into());
        }
        if u64::from(header.payload_length) + 6 > maximum as u64 {
            return Err(ErrorCode::LimitExceeded.into());
        }
        Ok(Some(header))
    }
    pub fn total_length(self) -> usize {
        self.payload_length as usize + FRAME_HEADER
    }
    pub fn bytes(self) -> [u8; 6] {
        let mut out = [0; 6];
        out[..2].copy_from_slice(&self.type_id.to_le_bytes());
        out[2..].copy_from_slice(&self.payload_length.to_le_bytes());
        out
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpaqueFrame<'a> {
    pub header: FrameHeader,
    bytes: &'a [u8],
}
impl<'a> OpaqueFrame<'a> {
    pub fn parse_prefix(bytes: &'a [u8], maximum: usize) -> Result<Option<Self>> {
        let Some(header) = FrameHeader::parse(bytes, maximum)? else {
            return Ok(None);
        };
        if bytes.len() < header.total_length() {
            return Ok(None);
        }
        Ok(Some(Self {
            header,
            bytes: &bytes[..header.total_length()],
        }))
    }
    pub fn parse_exact(bytes: &'a [u8], maximum: usize) -> Result<Self> {
        let frame = Self::parse_prefix(bytes, maximum)?.ok_or(ErrorCode::MalformedFrame)?;
        if frame.bytes.len() != bytes.len() {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Ok(frame)
    }
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    pub fn payload(self) -> &'a [u8] {
        &self.bytes[6..]
    }
    /// Call endpoint admission on the header before this method. Unknown types
    /// fail without parsing their payload, including all experimental opcodes.
    pub fn decode(self) -> Result<Message> {
        registry::message(self.header.type_id).ok_or(ErrorCode::UnknownMessage)?;
        Message::preflight(self.header.type_id, self.payload())?;
        Message::from_payload(
            self.header.type_id,
            cbor::decode_map(self.payload(), self.payload().len())?,
        )
    }
}
pub fn encode(message: &Message, maximum: usize) -> Result<Vec<u8>> {
    let payload = cbor::encode_map(message.raw(), maximum.saturating_sub(6))?;
    let length = u32::try_from(payload.len()).map_err(|_| ErrorCode::LimitExceeded)?;
    let mut bytes = Vec::with_capacity(6 + payload.len());
    bytes.extend_from_slice(
        &FrameHeader {
            type_id: message.type_id(),
            payload_length: length,
        }
        .bytes(),
    );
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

/// One bounded incremental record. The header validator runs before body
/// allocation; an error poisons this stream instead of skipping to a later frame.
#[derive(Debug)]
pub struct FrameDecoder {
    maximum: usize,
    buffer: Vec<u8>,
    header: Option<FrameHeader>,
    failure: Option<Error>,
}
impl FrameDecoder {
    pub fn new(maximum: usize) -> Self {
        Self {
            maximum,
            buffer: Vec::with_capacity(6),
            header: None,
            failure: None,
        }
    }
    pub fn next(
        &mut self,
        input: &mut &[u8],
        mut admit: impl FnMut(FrameHeader) -> Result<()>,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        match self.read(input, &mut admit) {
            Ok(frame) => Ok(frame),
            Err(error) => {
                self.failure = Some(error.clone());
                Err(error)
            }
        }
    }
    fn read(
        &mut self,
        input: &mut &[u8],
        admit: &mut impl FnMut(FrameHeader) -> Result<()>,
    ) -> Result<Option<Vec<u8>>> {
        if self.header.is_none() {
            let n = (6 - self.buffer.len()).min(input.len());
            self.buffer.extend_from_slice(&input[..n]);
            *input = &input[n..];
            let Some(header) = FrameHeader::parse(&self.buffer, self.maximum)? else {
                return Ok(None);
            };
            admit(header)?;
            self.buffer.reserve_exact(header.total_length() - 6);
            self.header = Some(header);
        }
        let header = self.header.unwrap();
        let n = (header.total_length() - self.buffer.len()).min(input.len());
        self.buffer.extend_from_slice(&input[..n]);
        *input = &input[n..];
        if self.buffer.len() != header.total_length() {
            return Ok(None);
        }
        self.header = None;
        Ok(Some(std::mem::replace(&mut self.buffer, Vec::with_capacity(6))))
    }
    pub fn finish(&self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if !self.buffer.is_empty() {
            return Err(ErrorCode::MalformedFrame.into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamHeader {
    pub kind: u16,
    pub priority: u8,
    pub route_tag: u64,
    pub ordinal: u32,
    pub datagram_flow: u32,
}
impl StreamHeader {
    pub fn bootstrap() -> Self {
        Self {
            kind: 0,
            priority: 0,
            route_tag: 0,
            ordinal: 1,
            datagram_flow: 0,
        }
    }
    pub fn encode(&self) -> Result<[u8; 32]> {
        if self.ordinal == 0 || self.kind == 0xffff {
            return Err(ErrorCode::InvalidHeader.into());
        }
        if self.priority > 1 {
            return Err(ErrorCode::UnsupportedValue.into());
        }
        let mut out = [0; 32];
        out[..4].copy_from_slice(b"OINK");
        out[4..6].copy_from_slice(&32u16.to_le_bytes());
        out[6..8].copy_from_slice(&self.kind.to_le_bytes());
        out[10] = self.priority;
        out[12..20].copy_from_slice(&self.route_tag.to_le_bytes());
        out[20..24].copy_from_slice(&self.ordinal.to_le_bytes());
        out[24..28].copy_from_slice(&self.datagram_flow.to_le_bytes());
        Ok(out)
    }
    pub fn parse(bytes: &[u8]) -> Result<Option<(Self, usize)>> {
        if bytes.len() < 6 {
            return Ok(None);
        }
        if &bytes[..4] != b"OINK" {
            return Err(ErrorCode::InvalidHeader.into());
        }
        let len = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as usize;
        if !(32..=limits::MAX_HEADER).contains(&len) {
            return Err(ErrorCode::InvalidHeader.into());
        }
        if bytes.len() < len {
            return Ok(None);
        }
        if bytes[8..10] != [0, 0] || bytes[11] != 0 || bytes[28..32] != [0; 4] {
            return Err(ErrorCode::InvalidHeader.into());
        }
        let header = Self {
            kind: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            priority: bytes[10],
            route_tag: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
            ordinal: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            datagram_flow: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        };
        header.encode()?;
        if len > 32 {
            if len < 36 {
                return Err(ErrorCode::InvalidHeader.into());
            }
            let id = u16::from_le_bytes(bytes[32..34].try_into().unwrap());
            let size = u16::from_le_bytes(bytes[34..36].try_into().unwrap()) as usize;
            if id == 0 || 36 + size > len {
                return Err(ErrorCode::InvalidHeader.into());
            }
            // No initial TLV allocations. Future known TLVs need a hop capability.
            return Err(ErrorCode::UnsupportedHeaderExtension.into());
        }
        Ok(Some((header, len)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Datagram<'a> {
    Unregistered { flow: u32 },
    Active { flow: u32, frame: OpaqueFrame<'a> },
}
pub fn datagram<'a>(bytes: &'a [u8], registered: impl FnOnce(u32) -> bool) -> Result<Datagram<'a>> {
    if bytes.len() > limits::DATAGRAM {
        return Err(ErrorCode::LimitExceeded.into());
    }
    if bytes.len() < 4 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let flow = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    if flow == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    if !registered(flow) {
        return Ok(Datagram::Unregistered { flow });
    }
    Ok(Datagram::Active {
        flow,
        frame: OpaqueFrame::parse_exact(&bytes[4..], limits::DATAGRAM - 4)?,
    })
}
pub fn encode_datagram(flow: u32, message: &Message) -> Result<Vec<u8>> {
    if flow == 0 {
        return Err(ErrorCode::MalformedFrame.into());
    }
    let mut bytes = flow.to_le_bytes().to_vec();
    bytes.extend(encode(message, limits::DATAGRAM - 4)?);
    Ok(bytes)
}
pub fn discovery(bytes: &[u8]) -> Result<Message> {
    if bytes.len() > limits::DISCOVERY {
        return Err(ErrorCode::LimitExceeded.into());
    }
    if bytes.len() < 6 || &bytes[..6] != b"OINK\x01\x00" {
        return Err(ErrorCode::InvalidHeader.into());
    }
    let frame = OpaqueFrame::parse_exact(&bytes[6..], limits::DISCOVERY - 6)?;
    if !(0x901..=0x903).contains(&frame.header.type_id) {
        return Err(ErrorCode::InvalidChannel.into());
    }
    frame.decode()
}
pub fn encode_discovery(message: &Message) -> Result<Vec<u8>> {
    if !(0x901..=0x903).contains(&message.type_id()) {
        return Err(ErrorCode::InvalidChannel.into());
    }
    let mut bytes = b"OINK\x01\x00".to_vec();
    bytes.extend(encode(message, limits::DISCOVERY - 6)?);
    Ok(bytes)
}
