//! Per-link C0 admission progress. Human pairing owns a separate session table.
use oink_network::{
    admission::{GreetingSession, Peer},
    group_state::State,
    link::{Direction, SendStream},
};
use std::{collections::VecDeque, time::Instant};
pub(super) struct Link {
    pub peer_boot: oink_foundation::session::Boot,
    pub direction: Direction,
    pub path: oink_network::link::LinkPath,
    pub remote: Option<std::net::SocketAddr>,
    pub greeting: GreetingSession,
    pub peer: Option<Peer>,
    pub sent: Option<State>,
    pub accepted: Option<State>,
    pub ready: bool,
    pub admitted_anchor: Option<oink_network::roster::Anchor>,
    pub departing: bool,
    pub tx: SendStream,
    pub reader: tokio::task::JoinHandle<()>,
    pub outbox: VecDeque<Vec<u8>>,
    pub deadline: Option<Instant>,
}
