//! Local stream metadata with explicit conversion to the v1 wire header.
use serde::{Deserialize, Serialize};

/// Largest encoded message on a reliable stream other than configuration control.
pub const MAX_RELIABLE_MESSAGE: usize = crate::v1::limits::RELIABLE_FRAME;
/// Largest encoded configuration-control message.
/// A larger one is an error at the sender, logged; never truncated or split.
pub const MAX_GROUP_STATE_MESSAGE: usize = crate::v1::limits::CONFIG_FRAME;
/// Largest datagram including its flow number: fits every hop at the minimum path MTU of
/// 1200 bytes. Posting a larger message is refused and counted.
pub const MAX_DATAGRAM: usize = crate::v1::limits::DATAGRAM;
/// Largest encoded message on a low-priority operation.
pub const MAX_LOW_MESSAGE: usize = crate::v1::limits::LOW_FRAME;

/// Concurrent unidirectional streams one side may have open on a QUIC connection.
pub const MAX_UNI_STREAMS: u32 = 256;
/// Bidirectional QUIC streams are not used.
pub const MAX_BIDI_STREAMS: u32 = 0;
/// Stream receive window, one value for every stream of a connection.
pub const STREAM_RECEIVE_WINDOW: u32 = 64 * 1024;

/// Which module a stream belongs to. Values are the wire numbering and are append-only.
///
/// A datagram flow (I1, M1, D1) has no kind of its own:
/// it follows the reliable stream of its module and is declared in that stream's header.
/// `Wallpaper` and `Dnd` only ever appear on low-priority operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum StreamKind {
    /// N0: private to the network layer (heartbeat, selection, relay, cut-over). Never handed up.
    Internal = 0,
    /// N1: control of the low-priority pool. Private to the network layer.
    LowControl = 1,
    /// C0: configuration control stream, opened by the network layer for the config module.
    Config = 2,
    /// I0 (and I1 following it): input, focus, drag-and-drop control.
    Input = 3,
    /// B0: clipboard offers. Low-priority: clipboard and file content.
    Clipboard = 4,
    /// M0 (and M1 following it): audio control and audio packets.
    Audio = 5,
    // Kind 6 remains reserved.
    /// A0: mini-game.
    Aim = 7,
    /// D0 (and D1 following it): diagnostics. Low-priority: bundle collection.
    Diagnostics = 8,
    /// Low-priority only: wallpaper sets.
    Wallpaper = 9,
    /// Low-priority only: drag previews and drag content.
    Dnd = 10,
    /// Low-priority cache usage and remote clear. Available during pause.
    Cache = 11,
    /// P0: ephemeral layout previews and non-activating physical edit handles.
    Projection = 12,
    /// Capability-gated companion layout-profile configuration snapshots.
    LayoutProfiles = 13,
    LayoutProfilesV2 = 14,
    LayoutProfilesV3 = 15,
}

impl StreamKind {
    pub fn from_wire(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Internal,
            1 => Self::LowControl,
            2 => Self::Config,
            3 => Self::Input,
            4 => Self::Clipboard,
            5 => Self::Audio,
            7 => Self::Aim,
            8 => Self::Diagnostics,
            9 => Self::Wallpaper,
            10 => Self::Dnd,
            11 => Self::Cache,
            12 => Self::Projection,
            13 => Self::LayoutProfiles,
            14 => Self::LayoutProfilesV2,
            15 => Self::LayoutProfilesV3,
            _ => return None,
        })
    }
    /// Kinds an upper layer (config module excluded) may open or register for.
    pub fn is_business(self) -> bool {
        !matches!(self, Self::Internal | Self::LowControl | Self::Config)
    }
    /// Priority of the reliable stream of this kind.
    pub fn priority(self) -> Priority {
        match self {
            Self::Internal | Self::Input => Priority::Interactive,
            _ => Priority::Normal,
        }
    }
    /// Priority of the datagram flow following this kind, if the kind has one.
    pub fn datagram_priority(self) -> Option<Priority> {
        match self {
            Self::Input | Self::Audio => Some(Priority::Interactive),
            Self::Diagnostics => Some(Priority::Normal),
            _ => None,
        }
    }
    /// Size limit of one encoded message on the reliable stream of this kind.
    pub fn max_message(self) -> usize {
        match self {
            Self::Config => MAX_GROUP_STATE_MESSAGE,
            _ => MAX_RELIABLE_MESSAGE,
        }
    }
}

/// Service class written in the stream header. The network thread and a relay serve
/// interactive streams first and give their QUIC stream a higher QUIC priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Priority {
    Interactive = 0,
    Normal = 1,
}

