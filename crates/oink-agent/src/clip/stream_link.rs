//! File operations belong to the current inbound B0. A download retains its offer and
//! source process across link/stream interruptions and resumes at the file layer's
//! verified offset; no session identity or input-stream state participates.
use oink_clipboard::model::{ContentMsg, Inventory, ItemId, RepresentationId};
use oink_clipboard::sync::files::{BoxFuture, BulkChannel, BulkLink};
use oink_foundation::content::ClipId;
use oink_foundation::drag_trace::{Context as ReadContext, Trace as ReadTrace, stage as read_stage};
use oink_foundation::{DeviceId, session::Boot};
use oink_foundation::{clipboard::ClipPlatform, transfer::LowOptions};
use oink_network::link::{LinkId, LinkLayer, LinkLost, LowStream, StreamId};
use oink_protocol::{frame::StreamKind, v1::dto};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Debug)]
struct Peer {
    link: LinkId,
    boot: Boot,
    inbound: Option<StreamId>,
    interrupted: watch::Sender<bool>,
}
#[derive(Debug)]
struct Binding {
    peer: DeviceId,
    offer: ClipId,
    boot: Boot,
    cancelled: watch::Sender<bool>,
}
impl Binding {
    fn cancel(&self) {
        self.cancelled.send_replace(true);
    }
}
#[derive(Debug, Default)]
struct State {
    paused: bool,
    input_independent: bool,
    peers: HashMap<DeviceId, Peer>,
    offers: HashMap<(DeviceId, ClipId), Arc<Binding>>,
}
/// Ordinary file consumer's adapter to link-authorized low-priority operations.
#[derive(Debug)]
pub struct FileLinks {
    links: LinkLayer,
    kind: StreamKind,
    state: Mutex<State>,
    log_config: Mutex<Option<crate::config_module::ConfigModule>>,
}
impl FileLinks {
    pub fn new(links: LinkLayer, kind: StreamKind) -> Arc<Self> {
        Arc::new(Self {
            links,
            kind,
            // A drag's source can differ from its input owner. Content keeps
            // source-boot/offer authority; it must not require a direct I0 pair.
            // Input status and module LinkReady arrive on different queues: an
            // earlier I0 observation can otherwise be lost before this peer exists.
            state: Mutex::new(State {
                input_independent: kind == StreamKind::Dnd,
                ..Default::default()
            }),
            log_config: Mutex::default(),
        })
    }
    pub(crate) fn set_log_config(&self, config: crate::config_module::ConfigModule) {
        *self.log_config.lock().unwrap() = Some(config);
    }
    pub fn peer_formats(&self, peer: DeviceId, link: LinkId) -> Option<dto::FormatSupport> {
        self.links
            .links()
            .by_peer(peer)
            .filter(|e| e.link == link)
            .and_then(|e| e.peer_formats.clone())
    }
    pub fn link_ready(&self, peer: DeviceId, link: LinkId, boot: Boot) {
        self.state.lock().unwrap().link_ready(peer, link, boot);
    }
    pub fn inbound(&self, peer: DeviceId, stream: StreamId) {
        self.state.lock().unwrap().inbound(peer, stream);
    }
    pub fn stream_closed(&self, peer: DeviceId, stream: StreamId) {
        self.state.lock().unwrap().stream_closed(peer, stream);
    }
    pub fn link_lost(&self, peer: DeviceId, link: LinkId, reason: LinkLost) {
        self.state.lock().unwrap().link_lost(peer, link, reason);
    }
    pub fn pause_changed(&self, paused: bool) {
        self.state.lock().unwrap().pause_changed(paused);
    }
    pub fn accept(&self, peer: DeviceId, offer: ClipId) {
        self.state.lock().unwrap().accept(peer, offer);
    }
    pub fn offer_current(&self, peer: DeviceId, offer: ClipId) -> bool {
        self.state
            .lock()
            .unwrap()
            .offers
            .get(&(peer, offer))
            .is_some_and(|binding| !*binding.cancelled.borrow())
    }
    pub fn cancel_all(&self) {
        self.state.lock().unwrap().cancel_all();
    }
}
impl State {
    fn link_ready(&mut self, peer: DeviceId, link: LinkId, boot: Boot) {
        let state = self;
        for binding in state
            .offers
            .values()
            .filter(|binding| binding.peer == peer && binding.boot != boot)
        {
            binding.cancel();
        }
        if state.peers.get(&peer).is_some_and(|p| p.link == link) {
            return;
        }
        if let Some(old) = state.peers.remove(&peer) {
            old.interrupted.send_replace(true);
        }
        state.peers.insert(
            peer,
            Peer {
                link,
                boot,
                inbound: None,
                interrupted: watch::channel(state.paused || !state.input_independent).0,
            },
        );
    }
    fn inbound(&mut self, peer: DeviceId, stream: StreamId) {
        if self.input_independent {
            return;
        }
        let state = self;
        let paused = state.paused;
        if let Some(peer) = state.peers.get_mut(&peer).filter(|peer| peer.link == stream.link) {
            peer.interrupted.send_replace(true);
            peer.inbound = Some(stream);
            peer.interrupted = watch::channel(paused).0;
        }
    }
    fn stream_closed(&mut self, peer: DeviceId, stream: StreamId) {
        if self.input_independent {
            return;
        }
        if let Some(peer) = self.peers.get_mut(&peer)
            && peer.inbound == Some(stream)
        {
            peer.inbound = None;
            peer.interrupted.send_replace(true);
        }
    }
    fn link_lost(&mut self, peer: DeviceId, link: LinkId, reason: LinkLost) {
        let state = self;
        if state.peers.get(&peer).is_none_or(|peer| peer.link != link) {
            return;
        }
        let old = state.peers.remove(&peer).unwrap();
        old.interrupted.send_replace(true);
        if !download_resumable(reason) {
            for binding in state.offers.values().filter(|binding| binding.peer == peer) {
                binding.cancel();
            }
        }
    }
    fn pause_changed(&mut self, paused: bool) {
        let state = self;
        if state.paused == paused {
            return;
        }
        state.paused = paused;
        for peer in state.peers.values_mut() {
            peer.interrupted.send_replace(true);
            if !paused && (state.input_independent || peer.inbound.is_some()) {
                peer.interrupted = watch::channel(false).0;
            }
        }
    }
    fn accept(&mut self, peer: DeviceId, offer: ClipId) {
        let state = self;
        let Some(boot) = state.peers.get(&peer).map(|peer| peer.boot) else {
            return;
        };
        if state
            .offers
            .get(&(peer, offer))
            .is_some_and(|b| b.boot == boot && !*b.cancelled.borrow())
        {
            return;
        }
        state
            .offers
            .retain(|(p, _), binding| *p != peer || Arc::strong_count(binding) > 1);
        state.offers.insert(
            (peer, offer),
            Arc::new(Binding {
                peer,
                offer,
                boot,
                cancelled: watch::channel(false).0,
            }),
        );
    }
    fn cancel_all(&self) {
        for binding in self.offers.values() {
            binding.cancel();
        }
    }
}
/// Stream closures, including low-priority interruption during cut-over, only end the
/// current operation. A different source process cannot serve an existing download.
fn download_resumable(reason: LinkLost) -> bool {
    !matches!(reason, LinkLost::PeerRestarted)
}
impl BulkLink for FileLinks {
    fn peer_label(&self, peer: DeviceId) -> String {
        self.log_config
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|config| {
                config
                    .members()
                    .members
                    .iter()
                    .find(|m| m.device == peer)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_else(|| peer.short())
    }
    fn bind(self: Arc<Self>, peer: DeviceId, offer: ClipId) -> Arc<dyn BulkLink> {
        let binding = self.state.lock().unwrap().offers.get(&(peer, offer)).cloned();
        let source_formats = binding.as_ref().and_then(|b| {
            self.links
                .links()
                .by_peer(peer)
                .filter(|e| e.peer_boot == b.boot)
                .and_then(|e| e.peer_formats.clone())
        });
        Arc::new(BoundLink {
            links: self,
            binding,
            source_formats,
        })
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async { Err(io::Error::other("file link must be bound to an accepted offer")) })
    }
}
struct BoundLink {
    links: Arc<FileLinks>,
    binding: Option<Arc<Binding>>,
    source_formats: Option<dto::FormatSupport>,
}
impl BoundLink {
    async fn open_stream(
        &self,
        peer: DeviceId,
        options: LowOptions,
        diagnostic: ReadContext,
    ) -> io::Result<Box<dyn BulkChannel>> {
        let update_activity = oink_foundation::maintenance::process()
            .enter(oink_foundation::maintenance::ActivityKind::FileTransfer)
            .ok_or(io::ErrorKind::WouldBlock)?;
        let binding = self
            .binding
            .as_ref()
            .filter(|b| b.peer == peer && !*b.cancelled.borrow())
            .ok_or_else(|| io::Error::from(io::ErrorKind::Interrupted))?;
        let (scope, mut permission) = {
            let state = self.links.state.lock().unwrap();
            let peer_state = state.peers.get(&peer);
            let flags = u32::from(state.paused)
                | (u32::from(state.input_independent) << 1)
                | (u32::from(peer_state.is_some()) << 2)
                | (u32::from(peer_state.is_some_and(|p| p.boot == binding.boot)) << 3)
                | (u32::from(peer_state.is_some_and(|p| p.inbound.is_some())) << 4);
            let gate = ReadTrace::new(
                read_stage::LOW_OPEN,
                ReadContext {
                    object: peer_state.map_or(0, |p| p.link.0),
                    aux: flags,
                    ..diagnostic
                },
            );
            gate.result(0);
            let current = state
                .peers
                .get(&peer)
                .filter(|p| !state.paused && p.boot == binding.boot)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
            let link = if state.input_independent {
                current.link
            } else {
                current
                    .inbound
                    .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?
                    .link
            };
            (link, current.interrupted.subscribe())
        };
        let mut cancelled = binding.cancelled.subscribe();
        let stream = tokio::select! {
            result = self.links.links.open_low(scope, self.links.kind, options) => result.map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, error))?,
            _ = interrupted(&mut permission, &mut cancelled) => return Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
        };
        if *permission.borrow() || *cancelled.borrow() {
            return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
        }
        Ok(Box::new(FileBulk {
            _update_activity: update_activity,
            stream,
            permission,
            binding: binding.clone(),
        }))
    }
}
impl BulkLink for BoundLink {
    fn supports_namespace(&self) -> bool {
        self.source_formats.as_ref().is_some_and(|source| {
            oink_clipboard::negotiation::namespace_supported(source, &oink_clipboard::negotiation::formats())
        })
    }
    fn select_representation(
        &self,
        inventory: &Inventory,
        representation: RepresentationId,
        target: ClipPlatform,
    ) -> io::Result<(ItemId, String)> {
        let source = self.source_formats.as_ref().ok_or(io::ErrorKind::Unsupported)?;
        oink_clipboard::negotiation::negotiated_selection(
            source,
            &oink_clipboard::negotiation::formats(),
            inventory,
            representation,
            target,
        )
        .map_err(|_| io::ErrorKind::Unsupported.into())
    }
    fn peer_label(&self, peer: DeviceId) -> String {
        self.links.peer_label(peer)
    }
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn is_cancelled(&self) -> bool {
        self.binding.as_ref().is_none_or(|b| *b.cancelled.borrow())
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(binding) = &self.binding else {
                return;
            };
            let mut cancelled = binding.cancelled.subscribe();
            while !*cancelled.borrow_and_update() {
                if cancelled.changed().await.is_err() {
                    return;
                }
            }
        })
    }
    fn open(&self, _peer: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        // Every native operation supplies its exact remaining wire budget.
        Box::pin(async { Err(io::Error::other("file operation requires its byte budget")) })
    }
    fn open_with_options(
        &self,
        peer: DeviceId,
        options: LowOptions,
    ) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        let trace = ReadTrace::new(read_stage::LOW_OPEN, ReadContext::current());
        Box::pin(async move {
            let result = self.open_stream(peer, options, trace.context()).await;
            trace.io(&result);
            result
        })
    }
}
async fn interrupted(permission: &mut watch::Receiver<bool>, cancelled: &mut watch::Receiver<bool>) {
    loop {
        if *permission.borrow_and_update() || *cancelled.borrow_and_update() {
            return;
        }
        tokio::select! {
            result = permission.changed() => if result.is_err() { return; },
            result = cancelled.changed() => if result.is_err() { return; },
        }
    }
}
struct FileBulk {
    _update_activity: oink_foundation::maintenance::Lease,
    stream: LowStream,
    permission: watch::Receiver<bool>,
    binding: Arc<Binding>,
}
pub(crate) fn file_pacing_observer(stream: &LowStream) -> oink_clipboard::sync::files::PacingObserver {
    let observe = stream.pacing_observer();
    Arc::new(move || {
        observe().map(|p| oink_clipboard::sync::files::TransferPacing {
            limited: p.limited,
            rtt: p.rtt,
            target: p.target,
        })
    })
}
impl BulkChannel for FileBulk {
    fn diagnostic_id(&self) -> [u8; 16] {
        self.stream.diagnostic_id()
    }
    fn pacing_observer(&self) -> oink_clipboard::sync::files::PacingObserver {
        file_pacing_observer(&self.stream)
    }
    fn transfer_path(&self) -> &'static str {
        match self.stream.path() {
            oink_network::link::LinkPath::Direct => "direct",
            oink_network::link::LinkPath::Relayed { .. } => "relayed",
        }
    }
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let permission = self.permission.clone();
        let cancelled = self.binding.cancelled.subscribe();
        let operation = self.stream.effect_validity();
        Arc::new(move || !*permission.borrow() && !*cancelled.borrow() && operation())
    }
    fn send<'a>(&'a mut self, frame: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let valid = match frame {
                ContentMsg::TargetStored { delivery, .. } | ContentMsg::Failed { delivery, .. } => {
                    delivery.offer.clip == self.binding.offer
                }
                request => request
                    .request()
                    .is_some_and(|(offer, _)| offer.clip == self.binding.offer),
            };
            if !valid {
                return Err(io::Error::other("different file offer"));
            }
            let wire = oink_clipboard::wire::content(frame).map_err(io::Error::other)?;
            let mut cancelled = self.binding.cancelled.subscribe();
            tokio::select! {
                biased;
                _ = interrupted(&mut self.permission, &mut cancelled) => {
                    self.stream.cancel(); Err(io::Error::from(io::ErrorKind::ConnectionAborted))
                },
                result = self.stream.send(&wire) => result.map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, error)),
            }
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async move {
            let mut cancelled = self.binding.cancelled.subscribe();
            tokio::select! {
                biased;
                _ = interrupted(&mut self.permission, &mut cancelled) => {
                    self.stream.cancel(); Err(io::Error::from(io::ErrorKind::ConnectionAborted))
                },
                result = self.stream.recv() => result.map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted,error))?
                    .map(oink_clipboard::wire::local_content).transpose().map_err(io::Error::other),
            }
        })
    }
    fn finish(&mut self) {
        self.stream.finish();
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_network::link::{Disconnect, StreamDir};
    use oink_protocol::frame::StreamTag;
    const PEER: DeviceId = DeviceId([1; 32]);
    fn stream(link: u64, ordinal: u32) -> StreamId {
        StreamId {
            link: LinkId(link),
            dir: StreamDir::In,
            tag: StreamTag {
                kind: StreamKind::Clipboard,
                ordinal,
            },
        }
    }
    fn boot(nonce: u8) -> Boot {
        Boot {
            device_id: PEER,
            nonce: [nonce; 16],
        }
    }
    fn state() -> State {
        let mut state = State::default();
        state.link_ready(PEER, LinkId(1), boot(1));
        state.inbound(PEER, stream(1, 1));
        state.accept(PEER, ClipId { origin: PEER, seq: 1 });
        state
    }
    #[test]
    fn drag_content_ignores_input_pair_replacement_but_preserves_pause_and_boot_fences() {
        let mut state = State {
            input_independent: true,
            ..Default::default()
        };
        state.link_ready(PEER, LinkId(1), boot(1));
        state.accept(PEER, ClipId { origin: PEER, seq: 1 });
        let binding = state.offers.values().next().unwrap().clone();
        let permission = state.peers[&PEER].interrupted.subscribe();
        assert!(!*permission.borrow());
        state.inbound(PEER, stream(1, 1));
        state.stream_closed(PEER, stream(1, 1));
        assert!(!*permission.borrow(), "I0 replacement is not delivery cancellation");
        state.pause_changed(true);
        state.pause_changed(false);
        assert!(*permission.borrow(), "fast resume cannot revive an old operation");
        assert!(!*state.peers[&PEER].interrupted.borrow());
        assert!(!*binding.cancelled.borrow());
        state.link_ready(PEER, LinkId(2), boot(2));
        assert!(
            *binding.cancelled.borrow(),
            "a new source backend cannot serve an old offer"
        );
    }

    #[test]
    fn late_old_stream_close_does_not_cancel_replacement() {
        let mut state = state();
        let old = state.peers[&PEER].interrupted.subscribe();
        state.inbound(PEER, stream(1, 2));
        assert!(*old.borrow());
        state.stream_closed(PEER, stream(1, 1));
        assert!(!*state.peers[&PEER].interrupted.borrow());
        assert_eq!(state.peers[&PEER].inbound, Some(stream(1, 2)));
        assert!(!*state.offers.values().next().unwrap().cancelled.borrow());
    }
    #[test]
    fn fast_pause_resume_interrupts_old_operation_but_retains_download() {
        crate::test_watchdog::run(
            module_path!(),
            "fast_pause_resume_interrupts_old_operation_but_retains_download",
            None,
            fast_pause_resume_interrupts_old_operation_but_retains_download_scenario(),
        );
    }

    async fn fast_pause_resume_interrupts_old_operation_but_retains_download_scenario() {
        let mut state = state();
        let mut operation = state.peers[&PEER].interrupted.subscribe();
        let mut cancelled = state.offers.values().next().unwrap().cancelled.subscribe();
        state.pause_changed(true);
        state.pause_changed(false);
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            interrupted(&mut operation, &mut cancelled),
        )
        .await
        .unwrap();
        assert!(!*cancelled.borrow());
        assert!(!*state.peers[&PEER].interrupted.borrow());
    }
    #[test]
    fn link_loss_allows_verified_resume_but_a_new_source_process_cancels_download() {
        for reason in [
            LinkLost::ConnectionLost,
            LinkLost::CutoverTimeout,
            LinkLost::Local(Disconnect::Sleeping),
        ] {
            let mut state = state();
            let binding = state.offers.values().next().unwrap().clone();
            let operation = state.peers[&PEER].interrupted.subscribe();
            state.link_lost(PEER, LinkId(1), reason);
            assert!(*operation.borrow());
            assert!(!*binding.cancelled.borrow());
            state.link_ready(PEER, LinkId(2), boot(1));
            state.inbound(PEER, stream(2, 1));
            state.link_lost(PEER, LinkId(1), LinkLost::PeerRestarted);
            assert!(!*binding.cancelled.borrow());
            state.link_ready(PEER, LinkId(3), boot(2));
            assert!(*binding.cancelled.borrow());
        }
    }
}
