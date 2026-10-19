//! No SDK integrations, arbitrary events, identities, strings, or attachments.
//! Only the per-user agent may instantiate the store and HTTPS transport.
pub mod capture;
pub mod model;
pub mod realtime;
pub mod sampling;
pub mod store;
pub mod transport;
pub mod wire;
pub use model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Io,
    Busy,
    Disabled,
    Stale,
    Limit,
    Entropy,
}
pub type Result<T> = std::result::Result<T, Error>;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