/// The label of a stream. Together with the direction it is the stream's identity on a
/// link; there is no other stream number. The network layer carries it and does not
/// interpret it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamTag {
    pub kind: StreamKind,
    /// The n-th stream of this kind in this direction on this link. Starts at 1, grows by
    /// one on every reopen, restarts with a new link.
    pub ordinal: u32,
}

/// Written once at the start of every reliable stream by the opener's network layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamHeader {
    /// Selects the relayed link on a relay's connection; assigned by the relay. A direct
    /// link uses the tag agreed when its path was installed.
    pub route_tag: u64,
    pub priority: Priority,
    pub tag: StreamTag,
    /// Flow number of the datagram flow that follows this stream on this hop, 0 for none.
    /// Allocated by the sender of the hop; a relay replaces it when forwarding the header.
    pub datagram_flow: u32,
}

pub const MAX_STREAM_HEADER: usize = crate::v1::limits::MAX_HEADER;

impl StreamHeader {
    pub fn to_wire(self) -> crate::v1::frame::StreamHeader {
        crate::v1::frame::StreamHeader {
            kind: self.tag.kind as u16,
            priority: self.priority as u8,
            route_tag: self.route_tag,
            ordinal: self.tag.ordinal,
            datagram_flow: self.datagram_flow,
        }
    }
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_wire().encode().expect("valid local stream header"));
    }
    pub fn decode(bytes: &[u8]) -> Result<Option<(Self, usize)>, FrameError> {
        let Some((header, size)) = crate::v1::frame::StreamHeader::parse(bytes).map_err(|_| FrameError::Header)? else {
            return Ok(None);
        };
        let kind = u8::try_from(header.kind)
            .ok()
            .and_then(StreamKind::from_wire)
            .ok_or(FrameError::Header)?;
        let priority = match header.priority {
            0 => Priority::Interactive,
            1 => Priority::Normal,
            _ => return Err(FrameError::Header),
        };
        Ok(Some((
            Self {
                route_tag: header.route_tag,
                priority,
                tag: StreamTag {
                    kind,
                    ordinal: header.ordinal,
                },
                datagram_flow: header.datagram_flow,
            },
            size,
        )))
    }
}

/// Close and reject codes, one set for both. They travel with the reader's stop
/// request or the writer's reset. There is no "paused" code: a paused device neither
/// rejects nor closes streams opened towards it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamCode {
    None,
    Unsupported,
    NotReady,
    StaleOrdinal,
    Invalid,
    Resend,
    UnknownMessage,
    UnnegotiatedCapability,
    UnsupportedMajor,
    MissingCapability,
    UnsupportedValue,
    AuthenticationFailed,
    LimitExceeded,
    InvalidChannel,
    NegotiationMismatch,
    UnsupportedHeaderExtension,
    Cancelled,
    StateConflict,
    InvalidHeader,
    Unknown(u64),
}
impl StreamCode {
    pub fn from_wire(code: u64) -> Self {
        match code {
            0 => Self::None,
            1 => Self::Unsupported,
            2 => Self::NotReady,
            3 => Self::StaleOrdinal,
            4 => Self::Invalid,
            5 => Self::Resend,
            6 => Self::UnknownMessage,
            7 => Self::UnnegotiatedCapability,
            8 => Self::UnsupportedMajor,
            9 => Self::MissingCapability,
            10 => Self::UnsupportedValue,
            11 => Self::AuthenticationFailed,
            12 => Self::LimitExceeded,
            13 => Self::InvalidChannel,
            14 => Self::NegotiationMismatch,
            15 => Self::UnsupportedHeaderExtension,
            16 => Self::Cancelled,
            17 => Self::StateConflict,
            18 => Self::InvalidHeader,
            other => Self::Unknown(other),
        }
    }
    pub fn to_wire(self) -> u64 {
        match self {
            Self::None => 0,
            Self::Unsupported => 1,
            Self::NotReady => 2,
            Self::StaleOrdinal => 3,
            Self::Invalid => 4,
            Self::Resend => 5,
            Self::UnknownMessage => 6,
            Self::UnnegotiatedCapability => 7,
            Self::UnsupportedMajor => 8,
            Self::MissingCapability => 9,
            Self::UnsupportedValue => 10,
            Self::AuthenticationFailed => 11,
            Self::LimitExceeded => 12,
            Self::InvalidChannel => 13,
            Self::NegotiationMismatch => 14,
            Self::UnsupportedHeaderExtension => 15,
            Self::Cancelled => 16,
            Self::StateConflict => 17,
            Self::InvalidHeader => 18,
            Self::Unknown(other) => other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("invalid stream header")]
    Header,
    #[error("record of {0} bytes exceeds the limit of its stream")]
    TooLarge(usize),
    #[error("invalid length prefix")]
    Length,
}
