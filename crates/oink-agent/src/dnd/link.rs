//! I0 orders drag metadata; this ordinary actor owns descriptor and content Low work.
use super::*;
use crate::{
    attachment::link::Assemblies,
    config_module::ConfigChange,
    streams::{StreamModule, dnd::DndStreams},
    threads::input::{InputCommand, InputThread},
};
use oink_network::link::{ModuleEvent, ModulePort, StreamDir, StreamId};
use oink_protocol::{
    frame::StreamKind,
    v1::{
        self, Message, dto,
        schema::{Open, WireDto},
    },
};
use std::time::Instant;
#[derive(Clone)]
pub(crate) struct Handle {
    send: tokio::sync::mpsc::UnboundedSender<Command>,
    pub(super) glue: Arc<Mutex<Option<Arc<DndGlue>>>>,
    pub(super) files: Arc<crate::streams::clipboard::files::FileLinks>,
}
enum Command {
    Preview {
        peer: DeviceId,
        stream: StreamId,
        message: oink_routing::dnd::DndMsg,
    },
    Low {
        peer: DeviceId,
        stream: StreamId,
        message: Message,
    },
    Attachment {
        peer: DeviceId,
        stream: StreamId,
        message: Message,
    },
}
impl Handle {
    pub(crate) fn preview(&self, peer: DeviceId, stream: StreamId, message: oink_routing::dnd::DndMsg) {
        let _ = self.send.send(Command::Preview { peer, stream, message });
    }
    pub(crate) fn low(&self, peer: DeviceId, stream: StreamId, message: Message) {
        let _ = self.send.send(Command::Low { peer, stream, message });
    }
    pub(crate) fn attachment(&self, peer: DeviceId, stream: StreamId, message: Message) {
        let _ = self.send.send(Command::Attachment { peer, stream, message });
    }
    pub(crate) fn stop_native(&self) {
        if let Some(glue) = self.glue.lock().unwrap().take() {
            glue.cancel_targets();
            let source = *glue.active_source.lock().unwrap();
            if let Some((id, operation)) = source {
                let physical = glue.source_physical.load(Ordering::Acquire);
                glue.backend
                    .finish_source(operation, false, &|| glue.source_release(id, physical));
                let _ = glue.set_source_anchor(None);
            }
            glue.backend.cancel();
            glue.backend.unprobe();
            glue.files.set_paused(true);
        }
        self.files.cancel_all();
    }
    pub(crate) fn execute_reported(&self, command: DndCmd, report: Option<crate::telemetry::Stamp>) -> Vec<DndEvent> {
        let glue = self.glue.lock().unwrap().clone();
        glue.map_or_else(Vec::new, |glue| glue.execute_reported(command, report))
    }
}
#[derive(Clone)]
struct Pending {
    stream: StreamId,
    key: oink_routing::dnd::DragKey,
    revision: u64,
}
pub(crate) fn start_link_runtime(
    node: Node,
    mut port: ModulePort,
) -> anyhow::Result<(Handle, tokio::task::JoinHandle<()>)> {
    let links = node.links()?;
    let group = node.link_config()?;
    let input = node.input_thread()?;
    let files = crate::streams::clipboard::files::FileLinks::new(links.clone(), StreamKind::Dnd);
    files.set_log_config(group.clone());
    files.pause_changed(group.applied().paused);
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let handle = Handle {
        send,
        glue: Default::default(),
        files: files.clone(),
    };
    let shared = handle.clone();
    let task = tokio::spawn(async move {
        let mut streams = DndStreams::new(links.clone(), group.clone());
        let mut assemblies = Assemblies::default();
        let mut outgoing = HashMap::<(DeviceId, u64), Pending>::new();
        let mut pending = HashMap::<(DeviceId, u64), Pending>::new();
        let mut current = HashMap::<DeviceId, StreamId>::new();
        let mut jobs = tokio::task::JoinSet::new();
        let mut changes = group.subscribe();
        let mut stop = node.stop_receiver();
        loop {
            tokio::select! {
                _=stop.changed()=>break,
                _=input.status_changed()=>{},
                _=async{if let Some(at)=assemblies.deadline(){tokio::time::sleep_until(at.into()).await}else{std::future::pending::<()>().await}}=>{},
                _=jobs.join_next(),if !jobs.is_empty()=>{},
                event=port.next()=>match event{
                    Some(ModuleEvent::LinkReady{link,peer})=>{streams.link_ready(link,peer);if let Some(entry)=links.links().by_peer(peer).filter(|e|e.link==link){files.link_ready(peer,link,entry.peer_boot);}},
                    Some(ModuleEvent::LinkLost{link,peer,reason})=>{streams.link_lost(link,peer,reason);files.link_lost(peer,link,reason);},
                    Some(ModuleEvent::Inbound(stream))=>streams.inbound(stream),
                    Some(ModuleEvent::InboundLow(operation))=>streams.inbound_low(*operation),
                    Some(ModuleEvent::StreamClosed{stream,reason})=>streams.stream_closed(stream,reason),
                    Some(ModuleEvent::ProtocolViolation{..})=>{},
                    Some(ModuleEvent::PathChanged{..})=>{},None=>break,
                },
                change=changes.next()=>match change{
                    Some(ConfigChange::State{..})=>{
                        let paused=group.applied().paused;streams.pause_changed(paused);files.pause_changed(paused);
                        if paused{pending.clear();outgoing.clear();files.cancel_all();}
                        if let Some(glue)=shared.glue.lock().unwrap().as_ref(){glue.files.set_paused(paused);if paused{glue.cancel_targets();}}
                    },Some(_)=>{},None=>break,
                },
                command=receive.recv()=>match command{
                    Some(Command::Preview{peer,stream,message})=>{
                        if let Ok(message)=wire::encode(&message){ let _=shared.send.send(Command::Low{peer,stream,message}); }
                    },
                    Some(Command::Low{peer,stream,message})=>if current_out(&input,peer)==Some(stream){streams.input_streams(peer,current_in(&input,peer).zip(Some(stream)));streams.send(peer,message);},
                    Some(Command::Attachment{peer,stream,message})=>{
                        if stream.dir==StreamDir::Out&&current_out(&input,peer)==Some(stream){
                            if let Message::AttachmentHead(head)=&message
                                && let Ok(notice)=head_notice(head){
                                outgoing.retain(|(p,_),_|*p!=peer);if let Ok(id)=head.id(){outgoing.insert((peer,id),Pending{stream,key:notice.key,revision:notice.revision});}
                            }
                        }else if current_in(&input,peer)==Some(stream){
                            if let Message::AttachmentResult(result)=message{
                                if result.kind()==Ok(dto::AttachmentKind::Drag)&&let Ok(id)=result.id(){streams.refused(peer,id);if let Some(old)=outgoing.remove(&(peer,id)){input.send(InputCommand::DndRefused{peer,stream:old.stream,key:old.key,revision:old.revision});}}
                            }else{receive_metadata(&input,&files,&mut assemblies,&mut pending,peer,stream,message);}
                        }
                    },None=>break,
                },
            }
            let status = input.status();
            for peer in &status.peers {
                streams.input_streams(
                    peer.peer,
                    peer.attached.then(|| peer.inbound.zip(peer.outbound)).flatten(),
                );
                if peer.attached
                    && let Some(stream) = peer.inbound
                {
                    let previous = current.insert(peer.peer, stream);
                    if previous != Some(stream) {
                        if let Some(old) = previous {
                            assemblies.clear(old);
                        }
                        files.inbound(peer.peer, stream);
                    }
                }
            }
            current.retain(|peer, stream| {
                let live = current_in(&input, *peer) == Some(*stream);
                if !live {
                    assemblies.clear(*stream);
                    files.stream_closed(*peer, *stream);
                    streams.input_attached(*peer, false);
                }
                live
            });
            pending.retain(|(peer, _), p| current.get(peer) == Some(&p.stream));
            outgoing.retain(|(peer, _), p| current_out(&input, *peer) == Some(p.stream));
            for (stream, reply) in assemblies.expire(Instant::now()) {
                if let Some(peer) = current.iter().find_map(|(peer, s)| (*s == stream).then_some(*peer)) {
                    refused(&input, &mut pending, peer, &reply);
                    send_control(&input, peer, reply);
                }
            }
            for mut operation in streams.take_low() {
                let peer = operation.stream.peer();
                // File content is authorized by the source's retained offer and
                // holder generation in serve_link. It can outlive an I0 pair or
                // flow directly between source and target in a three-role drag.
                // Only metadata/previews below inherit the current I0 identity.
                if oink_clipboard::wire::local_content(operation.first.clone()).is_ok() {
                    if let Some(clip) = node.inner.clip.get().cloned() {
                        jobs.spawn(async move {
                            clip.serve_link(operation).await;
                        });
                    } else {
                        operation.stream.cancel();
                    }
                    continue;
                }
                let Some(scope) = current_in(&input, peer) else {
                    operation.stream.cancel();
                    continue;
                };
                match operation.first.clone() {
                    message @ Message::AttachmentItems(_) => {
                        receive_metadata(&input, &files, &mut assemblies, &mut pending, peer, scope, message)
                    }
                    Message::DndPreview(preview) => {
                        if let Ok(message) = wire::local_preview(preview) {
                            input.send(InputCommand::PeerMessage {
                                peer,
                                stream: scope,
                                message: oink_routing::Packet::Dnd(message),
                            });
                        }
                    }
                    _ => {
                        operation.stream.cancel();
                        continue;
                    }
                }
                operation.stream.finish();
                jobs.spawn(async move {
                    let _ = operation.stream.recv().await;
                });
            }
        }
        files.cancel_all();
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        input.send(InputCommand::DndAvailable(false));
    });
    Ok((handle, task))
}
fn current_in(input: &InputThread, peer: DeviceId) -> Option<StreamId> {
    input
        .status()
        .peers
        .iter()
        .find(|p| p.peer == peer && p.attached)
        .and_then(|p| p.inbound)
}
fn current_out(input: &InputThread, peer: DeviceId) -> Option<StreamId> {
    input
        .status()
        .peers
        .iter()
        .find(|p| p.peer == peer && p.attached)
        .and_then(|p| p.outbound)
}
fn send_control(input: &InputThread, peer: DeviceId, message: Message) {
    if let Some(stream) = current_out(input, peer) {
        input.send(InputCommand::SendMessage { peer, stream, message });
    }
}
fn head_notice(head: &dto::AttachmentHead) -> v1::Result<oink_routing::dnd::DragNotice> {
    let header = head.header()?;
    if header.variant() != Ok(Open::Known(dto::AttachmentKind::Drag)) {
        return Err(v1::ErrorCode::InvalidChannel.into());
    }
    wire::local_notice(&dto::DragAttachment::from_raw(header.body()?)?.notice()?)
}
fn refused(input: &InputThread, pending: &mut HashMap<(DeviceId, u64), Pending>, peer: DeviceId, message: &Message) {
    if let Message::AttachmentResult(result) = message
        && let Ok(id) = result.id()
        && let Some(old) = pending.remove(&(peer, id))
    {
        input.send(InputCommand::DndRefused {
            peer,
            stream: old.stream,
            key: old.key,
            revision: old.revision,
        });
    }
}
fn receive_metadata(
    input: &Arc<InputThread>,
    files: &Arc<crate::streams::clipboard::files::FileLinks>,
    assemblies: &mut Assemblies,
    pending: &mut HashMap<(DeviceId, u64), Pending>,
    peer: DeviceId,
    stream: StreamId,
    message: Message,
) {
    if let Message::DndNotice(value) = message {
        if let Ok(notice) = wire::local_notice(&value)
            && notice.valid(peer)
        {
            files.accept(peer, notice.key.offer.clip);
            input.send(InputCommand::DndItems {
                peer,
                stream,
                notice,
                incomplete: false,
            });
        }
        return;
    }
    if let Message::AttachmentHead(head) = &message {
        let Ok(notice) = head_notice(head) else {
            return;
        };
        if !notice.valid(peer) {
            return;
        }
        let Ok(id) = head.id() else {
            return;
        };
        pending.retain(|(p, old), _| *p != peer || *old >= id);
        pending.insert(
            (peer, id),
            Pending {
                stream,
                key: notice.key,
                revision: notice.revision,
            },
        );
        input.send(InputCommand::DndItems {
            peer,
            stream,
            notice,
            incomplete: true,
        });
    }
    if let Ok((complete, replies)) = assemblies.receive(stream, message, Instant::now()) {
        for reply in replies {
            refused(input, pending, peer, &reply);
            send_control(input, peer, reply);
        }
        if let Some(Message::DndNotice(value)) = complete
            && let Ok(notice) = wire::local_notice(&value)
            && notice.valid(peer)
        {
            pending.retain(|(p, _), _| *p != peer);
            files.accept(peer, notice.key.offer.clip);
            input.send(InputCommand::DndItems {
                peer,
                stream,
                notice,
                incomplete: false,
            });
        }
    }
}
