//! One ordinary clipboard actor; native readers own independent content lifetimes.
use super::*;
use crate::{
    config_module::{ConfigChange, ConfigModule},
    node::{Node, lock},
    streams::{StreamModule, clipboard::ClipboardStreams},
};
use oink_clipboard::{
    model::ContentMsg,
    sync::files::{BoxFuture, BulkChannel},
};
use oink_foundation::drag_trace::{Context as ReadContext, Trace as ReadTrace, stage as read_stage};
use oink_network::link::{InboundLow, LowStream, ModuleEvent, ModulePort};
use oink_protocol::{
    frame::StreamKind,
    v1::{
        Message,
        schema::{WireDto, WireValue},
    },
};
use std::io;
use tokio::sync::mpsc::UnboundedReceiver;
pub(crate) fn start_link_runtime(
    node: Node,
    port: ModulePort,
    backend: Arc<dyn ClipboardBackend>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let group = node.link_config()?;
    let streams = ClipboardStreams::new(node.links()?, group.clone());
    let links = streams.file_links();
    let (enabled, settings) = {
        let config = lock(&node.inner.config);
        (
            config.settings.clipboard_enabled,
            config.settings.clipboard.unwrap_or_else(default_settings),
        )
    };
    let paused = group.applied().paused;
    let data_dir = node.cache_dir();
    std::fs::create_dir_all(&data_dir)?;
    let clock = clock::Clock::load(node.data_dir().join("clipboard-clock.json"))?;
    let ordering = oink_clipboard::sync::Ordering::new(node.id(), clock.value());
    let (tx, rx) = unbounded_channel();
    let observed = backend.change_count();
    let shared = Arc::new(Shared {
        native: Arc::default(),
        native_target: Mutex::default(),
        links,
        settings: Mutex::new(settings),
        policy: oink_clipboard::policy::SharingPolicy::new(enabled && !paused, settings),
        enabled: AtomicBool::new(enabled),
        paused: AtomicBool::new(paused),
        activity: Mutex::default(),
        transfer: Mutex::default(),
        dnd_files: Mutex::default(),
    });
    let handle = ClipHandle {
        tx: tx.clone(),
        shared: shared.clone(),
    };
    anyhow::ensure!(node.inner.clip.set(handle.clone()).is_ok(), "clipboard already started");
    let notify = tx.clone();
    let weak = Arc::downgrade(&backend);
    backend.watch(Arc::new(move || {
        if let Some(backend) = weak.upgrade() {
            let _ = notify.send(Input::LocalChange(backend.change_count()));
        }
    }))?;
    backend.set_watch_paused(!enabled || paused)?;
    let weak = Arc::downgrade(&node.inner);
    let weak2 = weak.clone();
    let weak3 = weak.clone();
    let task = Task {
        streams,
        ordering,
        clock,
        fault: None,
        native_source: None,
        capture: None,
        capture_again: false,
        backend,
        data_dir,
        me: node.id(),
        tx,
        shared,
        observed,
        last_write: None,
        remote_scope: None,
        cancelled: None,
        prefetch: None,
        provider_retry: None,
        fetch_requested: None,
        native_write: None,
        queued_write: None,
        on_transfer: Arc::new(move |value| {
            if let Some(inner) = weak.upgrade() {
                Node { inner }.emit(crate::api::ApiEvent::FileTransfer(value));
            }
        }),
        name_of: Arc::new(move |peer| {
            weak2
                .upgrade()
                .map(|inner| Node { inner }.device_name(peer))
                .unwrap_or_default()
        }),
        on_activity: Arc::new(move |value| {
            if let Some(inner) = weak3.upgrade() {
                Node { inner }.emit(crate::api::ApiEvent::ClipActivity(value.clone()));
            }
        }),
    };
    Ok(tokio::spawn(task.run(node, group, port, rx, handle)))
}
impl Task {
    async fn run(
        mut self,
        node: Node,
        group: ConfigModule,
        mut port: ModulePort,
        mut commands: UnboundedReceiver<Input>,
        handle: ClipHandle,
    ) {
        let mut changes = group.subscribe();
        let mut stop = node.stop_receiver();
        let wake = self.streams.wake();
        let mut serving = tokio::task::JoinSet::new();
        let data = self.data_dir.clone();
        let backend = self.backend.clone();
        let _ = tokio::task::spawn_blocking(move || {
            oink_clipboard::store::recover(&data);
            backend.initialize_store(data);
        })
        .await;
        self.streams.pause_changed(!self.active());
        self.capture();
        let mut published = None;
        loop {
            let observation = (
                if self.active() && self.fault.is_none() {
                    dto::SharingMode::Shared
                } else {
                    dto::SharingMode::Local
                },
                self.fault,
            );
            if published != Some(observation) {
                // Missing health means unobserved to device cards. A running actor
                // must report explicit health even without clipboard content or peers.
                let code = self.fault.unwrap_or(dto::ResourceFaultCode::None);
                let fault = dto::ResourceFault::from_raw([(1, code.write())].into()).expect("typed clipboard fault");
                if group.publish_local_clipboard(Some(observation.0), Some(fault)).is_ok() {
                    published = Some(observation);
                }
            }
            let deadline = self
                .streams
                .deadline()
                .into_iter()
                .chain(self.active().then(|| self.ordering.deadline()).flatten())
                .min()
                .unwrap_or(Instant::now() + Duration::from_millis(500));
            tokio::select! {
                _=stop.changed()=>break,
                _=wake.notified()=>{},
                _=tokio::time::sleep_until(deadline.into())=>{},
                _=serving.join_next(),if !serving.is_empty()=>{},
                change=changes.next()=>match change {
                    Some(ConfigChange::State{..})=>handle.set_paused(group.applied().paused),
                    Some(ConfigChange::Facts{device,capabilities_changed})=>self.streams.facts_changed(device,capabilities_changed),
                    Some(_)=>{},None=>break,
                },
                event=port.next()=>match event {
                    Some(ModuleEvent::LinkReady{link,peer})=>self.streams.link_ready(link,peer),
                    Some(ModuleEvent::LinkLost{link,peer,reason})=>self.streams.link_lost(link,peer,reason),
                    Some(ModuleEvent::Inbound(stream))=>self.streams.inbound(stream),
                    Some(ModuleEvent::InboundLow(operation))=>self.streams.inbound_low(*operation),
                    Some(ModuleEvent::StreamClosed{stream,reason})=>self.streams.stream_closed(stream,reason),
                    Some(ModuleEvent::ProtocolViolation{..})=>{},
                    Some(ModuleEvent::PathChanged{..})=>{},None=>break,
                },
                command=commands.recv()=>match command {
                    Some(Input::Captured(before,result))=>self.captured(before,result),
                    Some(Input::Applied{id,ticket,publication})=>self.applied(id,ticket,publication),
                    Some(Input::LocalChange(count))=>if self.active(){self.observe(count);},
                    Some(Input::Reconfigure)=>self.reconfigure(),
                    Some(Input::FetchFiles)=>self.fetch(),
                    Some(Input::CancelFiles)=>{self.cancel_native_write();self.fetch_requested=None;self.prefetch=None;self.cancelled=self.current_id();if let Some(mut status)=handle.transfer(){status.phase=FileTransferPhase::Cancelled;self.set_transfer(Some(status));}},
                    #[cfg(target_os = "macos")]
                    Some(Input::FileProviderAvailable)=>if self.active()&&self.provider_retry.is_some()&& let Some(offer)=self.ordering.current().cloned(){self.apply(offer);},
                    Some(Input::Progress(id,batch,value))=>self.progress(id,batch,value),
                    Some(Input::Prefetched{id,ticket,before,result})=>{
                        if self.prefetch.as_ref().is_some_and(|(clip,current)|*clip==id&&Arc::ptr_eq(current,&ticket)) {
                            self.prefetch=None;
                            if self.backend.change_count()==before {
                                match result {
                                    Ok(spec) if self.may_write(id)=>self.enqueue_native_write(id,spec),
                                    Ok(spec) if self.active()&&self.cancelled!=Some(id)
                                        &&self.current_id().is_none_or(|current|current==id)
                                        &&self.shared.links.offer_current(id.origin,id)=>{
                                        if let Some((_,old))=self.queued_write.take(){old.contents.cancel();}
                                        self.queued_write=Some((id,spec));
                                    },
                                    Ok(_)=>{},
                                    Err(_) if self.may_write(id)=>self.set_transfer(self.status(id,FileTransferPhase::Failed,Some("文件准备未完成".into()))),
                                    Err(_)=>{},
                                }
                            }
                        }
                    },
                    Some(Input::Barrier(reply))=>{let _=reply.send(());},
                    Some(Input::Stop(reply))=>{let _=reply.send(());break;},None=>break,
                },
            }
            for (peer, stream) in self.streams.take_closed() {
                if self
                    .remote_scope
                    .is_some_and(|(_, from, scope)| from == peer && scope == stream)
                {
                    self.remote_scope = None;
                    self.ordering.forget(peer);
                    // A download binds the offer and source Boot, so metadata
                    // stream replacement does not revoke its explicit demand.
                    // FileLinks retires it on owner pause or peer restart.
                }
            }
            let messages = self.streams.poll(Instant::now());
            for (peer, message) in messages {
                let Message::ClipboardOffer(offer) = message else {
                    continue;
                };
                if !self.active() {
                    continue;
                }
                self.observe(self.backend.change_count());
                let Some(stream) = self.streams.inbound_id(peer) else {
                    continue;
                };
                match self.ordering.receive(peer, offer.clone()) {
                    Ok(accepted) => {
                        if !self.persist() {
                            continue;
                        }
                        if accepted && let Some(id) = self.current_id() {
                            self.remote_scope = Some((id, peer, stream));
                            self.cancelled = None;
                            self.apply(offer);
                        }
                    }
                    Err(_) => {
                        self.streams
                            .stream_closed(stream, oink_network::link::StreamClosed::Local);
                    }
                }
            }
            for operation in self.streams.take_low() {
                let handle = handle.clone();
                serving.spawn(async move {
                    handle.serve_link(operation).await;
                });
            }
            if self.active() && self.ordering.capture_due(Instant::now()) {
                self.capture();
            }
        }
        self.cancel_native_write();
        if let Some((_, remote)) = self.shared.native_target.lock().unwrap().take() {
            remote.cancel();
        }
        if let Some(capture) = self.capture.take() {
            capture.abort();
        }
        self.shared.native.cancel_all();
        self.shared.links.cancel_all();
        serving.abort_all();
        while serving.join_next().await.is_some() {}
        let _ = self.backend.set_watch_paused(true);
    }
}
impl ClipHandle {
    pub(crate) async fn serve_link(&self, mut operation: InboundLow) {
        let Some(_activity) =
            oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::FileTransfer)
        else {
            operation.stream.cancel();
            return;
        };
        let diagnostic = ReadContext {
            object: operation.stream.link().0,
            ..Default::default()
        }
        .with_request(operation.stream.diagnostic_id());
        let peer = operation.stream.peer();
        let kind = operation.stream.kind();
        let Ok(message) = oink_clipboard::wire::local_content(operation.first) else {
            operation.stream.cancel();
            return;
        };
        let (offer, representation) = match &message {
            ContentMsg::ReadRepresentation {
                offer, representation, ..
            } => (offer.clip.seq, representation.0),
            request => (request.request().map_or(0, |(offer, _)| offer.clip.seq), 0),
        };
        let served = ReadTrace::new(
            read_stage::SERVE,
            ReadContext {
                offer,
                representation,
                ..diagnostic
            },
        );
        let diagnostic = served.context();
        match message {
            ContentMsg::TargetStored { delivery, receipt } => {
                self.receipt(operation.stream, kind, peer, delivery, Some(receipt))
                    .await;
            }
            ContentMsg::Failed { delivery, .. } => {
                self.receipt(operation.stream, kind, peer, delivery, None).await;
            }
            request if request.request().is_some() => {
                let (offer, demand) = request.request().unwrap();
                let registry = match kind {
                    StreamKind::Clipboard if self.shared.policy.enabled() && demand == Demand::Clipboard => {
                        self.shared.native.clone()
                    }
                    StreamKind::Dnd => {
                        let files = self.shared.dnd_files.lock().unwrap().clone();
                        let Some(files) = files else {
                            operation.stream.cancel();
                            return;
                        };
                        if !files.authorize(peer, offer, demand, diagnostic).await {
                            operation.stream.cancel();
                            return;
                        }
                        files.native.clone()
                    }
                    _ => {
                        operation.stream.cancel();
                        return;
                    }
                };
                let Some(profile) = self.shared.links.peer_formats(peer, operation.stream.link()) else {
                    operation.stream.cancel();
                    return;
                };
                let mut channel = LinkBulk(operation.stream);
                let result = registry
                    .serve_negotiated_traced(
                        peer,
                        request,
                        &mut channel,
                        &oink_clipboard::negotiation::formats(),
                        &profile,
                        diagnostic,
                    )
                    .await;
                served.io(&result);
                channel.finish();
                let _ = channel.recv().await;
            }
            _ => operation.stream.cancel(),
        }
    }
    async fn receipt(
        &self,
        mut stream: LowStream,
        kind: StreamKind,
        peer: DeviceId,
        delivery: oink_foundation::content::DeliveryId,
        receipt: Option<[u8; 32]>,
    ) {
        let accepted = match kind {
            StreamKind::Clipboard => self.shared.native.clipboard_receipt(peer, delivery, receipt),
            StreamKind::Dnd => {
                let files = self.shared.dnd_files.lock().unwrap().clone();
                if let Some(files) = files {
                    files.receipt(peer, delivery, receipt).await
                } else {
                    false
                }
            }
            _ => false,
        };
        if !accepted {
            stream.cancel();
            return;
        }
        if let Ok(message) = oink_clipboard::wire::content(&ContentMsg::TerminalAck { delivery, receipt }) {
            let _ = stream.send(&message).await;
            stream.finish();
            let _ = stream.recv().await;
        }
    }
}
pub(crate) struct LinkBulk(pub LowStream);
impl BulkChannel for LinkBulk {
    fn diagnostic_id(&self) -> [u8; 16] {
        self.0.diagnostic_id()
    }
    fn pacing_observer(&self) -> oink_clipboard::sync::files::PacingObserver {
        crate::streams::clipboard::files::file_pacing_observer(&self.0)
    }
    fn transfer_path(&self) -> &'static str {
        match self.0.path() {
            oink_network::link::LinkPath::Direct => "direct",
            oink_network::link::LinkPath::Relayed { .. } => "relayed",
        }
    }
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        self.0.effect_validity()
    }
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let message = oink_clipboard::wire::content(message).map_err(io::Error::other)?;
            self.0.send(&message).await.map_err(io::Error::other)
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async move {
            self.0
                .recv()
                .await
                .map_err(io::Error::other)?
                .map(oink_clipboard::wire::local_content)
                .transpose()
                .map_err(io::Error::other)
        })
    }
    fn finish(&mut self) {
        self.0.finish();
    }
}
