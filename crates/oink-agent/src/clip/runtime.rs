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
    v2::{
        Message,
        schema::{WireDto, WireValue},
    },
};
use std::{collections::HashMap, io};
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
    // The owner's decision, not its publication (see `ClipHandle::config_state_changed`).
    let paused = node.fully_paused();
    let data_dir = node.cache_dir();
    std::fs::create_dir_all(&data_dir)?;
    let clock = clock::Clock::load(node.data_dir().join("clipboard-clock.json"))?;

    let (tx, rx) = unbounded_channel();
    let observed = backend.change_count();
    let native = Arc::new(
        group
            .source_grants()
            .map_or_else(content::Registry::default, content::Registry::with_source_grants),
    );
    // A returned exported promise has a native catalog whose source is this
    // device. Recovery must use the same durable token service locally: there
    // is deliberately no network peer/connection to ourselves. Transient Drop
    // authorization still belongs to DndFiles::local, not to this adapter.
    links.pause_changed(!enabled || paused);
    links.set_local_source(node.id(), native.clone());
    let send_policy = oink_clipboard::policy::SharingPolicy::new(enabled && !paused && settings.send);
    let history_store = oink_clipboard::history::Store::load(
        node.data_dir(),
        &data_dir,
        group.applied().group,
        node.id(),
        send_policy.clone(),
    )?;
    native.set_history(history_store.clone());
    let ordering = oink_clipboard::sync::Ordering::new(node.id(), clock.value().max(history_store.clock()));
    let history = history::History::new(history_store.clone());
    let shared = Arc::new(Shared {
        _native_connection: node
            .native_services_enabled()
            .then(|| content::linked::native_connector().connect(links.clone(), tokio::runtime::Handle::current())),
        native,
        native_target: Mutex::default(),
        links,
        settings: Mutex::new(settings),
        policy: send_policy,
        receive_policy: oink_clipboard::policy::SharingPolicy::new(enabled && !paused && settings.receive),
        enabled: AtomicBool::new(enabled),
        paused: AtomicBool::new(paused),
        changes: Mutex::default(),
        generation: AtomicU64::new(0),
        activity: Mutex::default(),
        transfer: Mutex::default(),
        dnd_files: Mutex::default(),
        retained: history_store.bytes.clone(),
        history: history_store,
    });
    // Copied real files stay readable by phones by their paths
    // (clipboard.md「手机的文件保留」); reads follow the sending policy.
    shared
        .native
        .set_announced_files(Arc::new(oink_clipboard::content::announced::AnnouncedFiles::new(
            node.data_dir(),
            shared.policy.clone(),
        )));
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
    let weak4 = weak.clone();
    let task = Task {
        streams,
        ordering,
        clock,
        history,
        fault: None,
        native_source: None,
        capture: None,
        capture_again: false,
        capture_superseded: false,
        backend,
        data_dir,
        me: node.id(),
        tx,
        shared,
        observed,
        copy_report: None,
        apply_report: None,
        last_write: None,
        remote_scope: None,
        cancelled: None,
        provider_retry: None,
        fetch_requested: None,
        native_write: None,
        queued_write: None,
        identity: None,
        resumable: None,
        echo_base: None,
        writing: None,
        recent: VecDeque::new(),
        on_transfer: Arc::new(move |value| {
            if let Some(inner) = weak.upgrade() {
                Node { inner }.emit(crate::api::ApiEvent::FileTransfer(value));
            }
        }),
        on_provider_required: Arc::new(move || {
            if cfg!(target_os = "macos")
                && let Some(inner) = weak4.upgrade()
            {
                Node { inner }.emit(crate::api::ApiEvent::FileProviderRequired);
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
use oink_foundation::clipboard_dedupe::{Identity, Source};
impl Task {
    /// Inline phone heads are the phone's current clipboard publication, also
    /// when a missed delta is repaired by Records or after resuming a link.
    fn phone_head_offer(&self, members: &crate::config_module::MemberTable) -> Option<dto::ClipboardOffer> {
        let window = self.shared.history.window();
        let head = window.records().first()?;
        let phone = members
            .members
            .iter()
            .any(|m| m.device == head.id.clip.origin && m.is_mobile());
        phone.then(|| history::phone_offer(head)).flatten()
    }
    /// A validated history delta can arrive through another PC; ordering uses
    /// the authenticated record author, while native lifetime follows the
    /// actual inbound B0 peer. Both Reliable and Low use this same cut-over.
    fn receive_current_offer(
        &mut self,
        node: &Node,
        members: &crate::config_module::MemberTable,
        peer: DeviceId,
        author: DeviceId,
        offer: dto::ClipboardOffer,
    ) {
        if sensitive_superseded(&offer, &self.shared.history.window()) {
            return;
        }
        if !offer_parties_are_members(&offer, members) {
            tracing::debug!("clipboard offer names a sender or proxy outside the group");
            return;
        }
        if !self.receiving() {
            // Not written here, but a copy made here later must still
            // order after it (a send-only device otherwise announces
            // with a clock the group already used, and loses).
            if let Ok(clock) = offer.clock()
                && self.ordering.witness_clock(clock)
            {
                self.persist();
            }
            // The group's current clip moved on without us: our own
            // older offer is not announced to peers that connect later.
            self.streams.witness_offer(&offer);
            return;
        }
        let report = crate::telemetry::producer().and_then(|p| p.stamp());
        self.observe(self.backend.change_count());
        let stream = self.streams.inbound_id(peer);
        let inline_phone_head = clip_of(&offer).is_some_and(|id| self.inline_phone_head(id));
        if stream.is_none() && !inline_phone_head {
            return;
        }
        match self.ordering.receive(author, offer.clone()) {
            Ok(accepted) => {
                if !self.persist() {
                    return;
                }
                if accepted {
                    // A newer group offer replaces any withdrawn one of ours
                    // and a capture not caused by a local change (resume,
                    // reconfiguration, startup) that is still running.
                    self.resumable = None;
                    self.capture_superseded = self.capture.is_some() && self.echo_base.is_none();
                }
                if accepted && let Some(id) = self.current_id() {
                    if self.streams.witness_offer(&offer) {
                        self.mobile_head(node, &offer, peer);
                    }
                    self.remote_scope = stream.map(|stream| (id, peer, stream));
                    self.cancelled = None;
                    // The clip this clipboard already holds from our
                    // own earlier write, sent again after the source
                    // resumed or reconnected: nothing new to write.
                    if self.identity.as_ref().is_some_and(|held| held.clip == Some(id))
                        && self.last_write == Some(self.backend.change_count())
                    {
                        tracing::debug!(clip = ?id, peer = ?peer, "clipboard offer re-delivered for the clip already written here: not written again");
                        return;
                    }
                    self.apply_report = report.map(|stamp| (id, stamp));
                    self.apply(offer);
                }
            }
            Err(_) => {
                if let Some(stream) = stream {
                    self.streams
                        .stream_closed(stream, oink_network::link::StreamClosed::Local);
                }
            }
        }
    }
    fn mobile_current(
        &mut self,
        node: &Node,
        peer: DeviceId,
        query: dto::ClipboardCurrentQuery,
        proxy_sources: &[(PathBuf, content::SourceLease)],
    ) {
        use crate::mobile_v2::{ClipboardPublisher, V2Identity};
        use oink_network::wire_identity;
        let (Ok(links), Some(group)) = (node.links(), node.group_id()) else {
            return;
        };
        let links = links.links();
        // Answer on any admitted link, direct or through a relay station: a
        // phone that reaches this PC must never wait for a reply it cannot get.
        let Some(entry) = links.by_peer(peer) else {
            return;
        };
        if !links.ready(peer)
            || query.caller() != Ok(wire_identity::boot(entry.peer_boot))
            || query.group() != Ok(oink_protocol::v2::schema::FixedBytes(group.0))
        {
            return;
        }
        self.observe(self.backend.change_count());
        let publisher = ClipboardPublisher::new(V2Identity {
            device: node.id(),
            boot: node.boot(),
            group,
        });
        let mut status = dto::CurrentResultStatus::Empty;
        let mut source = None;
        let mut current = None;
        if self.active()
            && let Some(offer) = self.streams.witnessed_offer()
            && let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id)
        {
            let origin = id.clip.origin;
            source = Some(oink_clipboard::wire::device(origin));
            if origin != node.id() && !links.ready(origin) {
                status = dto::CurrentResultStatus::SourceOffline;
            } else if origin == node.id()
                && offer.proxy() == Ok(Some(oink_clipboard::wire::device(node.id())))
                && !proxy_sources
                    .iter()
                    .any(|(path, lease)| path.exists() && lease.inventory().id == id)
            {
                status = dto::CurrentResultStatus::Expired;
            } else if (origin == node.id() && self.sending()) || (origin != node.id() && self.receiving()) {
                if offer.item_count().unwrap_or(u32::MAX) <= u32::from(query.max_items().unwrap_or_default()) {
                    status = dto::CurrentResultStatus::Ready;
                    current = Some(offer.clone());
                } else {
                    status = dto::CurrentResultStatus::Unsupported;
                }
            }
        }
        // Background phone publishers cannot subscribe to history. Return the
        // durable maximum even for Empty, so their next author clock is newer
        // than records whose bodies have already left the shared window.
        let clock = self.history.store.clock();
        if let Ok(mut response) = publisher.current_result(&query, status, current, source.clone(), None, clock) {
            if oink_protocol::v2::frame::encode(&response, oink_protocol::v2::limits::RELIABLE_FRAME).is_err() {
                let Ok(fallback) =
                    publisher.current_result(&query, dto::CurrentResultStatus::Unsupported, None, source, None, clock)
                else {
                    return;
                };
                response = fallback;
            }
            let _ = self.streams.send(peer, &response, Instant::now());
        }
    }

    fn mobile_head(&mut self, node: &Node, offer: &dto::ClipboardOffer, origin: DeviceId) {
        use crate::mobile_v2::{ClipboardPublisher, V2Identity};
        let (Ok(links), Some(group)) = (node.links(), node.group_id()) else {
            return;
        };
        let publisher = ClipboardPublisher::new(V2Identity {
            device: node.id(),
            boot: node.boot(),
            group,
        });
        let message = publisher.head_update(offer.clone(), true);
        for entry in &links.links().links {
            if entry.peer != origin
                && entry.state == oink_network::link::LinkState::Ready
                && entry.permits(oink_protocol::v2::registry::type_id::ClipboardHeadUpdate)
                && node
                    .device_facts(entry.peer)
                    .is_some_and(|facts| facts.device_kind() == Ok(dto::DeviceKind::Mobile))
            {
                let _ = self.streams.send(entry.peer, &message, Instant::now());
            }
        }
    }

    fn accept_mobile_share(
        &mut self,
        node: &Node,
        peer: DeviceId,
        begin: dto::ClipboardShareBegin,
        pending: &mut HashMap<mobile::Operation, mobile::Pending>,
    ) {
        let Ok(id) = begin.operation_id() else { return };
        let (Ok(links), Some(group)) = (node.links(), node.group_id()) else {
            return;
        };
        let links = links.links();
        let Some(entry) = links.by_peer(peer) else { return };
        let key = (peer, id.0);
        let now = Instant::now();
        pending.retain(|_, share| share.expires > now);
        // The reason tells the phone whether another PC could take this share
        // (protocol-v2 5.2): ReceiveOff, Paused and Busy describe this PC only.
        let refusal = if self.shared.paused.load(Ordering::Acquire) {
            Some(dto::ShareRejectReason::Paused)
        } else if !self.receiving() || (begin.delivery() == Ok(dto::ShareDelivery::Directed) && !self.sending()) {
            Some(dto::ShareRejectReason::ReceiveOff)
        } else if !links.ready(peer) {
            Some(dto::ShareRejectReason::Invalid)
        } else if let Err(reason) = mobile::admissible(&begin, entry, node.boot()) {
            Some(reason)
        } else if pending.contains_key(&key) {
            Some(dto::ShareRejectReason::Duplicate)
        } else if pending.len() >= 128 {
            Some(dto::ShareRejectReason::Busy)
        } else {
            None
        };
        let reply = if let Some(reason) = refusal {
            mobile::rejection(id.0, reason)
        } else {
            let is_proxy = begin.delivery() == Ok(dto::ShareDelivery::Directed);
            pending.insert(
                key,
                mobile::Pending {
                    begin,
                    link: entry.link,
                    group,
                    revision: self.backend.change_count(),
                    expires: now + Duration::from_secs(30),
                    receive_permit: self.shared.receive_policy.publication_guard(),
                    proxy_permit: is_proxy.then(|| self.shared.policy.publication_guard()),
                },
            );
            Message::ClipboardShareAccept(
                dto::ClipboardShareAccept::from_raw([(1, id.write())].into()).expect("valid share acceptance"),
            )
        };
        let _ = self.streams.send(peer, &reply, now);
    }

    async fn publish_mobile_share(
        &mut self,
        node: &Node,
        result: mobile::Received,
        proxy_sources: &mut Vec<(PathBuf, content::SourceLease)>,
    ) {
        let mobile::Received {
            operation: (peer, id),
            pending,
            valid,
            payload,
        } = result;
        let is_proxy = pending.begin.delivery() == Ok(dto::ShareDelivery::Directed);
        let status = async {
            let payload = payload?;
            let links = node.links().map_err(|_| io::ErrorKind::NotConnected)?;
            let revision = pending.revision;
            let receiver = node.clone();
            let (group, link) = (pending.group, pending.link);
            let (receive_permit, proxy_permit) = (pending.receive_permit.clone(), pending.proxy_permit.clone());
            // The upload is still this operation's: same group, link and permits.
            let admitted: content::ReaderGuard = Arc::new(move || {
                if valid()
                    && receiver.group_id() == Some(group)
                    && receive_permit().is_ok()
                    && proxy_permit.as_ref().is_none_or(|permit| permit().is_ok())
                    && links.links().by_peer(peer).is_some_and(|entry| entry.link == link)
                {
                    Ok(())
                } else {
                    Err(io::ErrorKind::Interrupted.into())
                }
            });
            // Only writing into this PC's system clipboard follows the
            // clipboard revision seen when the upload was accepted: a copy
            // made meanwhile keeps the clipboard (H9, user rule 2026-11-09).
            // A proxied upload is still cached and announced to the group.
            let unchanged: content::ReaderGuard = {
                let admitted = admitted.clone();
                let backend = self.backend.clone();
                Arc::new(move || {
                    admitted()?;
                    if backend.change_count() == revision {
                        Ok(())
                    } else {
                        Err(io::ErrorKind::Interrupted.into())
                    }
                })
            };
            let seq = if is_proxy {
                // Ordering::capture allocates this same value below.
                self.ordering.clock().checked_add(1).ok_or(io::ErrorKind::InvalidData)?
            } else {
                // A direct share has no announced clip. Its native write still
                // needs an identity (the clipboard marker); reserve a clock
                // value so the next local copy never gets the same ClipId (M12).
                let seq = self.ordering.reserve().ok_or(io::ErrorKind::InvalidData)?;
                if !self.persist() {
                    return Err(io::Error::other("mobile share clock could not be persisted"));
                }
                seq
            };
            let offer_id = oink_clipboard::model::OfferId {
                clip: ClipId { origin: node.id(), seq },
                generation: seq,
            };
            self.cancel_native_write();
            // Names the phone's received-files folder the first time it is used.
            oink_clipboard::received::remember_source(peer, node.device_name(peer));
            let backend = self.backend.clone();
            let (count, payload, spec, fingerprint) = tokio::task::spawn_blocking(move || {
                admitted()?;
                let spec = payload.spec(offer_id)?;
                // Cached locally: every representation is cheap to read.
                let fingerprint = oink_clipboard::dedupe::fingerprint(spec.contents.as_ref(), false);
                let written = if unchanged().is_err() {
                    None
                } else {
                    match crate::mobile_v2::write_cached_mobile(backend.as_ref(), spec.clone(), unchanged) {
                        Ok(count) => Some(count),
                        Err(error) if is_proxy => {
                            // The proxy cache serves the group either way.
                            tracing::debug!(%error, "proxied upload not written into this clipboard");
                            None
                        }
                        Err(error) => return Err(io::Error::other(error)),
                    }
                };
                // The write may have failed because the operation ended.
                admitted()?;
                Ok::<_, io::Error>((written, payload, spec, fingerprint))
            })
            .await
            .map_err(io::Error::other)??;
            if is_proxy {
                return self
                    .announce_proxy(
                        node,
                        peer,
                        id,
                        &pending,
                        payload,
                        spec,
                        &fingerprint,
                        count,
                        proxy_sources,
                    )
                    .await;
            }
            // A direct share exists only as this write: a changed clipboard fails it.
            let count = count.ok_or(io::ErrorKind::Interrupted)?;
            {
                // A direct upload is already addressed to every target. Retire
                // the previous PC offer and suppress capture of our native write.
                self.observed = count;
                self.last_write = Some(count);
                self.echo_base = None;
                self.writing = None;
                self.identity = Some(Held {
                    identity: Identity {
                        fingerprint,
                        source: Source::Device(peer),
                    },
                    clip: None,
                });
                self.ordering.local_change(count, Instant::now());
                self.ordering.written(count);
                self.streams.set_offer(None);
            }
            drop(payload);
            Ok(())
        }
        .await;
        let status = match status {
            Ok(()) => dto::ShareStatus::Complete,
            Err(error) => {
                tracing::info!(kind = ?error.kind(), %error, proxy = is_proxy, "mobile clipboard publication failed");
                dto::ShareStatus::Failed
            }
        };
        let _ = self.streams.send(peer, &mobile::result(id, status), Instant::now());
    }

    /// Caches a proxied upload, registers it as this PC's offer and announces
    /// it. `written` is the native change count when it was also written into
    /// this PC's clipboard.
    #[allow(clippy::too_many_arguments)]
    async fn announce_proxy(
        &mut self,
        node: &Node,
        peer: DeviceId,
        id: [u8; 16],
        pending: &mobile::Pending,
        payload: mobile::Payload,
        spec: oink_clipboard::NativeSpec,
        fingerprint: &oink_foundation::clipboard_dedupe::Fingerprint,
        written: Option<u64>,
        proxy_sources: &mut Vec<(PathBuf, content::SourceLease)>,
    ) -> io::Result<()> {
        let digests = oink_clipboard::dedupe::digests(fingerprint, &spec.inventory).unwrap_or_default();
        // Not written: the offer has no native source; 0 is never a written count.
        let offer = self
            .ordering
            .capture(
                oink_clipboard::wire::inventory(&spec.inventory).map_err(io::Error::other)?,
                digests,
                written.unwrap_or(0),
                spec.sensitivity,
                crate::layout_sync::now_ms(),
            )
            .map_err(io::Error::other)?
            .ok_or(io::ErrorKind::InvalidData)?;
        if offer.id().and_then(oink_clipboard::wire::local_offer_id).ok() != Some(spec.inventory.id) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut raw = offer.into_raw();
        raw.insert(8, oink_clipboard::wire::device(peer).write());
        raw.insert(9, oink_clipboard::wire::device(node.id()).write());
        raw.insert(10, oink_protocol::v2::schema::FixedBytes(id).write());
        raw.insert(11, pending.begin.message_id().map_err(io::Error::other)?.write());
        // The message keeps the sender's item count; a photo's extra bitmap
        // item is a representation of the same message item.
        raw.insert(
            12,
            u32::from(pending.begin.item_count().map_err(io::Error::other)?).write(),
        );
        let offer = dto::ClipboardOffer::from_raw(raw).map_err(io::Error::other)?;
        let path = payload.cache_path().to_owned();
        let lease = self
            .shared
            .native
            .register(self.shared.policy.provider(spec.contents))?;
        if !self.persist() {
            return Err(io::Error::other("mobile proxy clock could not be persisted"));
        }
        let group = pending.group;
        tokio::task::spawn_blocking({
            let offer = offer.clone();
            move || payload.persist(group, &offer)
        })
        .await
        .map_err(io::Error::other)??;
        // A completed proxy upload is durable in the shared window before its
        // success result is sent; a crash cannot leave a published orphan cache.
        lease.set_history(
            offer.clock().unwrap_or(0),
            matches!(
                offer.sensitivity(),
                Ok(dto::Sensitivity::Concealed | dto::Sensitivity::Transient)
            ),
        );
        let capture = history::Capture {
            group,
            store: self.shared.history.clone(),
            registry: self.shared.native.clone(),
            offer: offer.clone(),
            provider: lease.provider(),
            fingerprint: Some(fingerprint.clone()),
            proxy: Some(path.clone()),
        };
        let changes = tokio::task::spawn_blocking(move || capture.run())
            .await
            .map_err(io::Error::other)??;
        let _ = self.tx.send(Input::HistoryReady(group, changes));
        proxy_sources.push((path, lease));
        // The previous own offer's native source no longer backs the current
        // offer (the proxy cache does).
        self.native_source = None;
        self.resumable = None;
        if let Some(count) = written {
            self.observed = count;
            self.last_write = Some(count);
            self.echo_base = None;
            self.writing = None;
            self.ordering.written(count);
            let identity = Identity {
                fingerprint: fingerprint.clone(),
                source: Source::Device(peer),
            };
            // A write-back of this clipboard names the proxy offer (M12).
            self.remember(spec.inventory.id.clip, identity.clone(), Some((self.me, offer.clone())));
            self.identity = Some(Held {
                identity,
                clip: Some(spec.inventory.id.clip),
            });
        }
        self.streams.set_offer(Some(offer.clone()));
        self.mobile_head(node, &offer, peer);
        Ok(())
    }

    fn register_mobile_sources(
        &self,
        restored: Vec<(PathBuf, dto::ClipboardOffer, oink_clipboard::NativeSpec)>,
    ) -> Vec<(PathBuf, content::SourceLease)> {
        restored
            .into_iter()
            .filter_map(|(path, offer, spec)| {
                if offer
                    .id()
                    .and_then(oink_clipboard::wire::local_offer_id)
                    .ok()?
                    .clip
                    .origin
                    != self.me
                {
                    return None;
                }
                if !self.shared.history.proxy_retained(&path) {
                    let _ = oink_clipboard::cache::remove_unless_active(&path);
                    return None;
                }
                let lease = self
                    .shared
                    .native
                    .register(self.shared.policy.provider(spec.contents))
                    .ok()?;
                lease.set_history(
                    offer.clock().ok()?,
                    matches!(
                        offer.sensitivity(),
                        Ok(dto::Sensitivity::Concealed | dto::Sensitivity::Transient)
                    ),
                );
                Some((path, lease))
            })
            .collect()
    }

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
        let mut mobile_serving = tokio::task::JoinSet::new();
        let mut mobile_pending = HashMap::new();
        let mut proxy_group = group.applied().group;
        let initial_group = proxy_group;
        let mut maintaining = tokio::task::JoinSet::new();
        let mut maintenance = tokio::time::interval(Duration::from_secs(1));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let data = self.data_dir.clone();
        let backend = self.backend.clone();
        let registry = self.shared.native.clone();
        let retained = self.shared.retained.clone();
        let mut next_sweep = Instant::now() + RETAINED_SWEEP;
        let restored = tokio::task::spawn_blocking(move || {
            oink_clipboard::store::recover(&data);
            retained.sweep();
            let restored = mobile::restore(&data, initial_group, true);
            backend.initialize_store(data);
            let _ = registry.check_clipboard(backend.as_ref());
            restored
        })
        .await
        .unwrap_or_default();
        let mut proxy_sources = self.register_mobile_sources(restored);
        self.streams
            .set_history_summary(self.history.summary(proxy_group), false);
        self.streams.pause_changed(!self.active());
        let mut history_ids: std::collections::HashSet<_> =
            self.shared.history.window().records().iter().map(|r| r.id).collect();
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
                .chain(self.sending().then(|| self.ordering.deadline()).flatten())
                .min()
                .unwrap_or(Instant::now() + Duration::from_millis(500));
            tokio::select! {
                _=stop.changed()=>break,
                _=wake.notified()=>{},
                _=tokio::time::sleep_until(deadline.into())=>{},
                _=serving.join_next(),if !serving.is_empty()=>{},
                result=mobile_serving.join_next(),if !mobile_serving.is_empty()=>{
                    if let Some(Ok(result)) = result {
                        self.publish_mobile_share(&node, result, &mut proxy_sources).await;
                    }
                },
                _=maintaining.join_next(),if !maintaining.is_empty()=>{},
                _=maintenance.tick(),if maintaining.is_empty()=>{
                    mobile_pending.retain(|_, pending: &mut mobile::Pending| pending.expires > Instant::now());
                    proxy_sources.retain(|(path, lease)| {
                        if path.exists() && self.shared.history.proxy_retained(path) { return true; }
                        lease.cancel();
                        false
                    });
                    let registry = self.shared.native.clone();
                    let backend = self.backend.clone();
                    let store = self.shared.history.clone();
                    let history_tx = self.tx.clone();
                    let history_group = proxy_group;
                    let sweep = (Instant::now() >= next_sweep).then(|| {
                        next_sweep = Instant::now() + RETAINED_SWEEP;
                        self.shared.retained.clone()
                    });
                    // Metadata-only even while sharing/watch is paused. Native
                    // identity and durable cleanup never run on the actor.
                    maintaining.spawn_blocking(move || {
                        if let Err(error) = registry.check_clipboard(backend.as_ref()) {
                            tracing::debug!(kind = ?error.kind(), "source clipboard retention check deferred");
                        }
                        let _ = store.cleanup();
                        if let Ok(entries) = store.enforce_cap(oink_clipboard::history::MAX_BYTES) {
                            let _ = history_tx.send(Input::HistoryReady(history_group, entries.into_iter().map(oink_foundation::clipboard_history::HistoryChange::Holder).collect()));
                        }
                        if let Some(retained) = sweep {
                            retained.sweep();
                        }
                    });
                },
                change=changes.next()=>match change {
                    Some(ConfigChange::State{..})=>{
                        let applied = group.applied();
                        if proxy_group != applied.group {
                            proxy_group = applied.group;
                            self.history.reset();
                            if let Err(error) = self.shared.history.reset_group(proxy_group) { tracing::warn!(%error, "history group reset failed"); }
                            self.streams.set_history_summary(self.history.summary(proxy_group), self.active());
                            mobile_pending.clear();
                            mobile_serving.abort_all();
                            for (_, lease) in proxy_sources.drain(..) {
                                lease.cancel();
                            }
                            self.streams.set_offer(None);
                        }
                        // The owner's pause decision, not its later publication.
                        handle.config_state_changed(&node);
                    },
                    Some(ConfigChange::Facts{device,capabilities_changed})=>{
                        // A member's device kind is a fact; a phone's first facts make it mobile.
                        self.streams.facts_changed(device,capabilities_changed);
                        let pcs=group.members().members.iter().filter(|m|!m.is_mobile()).map(|m|m.device).collect();
                        let _=self.shared.history.retain_pc_holders(&pcs);
                    },
                    Some(ConfigChange::Members)=>{
                        let members=group.members().members.iter().filter(|m|!m.is_mobile()).map(|m|m.device).collect();
                        let _=self.shared.history.retain_pc_holders(&members);
                    },
                    Some(_)=>{},None=>break,
                },
                event=port.next()=>match event {
                    Some(ModuleEvent::LinkReady{link,peer})=>self.streams.link_ready(link,peer),
                    Some(ModuleEvent::LinkLost{link,peer,reason})=>{
                        self.streams.link_lost(link,peer,reason);
                        self.history.disconnected(peer);
                        mobile_pending.retain(|_, pending| pending.link != link);
                    },
                    Some(ModuleEvent::Inbound(stream))=>self.streams.inbound(stream),
                    Some(ModuleEvent::InboundLow(operation))=>self.streams.inbound_low(*operation),
                    Some(ModuleEvent::StreamClosed{stream,reason})=>self.streams.stream_closed(stream,reason),
                    Some(ModuleEvent::ProtocolViolation{..})=>{},
                    Some(ModuleEvent::PathChanged{..})=>{},None=>break,
                },
                command=commands.recv()=>match command {
                    Some(Input::Captured(before,result,key,identity))=>{
                        oink_clipboard::copy_diagnostics::capture_handoff(
                            oink_clipboard::copy_diagnostics::Stage::CapturedReceived, before, 0, 0,
                        );
                        self.captured(before,result,key,identity);
                    },
                    Some(Input::Applied{id,ticket,publication})=>self.applied(id,ticket,publication),
                    Some(Input::LocalChange(count))=>if self.active(){self.observe(count);},
                    // A newer change sent its own Reconfigure (see `Shared::generation`).
                    Some(Input::Reconfigure(generation))=>if generation == self.shared.generation.load(Ordering::Acquire) {
                        mobile_pending.clear();
                        mobile_serving.abort_all();
                        proxy_sources.clear();
                        self.history.suspend();
                        self.reconfigure();
                        self.streams.set_history_summary(self.history.summary(proxy_group), self.active());
                        if self.active() { self.history.resumed(proxy_group); }
                        if self.sending() {
                            let data = self.data_dir.clone();
                            let restored = tokio::task::spawn_blocking(move || mobile::restore(&data, proxy_group, false))
                                .await.unwrap_or_default();
                            proxy_sources = self.register_mobile_sources(restored);
                        }
                    },
                    Some(Input::HistoryReady(history_group,changes)) => {
                        if history_group == proxy_group {
                            for change in changes {
                                if self.active() { self.history.forward(&node,change,None); }
                            }
                        }
                    },
                    Some(Input::HistoryMaterialized(history_group,inventory,representation,mut reader)) => {
                        if history_group == proxy_group {
                            let store = self.shared.history.clone();
                            let tx = self.tx.clone();
                            tokio::task::spawn_blocking(move || {
                                if store.group() == history_group && store.materialized(history_group,&inventory,representation,&mut reader).is_ok() {
                                    let scope = store.scope(inventory.id);
                                    let changes = store.holder(inventory.id,oink_foundation::clipboard_history::HolderState::Holding,scope).ok().flatten().map(oink_foundation::clipboard_history::HistoryChange::Holder).into_iter().collect();
                                    let _ = tx.send(Input::HistoryReady(history_group,changes));
                                }
                            });
                        }
                    },
                    Some(Input::FetchFiles)=>self.fetch(),
                    Some(Input::CancelFiles)=>{self.cancel_native_write();self.fetch_requested=None;self.cancelled=self.current_id();if let Some(mut status)=handle.transfer(){status.phase=FileTransferPhase::Cancelled;self.set_transfer(Some(status));}},
                    #[cfg(target_os = "macos")]
                    Some(Input::FileProviderAvailable)=>if self.active()&&self.provider_retry.is_some()&& let Some(offer)=self.ordering.current().cloned(){self.apply(offer);},
                    Some(Input::Progress(id,batch,value))=>self.progress(id,batch,value),
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
                let mut author = peer;
                let message = if is_history(&message) {
                    if !self.active() {
                        continue;
                    }
                    if let Err(error) = self.history.receive(&node, peer, message) {
                        tracing::debug!(%error, "history message rejected");
                        continue;
                    }
                    let Some(offer) = self.phone_head_offer(&group.members()) else {
                        continue;
                    };
                    if let Some(id) = clip_of(&offer) {
                        author = id.origin;
                        self.shared.links.accept(peer, id);
                    }
                    Message::ClipboardOffer(offer)
                } else {
                    message
                };
                let offer = match message {
                    Message::ClipboardOffer(offer) => offer,
                    Message::ClipboardCurrentQuery(query) => {
                        self.mobile_current(&node, peer, query, &proxy_sources);
                        continue;
                    }
                    Message::ClipboardShareBegin(begin) => {
                        self.accept_mobile_share(&node, peer, begin, &mut mobile_pending);
                        continue;
                    }
                    Message::ClipboardShareCancel(cancel) => {
                        if let Ok(id) = cancel.operation_id() {
                            mobile_pending.remove(&(peer, id.0));
                        }
                        continue;
                    }
                    _ => continue,
                };
                self.receive_current_offer(&node, &group.members(), peer, author, offer);
            }
            for mut operation in self.streams.take_low() {
                if is_history(&operation.first) {
                    if self.active() && operation.stream.effect_validity()() {
                        let peer = operation.stream.peer();
                        if let Err(error) = self.history.receive(&node, peer, operation.first) {
                            tracing::debug!(%error,"history Low merge rejected");
                            operation.stream.cancel();
                            continue;
                        }
                        if let Some(offer) = self.phone_head_offer(&group.members())
                            && let Some(id) = clip_of(&offer)
                        {
                            self.shared.links.accept(peer, id);
                            self.receive_current_offer(&node, &group.members(), peer, id.origin, offer);
                        }
                        operation.stream.finish();
                        serving.spawn(async move { while let Ok(Some(_)) = operation.stream.recv().await {} });
                    } else {
                        operation.stream.cancel();
                    }
                    continue;
                }
                if let Some(key) = mobile::operation(operation.stream.peer(), &operation.first) {
                    if let Some(pending) = mobile_pending.remove(&key)
                        && pending.link == operation.stream.link()
                        && pending.expires > Instant::now()
                        && mobile_serving.len() < 32
                    {
                        mobile_serving.spawn(mobile::receive(
                            operation,
                            key,
                            pending,
                            self.data_dir.join(crate::mobile_v2::PROXY_CACHE_DIR),
                        ));
                    } else {
                        operation.stream.cancel();
                    }
                } else {
                    let handle = handle.clone();
                    serving.spawn(async move {
                        handle.serve_link(operation).await;
                    });
                }
            }
            let window = self.shared.history.window();
            let current_ids: std::collections::HashSet<_> = window.records().iter().map(|r| r.id).collect();
            // Automatic bytes can finish before their authored metadata delta.
            // Replay them exactly when the record becomes part of the window.
            // Store rechecks membership at commit, so a late download cannot
            // recreate an evicted payload directory.
            if let Some((_, remote)) = self.shared.native_target.lock().unwrap().as_ref() {
                let id = remote.inventory().id;
                if current_ids.contains(&id) && !history_ids.contains(&id) {
                    remote.replay_materialized();
                }
            }
            let retired = history_ids.difference(&current_ids).copied().collect::<Vec<_>>();
            for id in retired {
                self.purge_history_offer(id);
            }
            if let Some(offer) = self
                .ordering
                .current()
                .filter(|offer| sensitive_superseded(offer, &window))
                .cloned()
                && let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id)
            {
                self.purge_history_offer(id);
            }
            history_ids = current_ids;
            proxy_sources.retain(|(path, lease)| {
                if self.shared.history.proxy_retained(path) {
                    true
                } else {
                    lease.cancel();
                    false
                }
            });
            if let Some(max) = window.records().first().map(|r| r.clock)
                && self.ordering.witness_clock(max)
            {
                self.persist();
            }
            self.streams
                .set_history_summary(self.history.summary(proxy_group), false);
            if self.active() {
                for record in window.records() {
                    let scope = self.shared.history.scope(record.id);
                    if scope.0 != 0
                        && let Ok(Some(entry)) = self.shared.history.holder(
                            record.id,
                            oink_foundation::clipboard_history::HolderState::Holding,
                            scope,
                        )
                    {
                        self.history.forward(
                            &node,
                            oink_foundation::clipboard_history::HistoryChange::Holder(entry),
                            None,
                        );
                    }
                }
                let owned = self
                    .shared
                    .history
                    .window()
                    .holders()
                    .iter()
                    .filter(|e| {
                        e.holder == self.me && e.state == oink_foundation::clipboard_history::HolderState::Holding
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                for entry in owned {
                    if self.shared.history.scope(entry.record_id).0 == 0
                        && let Ok(Some(entry)) = self.shared.history.holder(
                            entry.record_id,
                            oink_foundation::clipboard_history::HolderState::Released,
                            oink_foundation::clipboard_history::HolderScope(0),
                        )
                    {
                        self.history.forward(
                            &node,
                            oink_foundation::clipboard_history::HistoryChange::Holder(entry),
                            None,
                        );
                    }
                }
                self.history.flush(&mut self.streams);
            }
            if self.sending() && self.ordering.capture_due(Instant::now()) {
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
        mobile_serving.abort_all();
        maintaining.abort_all();
        while serving.join_next().await.is_some() {}
        let _ = self.backend.set_watch_paused(true);
    }
}
/// Sweep invalid thumbnail cache entries; history owns payload lifetime.
const RETAINED_SWEEP: Duration = Duration::from_secs(10 * 60);
/// protocol-v2 §2/§4.1: the DTO validator already enforces the legal
/// direct/proxied sender combinations; the named devices must also be members
/// of this group. The announcing origin itself is authenticated by the link.
pub(super) fn offer_parties_are_members(
    offer: &dto::ClipboardOffer,
    members: &crate::config_module::MemberTable,
) -> bool {
    let member = |id: &dto::DeviceId| {
        members
            .members
            .iter()
            .any(|m| oink_clipboard::wire::device(m.device) == *id)
    };
    offer.real_sender().is_ok_and(|sender| member(&sender))
        && offer.proxy().is_ok_and(|proxy| proxy.as_ref().is_none_or(member))
}
impl ClipHandle {
    /// Why this device does not serve its clipboard content now, if it does not.
    fn not_sending(&self) -> Option<Unavailable> {
        if self.shared.policy.enabled() {
            None
        } else if self.shared.paused.load(Ordering::Acquire) {
            Some(Unavailable::Paused)
        } else {
            Some(Unavailable::SendDisabled)
        }
    }
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
        let message = if let Message::ClipboardContentRead(read) = &operation.first {
            if kind != StreamKind::Clipboard {
                operation.stream.cancel();
                return;
            }
            let mut channel = ObservedServe::new(operation.stream, self.clone());
            if let Some(reason) = self.not_sending() {
                // The reader learns why (not a bare "interrupted").
                if let (Ok(offer), Ok(representation)) = (
                    read.offer().and_then(oink_clipboard::wire::local_offer_id),
                    read.representation_id(),
                ) {
                    let _ = channel
                        .send(&ContentMsg::RepresentationFailed {
                            offer,
                            representation: oink_clipboard::model::RepresentationId(representation),
                            reason,
                        })
                        .await;
                }
                channel.finish();
                let _ = channel.recv().await;
                return;
            }
            let _ = self
                .shared
                .native
                .serve_mobile_representation(peer, read, Some(self.shared.retained.as_ref()), &mut channel)
                .await;
            channel.finish();
            let _ = channel.recv().await;
            return;
        } else if let Message::ContentThumbnailRead(read) = &operation.first {
            if kind != StreamKind::Clipboard || !self.shared.policy.enabled() {
                operation.stream.cancel();
                return;
            }
            let link = operation.stream.link();
            let Some(reply) = self.thumbnail(peer, link, read).await else {
                operation.stream.cancel();
                return;
            };
            let mut stream = operation.stream;
            if stream.send(&reply).await.is_ok() {
                stream.finish();
                let _ = stream.recv().await;
            }
            return;
        } else {
            let Ok(message) = oink_clipboard::wire::local_content(operation.first) else {
                operation.stream.cancel();
                return;
            };
            message
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
        let refused = (kind == StreamKind::Clipboard).then(|| self.not_sending()).flatten();
        match message {
            ContentMsg::ReadRepresentation {
                offer, representation, ..
            } if refused.is_some() => {
                let mut channel = LinkBulk(operation.stream);
                let _ = channel
                    .send(&ContentMsg::RepresentationFailed {
                        offer,
                        representation,
                        reason: refused.expect("guarded"),
                    })
                    .await;
                channel.finish();
                let _ = channel.recv().await;
            }
            ContentMsg::TargetStored { delivery } => {
                self.receipt(operation.stream, kind, peer, delivery, true).await;
            }
            ContentMsg::Failed { delivery, .. } => {
                self.receipt(operation.stream, kind, peer, delivery, false).await;
            }
            request if request.request().is_some() || request.persistent_request() => {
                let persistent = request.persistent_request();
                let registry = match kind {
                    StreamKind::Clipboard
                        if self.shared.policy.enabled()
                            && (persistent
                                || request.request().is_some_and(|(_, demand)| demand == Demand::Clipboard)) =>
                    {
                        self.shared.native.clone()
                    }
                    StreamKind::Dnd => {
                        let files = self.shared.dnd_files.lock().unwrap().clone();
                        let Some(files) = files else {
                            operation.stream.cancel();
                            return;
                        };
                        if !persistent {
                            let (offer, demand) = request.request().unwrap();
                            if !files.authorize(peer, offer, demand, diagnostic).await {
                                operation.stream.cancel();
                                return;
                            }
                        }
                        // Later file reads are authorized by the durable grant,
                        // never by an expired drag's input-holder epoch.
                        files.native.clone()
                    }
                    _ => {
                        operation.stream.cancel();
                        return;
                    }
                };
                if !self.shared.links.peer_cloud_links(peer, operation.stream.link())
                    && !registry.non_file_request(&request)
                {
                    operation.stream.cancel();
                    return;
                }
                let Some(profile) = self.shared.links.peer_formats(peer, operation.stream.link()) else {
                    operation.stream.cancel();
                    return;
                };
                let omit_ds_store = self
                    .shared
                    .links
                    .peer_os(peer)
                    .is_some_and(|os| self.settings().omit_ds_store_for(std::env::consts::OS, &os));
                registry.set_peer_file_filter(peer, omit_ds_store);
                registry.set_peer_exec_bits(peer, self.shared.links.peer_exec_bits(peer, operation.stream.link()));
                registry.set_peer_safe_symlinks(
                    peer,
                    self.shared.links.peer_safe_symlinks(peer, operation.stream.link()),
                );
                let mut channel = ObservedServe::new(operation.stream, self.clone());
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
    /// The answer to a phone's `ContentThumbnailRead` (protocol-v2 §4.4), or
    /// `None` for a malformed request. A file target needs the same file
    /// link permission as reading that file.
    async fn thumbnail(
        &self,
        peer: DeviceId,
        link: oink_network::link::LinkId,
        read: &dto::ContentThumbnailRead,
    ) -> Option<Message> {
        use oink_clipboard::{content::ThumbnailOutcome, retained::Target};
        let operation = read.operation_id().ok()?;
        let offer = oink_clipboard::wire::local_offer_id(read.offer().ok()?).ok()?;
        let target = match (
            read.item_id().ok()?,
            read.representation_id().ok()?,
            read.node().ok()?,
            read.version().ok()?,
        ) {
            (Some(item), Some(representation), None, None) => Target::Representation { item, representation },
            (None, None, Some(node), Some(version)) => Target::File { node, version },
            _ => return None,
        };
        let edge = u32::from(read.max_edge().ok()?);
        let max_bytes = read.max_bytes().ok()? as usize;
        let outcome = if matches!(target, Target::File { .. }) && !self.shared.links.peer_cloud_links(peer, link) {
            ThumbnailOutcome::Unsupported
        } else {
            let (registry, store) = (self.shared.native.clone(), self.shared.retained.clone());
            tokio::task::spawn_blocking(move || {
                registry.thumbnail(offer, target, edge, max_bytes, Some(store.as_ref()))
            })
            .await
            .unwrap_or(ThumbnailOutcome::Failed)
        };
        let status = match &outcome {
            ThumbnailOutcome::Ready(_) => dto::ThumbnailStatus::Ready,
            ThumbnailOutcome::Expired => dto::ThumbnailStatus::Expired,
            ThumbnailOutcome::Unsupported => dto::ThumbnailStatus::Unsupported,
            ThumbnailOutcome::Failed => dto::ThumbnailStatus::Failed,
        };
        let mut raw = oink_protocol::v2::RawMap::from([(1, operation.write()), (2, status.write())]);
        if let ThumbnailOutcome::Ready(thumbnail) = outcome {
            raw.insert(
                3,
                oink_protocol::v2::schema::Text::<32>(thumbnail.mime.to_owned()).write(),
            );
            raw.insert(4, thumbnail.width.write());
            raw.insert(5, thumbnail.height.write());
            raw.insert(
                6,
                oink_protocol::v2::schema::Bytes::<{ dto::THUMBNAIL_MAX_BYTES }>(thumbnail.bytes).write(),
            );
            raw.insert(7, thumbnail.source_bytes.write());
        }
        let reply = dto::ContentThumbnail::from_raw(raw);
        if let Err(error) = &reply {
            tracing::warn!(?error, "thumbnail reply could not be encoded");
        }
        Some(Message::ContentThumbnail(reply.ok()?))
    }
    async fn receipt(
        &self,
        mut stream: LowStream,
        kind: StreamKind,
        peer: DeviceId,
        delivery: oink_foundation::content::DeliveryId,
        success: bool,
    ) {
        let accepted = match kind {
            // v2 has no content-derived receipt. The local clipboard registry
            // only needs the delivery identity to release its receiver pin.
            StreamKind::Clipboard => self.shared.native.clipboard_receipt(peer, delivery, None),
            StreamKind::Dnd => {
                let files = self.shared.dnd_files.lock().unwrap().clone();
                if let Some(files) = files {
                    files.receipt(peer, delivery, success).await
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
        if let Ok(message) = oink_clipboard::wire::content(&ContentMsg::TerminalAck { delivery }) {
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

fn sensitive_superseded(
    offer: &dto::ClipboardOffer,
    window: &oink_foundation::clipboard_history::HistoryWindow,
) -> bool {
    if !matches!(
        offer.sensitivity(),
        Ok(dto::Sensitivity::Concealed | dto::Sensitivity::Transient)
    ) {
        return false;
    }
    let Some(head) = window.records().first() else {
        return false;
    };
    let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id) else {
        return true;
    };
    let key = oink_foundation::clipboard_history::OrderKey {
        clock: offer.clock().unwrap_or(0),
        origin: id.clip.origin,
        seq: id.clip.seq,
        generation: id.generation,
    };
    head.key() > key
}
fn is_history(message: &Message) -> bool {
    matches!(
        message,
        Message::HistorySummary(_)
            | Message::HistoryIndexRequest(_)
            | Message::HistoryIndex(_)
            | Message::HistoryPull(_)
            | Message::HistoryRecords(_)
            | Message::HistoryDelta(_)
    )
}

struct ObservedServe {
    channel: LinkBulk,
    clip: ClipHandle,
}
impl ObservedServe {
    fn new(stream: LowStream, clip: ClipHandle) -> Self {
        Self {
            channel: LinkBulk(stream),
            clip,
        }
    }
}
impl BulkChannel for ObservedServe {
    fn diagnostic_id(&self) -> [u8; 16] {
        self.channel.diagnostic_id()
    }
    fn pacing_observer(&self) -> oink_clipboard::sync::files::PacingObserver {
        self.channel.pacing_observer()
    }
    fn transfer_path(&self) -> &'static str {
        self.channel.transfer_path()
    }
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        self.channel.effect_validity()
    }
    fn finish(&mut self) {
        self.channel.finish()
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        self.channel.recv()
    }
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        let missing = match message {
            ContentMsg::RepresentationFailed {
                offer,
                reason: Unavailable::NoSuchItem,
                ..
            } => Some((*offer, false)),
            ContentMsg::CloudLinkUnavailable {
                offer,
                reason: Unavailable::NoSuchItem | Unavailable::Superseded,
            }
            | ContentMsg::DirectoryFailed {
                offer,
                reason: Unavailable::NoSuchItem | Unavailable::Superseded,
                ..
            }
            | ContentMsg::FileFailed {
                offer,
                reason: Unavailable::NoSuchItem | Unavailable::Superseded,
                ..
            } => Some((*offer, true)),
            _ => None,
        };
        if let Some((id, files)) = missing {
            let store = self.clip.shared.history.clone();
            let tx = self.clip.tx.clone();
            let group = store.group();
            tokio::task::spawn_blocking(move || {
                if let Ok(Some(entry)) = store.confirmed_missing(id, files) {
                    let _ = tx.send(Input::HistoryReady(
                        group,
                        vec![oink_foundation::clipboard_history::HistoryChange::Holder(entry)],
                    ));
                }
            });
        }
        self.channel.send(message)
    }
}

#[cfg(test)]
mod sender_tests {
    use super::*;
    use crate::config_module::{MemberInfo, MemberTable};

    fn member(id: u8) -> MemberInfo {
        MemberInfo {
            device: DeviceId([id; 32]),
            is_self: id == 1,
            name: String::new(),
            os: "macos".into(),
            online: true,
            facts: None,
            added_by: None,
            added_at_ms: 0,
            last_connected_ms: None,
            address_stale: false,
            protocol_incompatible: false,
        }
    }
    fn offer(origin: u8, sender: u8, proxy: Option<u8>) -> dto::ClipboardOffer {
        let mut raw = oink_clipboard::sync::Ordering::new(DeviceId([origin; 32]), 1)
            .capture(
                oink_clipboard::wire::inventory(&oink_clipboard::model::Inventory {
                    id: oink_clipboard::model::OfferId {
                        clip: ClipId {
                            origin: DeviceId([origin; 32]),
                            seq: 1,
                        },
                        generation: 1,
                    },
                    platform: oink_foundation::clipboard::ClipPlatform::Unknown,
                    items: vec![oink_clipboard::model::ContentItem {
                        id: oink_clipboard::model::ItemId(0),
                        file: None,
                        virtual_files: false,
                        representations: Vec::new(),
                    }],
                    roots: Vec::new(),
                })
                .unwrap(),
                vec![],
                1,
                oink_foundation::clipboard::Sensitivity::Normal,
                0,
            )
            .unwrap()
            .unwrap()
            .into_raw();
        raw.insert(8, oink_clipboard::wire::device(DeviceId([sender; 32])).write());
        if let Some(proxy) = proxy {
            raw.insert(9, oink_clipboard::wire::device(DeviceId([proxy; 32])).write());
        }
        dto::ClipboardOffer::from_raw(raw).unwrap()
    }

    #[test]
    fn proxied_offer_needs_a_member_as_real_sender() {
        let members = MemberTable {
            members: vec![member(1), member(2), member(3)],
        };
        assert!(offer_parties_are_members(&offer(2, 2, None), &members));
        assert!(offer_parties_are_members(&offer(2, 3, Some(2)), &members));
        assert!(!offer_parties_are_members(&offer(2, 9, Some(2)), &members));
        let without_proxy = MemberTable {
            members: vec![member(1), member(3)],
        };
        assert!(!offer_parties_are_members(&offer(2, 3, Some(2)), &without_proxy));
    }
}
