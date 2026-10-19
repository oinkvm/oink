//! The low-priority half of drag and drop. Control and refusals remain on I0.
use super::StreamModule;
use crate::config_module::ConfigModule;
use oink_foundation::DeviceId;
use oink_network::link::{InboundLow, InboundStream, LinkId, LinkLayer, LinkLost, StreamClosed, StreamId};
use oink_protocol::{
    frame::{StreamCode, StreamKind},
    v1::{self, Message, dto},
};
use std::collections::HashMap;

pub const KINDS: &[StreamKind] = &[StreamKind::Dnd];
#[derive(Debug)]
struct Peer {
    link: LinkId,
    attached: bool,
    operations: Vec<(Option<u64>, tokio::task::JoinHandle<()>)>,
    input: Option<(StreamId, StreamId)>,
}
#[derive(Debug)]
pub struct DndStreams {
    links: LinkLayer,
    peers: HashMap<DeviceId, Peer>,
    paused: bool,
    incoming: Vec<InboundLow>,
}
impl DndStreams {
    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        Self {
            links,
            peers: HashMap::new(),
            paused: config.applied().paused,
            incoming: Vec::new(),
        }
    }
    pub fn input_attached(&mut self, peer: DeviceId, attached: bool) {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.attached = attached;
            if !attached {
                for (_, task) in state.operations.drain(..) {
                    task.abort();
                }
            }
        }
    }
    pub fn input_streams(&mut self, peer: DeviceId, pair: Option<(StreamId, StreamId)>) {
        if self.peers.get(&peer).is_some_and(|state| state.input != pair) {
            self.input_attached(peer, false);
            self.peers.get_mut(&peer).unwrap().input = pair;
        }
        self.input_attached(peer, pair.is_some());
    }
    pub fn refused(&mut self, peer: DeviceId, id: u64) {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.operations.retain(|(operation, task)| {
                if *operation == Some(id) {
                    task.abort();
                    false
                } else {
                    true
                }
            });
        }
    }
    /// Starts a one-way preview/tail. A failure belongs to this operation only.
    pub fn send(&mut self, peer: DeviceId, message: Message) -> bool {
        let Some(state) = self.peers.get_mut(&peer) else {
            return false;
        };
        if self.paused || !state.attached || !valid(&message) {
            return false;
        }
        let Ok(bytes) = v1::frame::encode(&message, v1::limits::LOW_FRAME) else {
            return false;
        };
        let send_max = oink_foundation::transfer::LowOptions::message_bytes(bytes.len());
        let options = oink_foundation::transfer::LowOptions {
            send_max,
            receive_max: 0,
            operation_max: send_max,
        };
        let links = self.links.clone();
        let link = state.link;
        state.operations.retain(|(_, task)| !task.is_finished());
        let attachment = match &message {
            Message::AttachmentItems(items) => items.id().ok(),
            _ => None,
        };
        state.operations.push((
            attachment,
            tokio::spawn(async move {
                if let Ok(mut stream) = links.open_low(link, StreamKind::Dnd, options).await
                    && stream.send(&message).await.is_ok()
                {
                    stream.finish();
                    let _ = stream.recv().await;
                }
            }),
        ));
        true
    }
    pub fn take_low(&mut self) -> Vec<InboundLow> {
        std::mem::take(&mut self.incoming)
    }
}
fn valid(message: &Message) -> bool {
    matches!(message, Message::DndPreview(_))
        || matches!(message,Message::AttachmentItems(items) if items.family()==Ok(dto::AttachmentKind::Drag))
}
impl StreamModule for DndStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        self.peers.entry(peer).or_insert_with(|| Peer {
            link,
            attached: false,
            operations: Vec::new(),
            input: None,
        });
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, _: LinkLost) {
        if self.peers.get(&peer).is_some_and(|p| p.link == link) {
            for (_, task) in self.peers.remove(&peer).unwrap().operations {
                task.abort();
            }
        }
        self.incoming.retain(|op| op.stream.link() != link);
    }
    fn inbound(&mut self, stream: InboundStream) {
        stream.reject(StreamCode::Unsupported);
    }
    fn inbound_low(&mut self, mut operation: InboundLow) {
        if operation.stream.kind() == StreamKind::Dnd
            && (valid(&operation.first) || oink_clipboard::wire::local_content(operation.first.clone()).is_ok())
            && self
                .peers
                .get(&operation.stream.peer())
                .is_some_and(|p| p.link == operation.stream.link())
        {
            self.incoming.push(operation);
        } else {
            operation.stream.cancel();
        }
    }
    fn stream_closed(&mut self, _: StreamId, _: StreamClosed) {}
    fn pause_changed(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            for peer in self.peers.values_mut() {
                for (_, task) in peer.operations.drain(..) {
                    task.abort();
                }
            }
        }
    }
    fn facts_changed(&mut self, _: DeviceId, _: bool) {}
}

impl Drop for DndStreams {
    fn drop(&mut self) {
        for peer in self.peers.values_mut() {
            for (_, task) in peer.operations.drain(..) {
                task.abort();
            }
        }
    }
}
