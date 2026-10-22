//! In-process handle pairs for other crates' tests: the real queues and handles without a
//! network thread, a link or QUIC. What one end posts the other end takes, in order, with
//! the same "full" and "closed" answers as on a link.
use super::queue::Queue;
use super::{
    stream::{DatagramRecv, DatagramSend, RecvStream, SendStream},
    types::{LinkId, QueueSpec, StreamDir, StreamId},
};
use oink_protocol::frame::StreamTag;
use std::sync::Arc;

/// One reliable stream: capacity is `queue` in total. Dropping either end closes the other.
pub fn reliable_pair(link: LinkId, tag: StreamTag, queue: QueueSpec) -> (SendStream, RecvStream) {
    let queue = Queue::new(queue);
    (
        SendStream {
            id: StreamId {
                link,
                tag,
                dir: StreamDir::Out,
            },
            queue: queue.clone(),
        },
        RecvStream {
            id: StreamId {
                link,
                tag,
                dir: StreamDir::In,
            },
            queue,
        },
    )
}

/// The datagram flow following `tag`: a full queue drops its oldest message.
pub fn datagram_pair(link: LinkId, tag: StreamTag, queue: QueueSpec) -> (DatagramSend, DatagramRecv) {
    let queue = super::queue::DatagramQueue::new(queue);
    (
        DatagramSend {
            _owner: Arc::new(super::stream::DatagramOwner(queue.clone())),
            parent: None,
            follows: StreamId {
                link,
                tag,
                dir: StreamDir::Out,
            },
            queue: queue.clone(),
        },
        DatagramRecv {
            _owner: Arc::new(super::stream::DatagramOwner(queue.clone())),
            parent: None,
            follows: StreamId {
                link,
                tag,
                dir: StreamDir::In,
            },
            queue,
        },
    )
}

/// Simulate an unreachable direct path while retaining real TLS relay paths.
pub fn set_direct_blocked(links: &super::LinkLayer, peer: oink_foundation::DeviceId, blocked: bool) {
    links.0.send(super::handle::Command::BlockDirect(peer, blocked));
}

/// Hold one peer's I0 header before assigning its logical reader identity, while
/// allowing QUIC and N0 migration to proceed. Releasing resumes normal dispatch.
pub fn hold_input_header(links: &super::LinkLayer, peer: Option<oink_foundation::DeviceId>) {
    links.0.hold_input_peer.store(peer.map(Arc::new));
    links.0.wake.notify_one();
}
pub async fn input_header_held(links: &super::LinkLayer) {
    links.0.input_header_held.notified().await;
}
pub async fn cutover_finished(links: &super::LinkLayer) {
    links.0.cutover_finished.notified().await;
}
/// Fence commands already submitted by an ordinary owner; never waits for traffic.
pub fn flush_commands(links: &super::LinkLayer) {
    links.config_phase_applied();
}

/// Counts outbound Low requests by module, including failed opens, across link
/// replacement. Test-only observation: it does not alter admission or transport.
pub fn low_open_attempts(layer: &super::LinkLayer, kind: oink_protocol::frame::StreamKind) -> u64 {
    layer
        .0
        .low_open_attempts
        .lock()
        .unwrap()
        .get(&(kind as u8))
        .copied()
        .unwrap_or(0)
}
