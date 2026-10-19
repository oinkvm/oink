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
