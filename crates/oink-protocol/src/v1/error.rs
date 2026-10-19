//! Numeric failures only; wire failure never implies an updater/UI action.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Normal,
    UnsupportedKind,
    NotReady,
    StaleOrdinal,
    MalformedFrame,
    ResendOpening,
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
    /// QUIC codes wider than u16 must never be truncated into a known success.
    Unknown(u64),
}
impl ErrorCode {
    pub const fn from_number(n: u64) -> Self {
        match n {
            0 => Self::Normal,
            1 => Self::UnsupportedKind,
            2 => Self::NotReady,
            3 => Self::StaleOrdinal,
            4 => Self::MalformedFrame,
            5 => Self::ResendOpening,
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
    pub const fn number(self) -> u64 {
        match self {
            Self::Normal => 0,
            Self::UnsupportedKind => 1,
            Self::NotReady => 2,
            Self::StaleOrdinal => 3,
            Self::MalformedFrame => 4,
            Self::ResendOpening => 5,
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
            Self::Unknown(n) => n,
        }
    }
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Normal)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: ErrorCode,
    pub field_path: Vec<u32>,
    pub offset: Option<usize>,
}
impl Error {
    pub fn at_field(mut self, key: u32) -> Self {
        self.field_path.insert(0, key);
        self
    }
    pub fn at_offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }
}
impl From<ErrorCode> for Error {
    fn from(code: ErrorCode) -> Self {
        Self {
            code,
            field_path: Vec::new(),
            offset: None,
        }
    }
}
impl From<minicbor::decode::Error> for Error {
    fn from(error: minicbor::decode::Error) -> Self {
        Self {
            code: ErrorCode::MalformedFrame,
            field_path: Vec::new(),
            offset: error.position(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{:?}:{:?}", self.code.number(), self.field_path, self.offset)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
