use super::*;
use crate::model::{ContentMsg, Demand};
use crate::sync::files::BulkChannel;
use oink_foundation::DeviceId;
use oink_foundation::content::DeliveryId;
use std::sync::atomic::{AtomicBool, Ordering};

/// Registry lookup grants a reader the offer's exact provider generation. The
/// registry retains only real clipboard file references in a bounded FIFO; raw
/// representations and Drop sources keep their ordinary owner lifetimes.
#[derive(Default)]
pub struct Registry(
    Mutex<HashMap<OfferId, Weak<Source>>>,
    Mutex<HashMap<(OfferId, DeviceId), ClipboardObligation>>,
    Mutex<HashMap<DeliveryId, ClipboardReceipt>>,
    Arc<Mutex<std::collections::VecDeque<RetainedFile>>>,
);

const RETAINED_FILES: usize = 1024;
struct RetainedFile {
    _reference: Arc<namespace::local::Reference>,
    _source: Arc<Source>,
}

/// Opaque registration context for real on-disk clipboard files. Namespace
/// wrappers forward it; virtual and Drop namespaces decline it. The table owns
/// the only historical path references, while namespace nodes hold weak links.
#[derive(Clone)]
pub struct FileRetention {
    table: Weak<Mutex<std::collections::VecDeque<RetainedFile>>>,
    source: Weak<Source>,
}
impl std::fmt::Debug for FileRetention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileRetention")
    }
}
impl FileRetention {
    pub(crate) fn insert(&self, reference: Arc<namespace::local::Reference>) -> Weak<namespace::local::Reference> {
        let weak = Arc::downgrade(&reference);
        if let (Some(table), Some(source)) = (self.table.upgrade(), self.source.upgrade()) {
            let mut table = table.lock().unwrap();
            if table.len() == RETAINED_FILES {
                table.pop_front();
            }
            table.push_back(RetainedFile {
                _reference: reference,
                _source: source,
            });
        }
        weak
    }
}
type ClipboardReceipt = (Option<[u8; 32]>, tokio::time::Instant);
struct ClipboardObligation {
    _source: Arc<Source>,
    delivery: DeliveryId,
    activity: Arc<Mutex<Activity>>,
}
struct Source {
    provider: Arc<dyn Provider>,
    valid: AtomicBool,
    activity: Mutex<Activity>,
}
struct Activity {
    started: bool,
    last: tokio::time::Instant,
    // High-water marks stop retries of the same data extending the obligation.
    progress: HashMap<(u8, u64), u64>,
}
impl Source {
    fn progress(&self, kind: u8, id: u64, through: u64) {
        let mut activity = self.activity.lock().unwrap();
        let previous = activity.progress.entry((kind, id)).or_default();
        if through > *previous {
            *previous = through;
            activity.last = tokio::time::Instant::now();
        }
    }
}
impl Activity {
    fn receiving() -> Self {
        Self {
            started: true,
            last: tokio::time::Instant::now(),
            progress: HashMap::new(),
        }
    }
    fn mark(&mut self, kind: u8, id: u64, through: u64) {
        let previous = self.progress.entry((kind, id)).or_default();
        if through > *previous {
            *previous = through;
            self.last = tokio::time::Instant::now();
        }
    }
}
pub struct SourceLease(Arc<Source>);
impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentRegistry")
    }
}
impl std::fmt::Debug for SourceLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentSourceLease")
    }
}
impl Clone for SourceLease {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl SourceLease {
    pub fn cancel(&self) {
        self.0.valid.store(false, Ordering::Release);
        self.0.provider.cancel();
    }
    pub fn inventory(&self) -> Inventory {
        self.0.provider.inventory()
    }
}
impl Registry {
    fn begin_clipboard(self: &Arc<Self>, peer: DeviceId, delivery: DeliveryId) -> io::Result<()> {
        if delivery.receiver != peer {
            return Err(unavailable(Unavailable::Permission));
        }
        let source = self
            .0
            .lock()
            .unwrap()
            .get(&delivery.offer)
            .and_then(Weak::upgrade)
            .filter(|source| source.valid.load(Ordering::Acquire))
            .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
        let key = (delivery.offer, peer);
        let mut obligations = self.1.lock().unwrap();
        if obligations
            .get(&key)
            .is_some_and(|current| current.delivery == delivery)
        {
            return Ok(());
        }
        let activity = Arc::new(Mutex::new(Activity::receiving()));
        obligations.insert(
            key,
            ClipboardObligation {
                _source: source,
                delivery,
                activity: activity.clone(),
            },
        );
        drop(obligations);
        let weak = Arc::downgrade(self);
        let activity = Arc::downgrade(&activity);
        tokio::spawn(async move {
            loop {
                let Some(current) = activity.upgrade() else { return };
                let deadline =
                    current.lock().unwrap().last + std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS);
                drop(current);
                tokio::time::sleep_until(deadline).await;
                let Some(registry) = weak.upgrade() else { return };
                let mut obligations = registry.1.lock().unwrap();
                if obligations.get(&key).is_none_or(|current| current.delivery != delivery) {
                    return;
                }
                if tokio::time::Instant::now()
                    >= obligations[&key].activity.lock().unwrap().last
                        + std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS)
                {
                    obligations.remove(&key);
                    return;
                }
            }
        });
        Ok(())
    }
    fn progress(&self, peer: DeviceId, offer: OfferId, source: &Source, kind: u8, id: u64, through: u64) {
        source.progress(kind, id, through);
        if let Some(delivery) = self.1.lock().unwrap().get(&(offer, peer)) {
            delivery.activity.lock().unwrap().mark(kind, id, through);
        }
    }
    /// Authenticated clipboard receipt ends only this receiver's pin. Current
    /// clipboard/history and independent readers may still own the snapshot.
    pub fn clipboard_receipt(&self, peer: DeviceId, delivery: DeliveryId, receipt: Option<[u8; 32]>) -> bool {
        if delivery.receiver != peer {
            return false;
        }
        let now = tokio::time::Instant::now();
        let mut receipts = self.2.lock().unwrap();
        receipts.retain(|_, (_, at)| {
            now.duration_since(*at) < std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS)
        });
        if let Some((previous, _)) = receipts.get(&delivery) {
            return *previous == receipt;
        }
        let mut obligations = self.1.lock().unwrap();
        let key = (delivery.offer, peer);
        if obligations.get(&key).is_none_or(|current| current.delivery != delivery) {
            return false;
        }
        obligations.remove(&key);
        if receipts.len() >= 128
            && let Some(oldest) = receipts.iter().min_by_key(|(_, (_, at))| at).map(|(id, _)| *id)
        {
            receipts.remove(&oldest);
        }
        receipts.insert(delivery, (receipt, now));
        true
    }
    /// Exactly one source watchdog per offer. It owns no provider while waiting,
    /// so a released source snapshot can drain as soon as its readers do.
    pub fn obligation(&self, offer: OfferId) -> Option<impl std::future::Future<Output = bool> + Send + use<>> {
        let source = self.0.lock().unwrap().get(&offer)?.upgrade()?;
        {
            let mut activity = source.activity.lock().unwrap();
            if activity.started {
                return None;
            }
            activity.started = true;
            activity.last = tokio::time::Instant::now();
        }
        let weak = Arc::downgrade(&source);
        Some(async move {
            loop {
                let Some(source) = weak.upgrade().filter(|s| s.valid.load(Ordering::Acquire)) else {
                    return false;
                };
                let deadline = source.activity.lock().unwrap().last
                    + std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS);
                if tokio::time::Instant::now() >= deadline {
                    return true;
                }
                drop(source);
                tokio::time::sleep_until(deadline).await;
            }
        })
    }
    pub fn cancel_all(&self) {
        for source in self.0.lock().unwrap().values().filter_map(Weak::upgrade) {
            source.valid.store(false, Ordering::Release);
            source.provider.cancel();
        }
        self.1.lock().unwrap().clear();
        self.3.lock().unwrap().clear();
    }

    pub fn register(&self, provider: Arc<dyn Provider>) -> io::Result<SourceLease> {
        let inventory = provider.inventory();
        inventory.validate().map_err(unavailable)?;
        let mut sources = self.0.lock().unwrap();
        sources.retain(|_, source| source.strong_count() > 0);
        if sources.get(&inventory.id).and_then(Weak::upgrade).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "content identity already registered",
            ));
        }
        let source = Arc::new(Source {
            provider,
            valid: AtomicBool::new(true),
            activity: Mutex::new(Activity {
                started: false,
                last: tokio::time::Instant::now(),
                progress: HashMap::new(),
            }),
        });
        sources.insert(inventory.id, Arc::downgrade(&source));
        Ok(SourceLease(source))
    }

    /// Register a clipboard offer, transferring real path/identity/metadata
    /// references into the fixed FIFO. No source content is copied. Dropping
    /// the current clipboard owner cannot revoke these paths; explicit policy
    /// cancellation still clears them and cancels admitted readers.
    pub fn register_clipboard(&self, provider: Arc<dyn Provider>) -> io::Result<SourceLease> {
        let lease = self.register(provider)?;
        if let Some(files) = lease.0.provider.namespace() {
            files.retain_files(FileRetention {
                table: Arc::downgrade(&self.3),
                source: Arc::downgrade(&lease.0),
            });
        }
        Ok(lease)
    }

    /// Network endpoints check the authenticated receiver's reconstruction
    /// profile before any native read or delivery obligation can begin.
    pub async fn serve_negotiated(
        self: &Arc<Self>,
        peer: DeviceId,
        request: ContentMsg,
        channel: &mut dyn BulkChannel,
        source_profile: &oink_protocol::v1::dto::FormatSupport,
        target_profile: &oink_protocol::v1::dto::FormatSupport,
    ) -> io::Result<()> {
        self.serve_negotiated_traced(
            peer,
            request,
            channel,
            source_profile,
            target_profile,
            oink_foundation::drag_trace::Context::current(),
        )
        .await
    }

    pub async fn serve_negotiated_traced(
        self: &Arc<Self>,
        peer: DeviceId,
        request: ContentMsg,
        channel: &mut dyn BulkChannel,
        source_profile: &oink_protocol::v1::dto::FormatSupport,
        target_profile: &oink_protocol::v1::dto::FormatSupport,
        diagnostic: oink_foundation::drag_trace::Context,
    ) -> io::Result<()> {
        let Some((offer, _)) = request.request() else {
            return Err(unavailable(Unavailable::InvalidData));
        };
        let source = self.0.lock().unwrap().get(&offer).and_then(Weak::upgrade);
        if let Some(source) = source {
            let inventory = source.provider.inventory();
            match &request {
                ContentMsg::ReadRepresentation {
                    representation,
                    target,
                    item,
                    accepted_format,
                    ..
                } => {
                    if crate::negotiation::negotiated_selection(
                        source_profile,
                        target_profile,
                        &inventory,
                        *representation,
                        *target,
                    )
                    .ok()
                    .as_ref()
                        != Some(&(*item, accepted_format.clone()))
                    {
                        return Err(unavailable(Unavailable::Unsupported));
                    }
                }
                _ if (!inventory.namespace_roots().is_empty()
                    || !matches!(request, ContentMsg::BeginDelivery { .. }))
                    && !crate::negotiation::namespace_supported(source_profile, target_profile) =>
                {
                    return Err(unavailable(Unavailable::Unsupported));
                }
                _ => {}
            }
        }
        self.serve_traced(peer, request, channel, diagnostic).await
    }

    /// Serving runs outside capture/route/native threads. The agent handles the
    /// request's Drop demand before entering here; bytes alone never grant Drop.
    pub async fn serve(
        self: &Arc<Self>,
        peer: DeviceId,
        request: ContentMsg,
        channel: &mut dyn BulkChannel,
    ) -> io::Result<()> {
        self.serve_traced(peer, request, channel, oink_foundation::drag_trace::Context::current())
            .await
    }

    pub async fn serve_traced(
        self: &Arc<Self>,
        peer: DeviceId,
        request: ContentMsg,
        channel: &mut dyn BulkChannel,
        diagnostic: oink_foundation::drag_trace::Context,
    ) -> io::Result<()> {
        if let ContentMsg::BeginDelivery { delivery, demand } = request {
            let offer = delivery.offer;
            if delivery.receiver != peer {
                return Err(unavailable(Unavailable::Permission));
            }
            match demand {
                Demand::Clipboard => {
                    if let Err(error) = self.begin_clipboard(peer, delivery) {
                        return send(
                            channel,
                            ContentMsg::Failed {
                                delivery,
                                reason: reason(&error),
                            },
                        )
                        .await;
                    }
                }
                Demand::Drop(holder) if holder.device == peer && holder.epoch == delivery.sequence => {}
                _ => return Err(unavailable(Unavailable::Permission)),
            }
            let source = self.0.lock().unwrap().get(&offer).and_then(Weak::upgrade);
            if source.is_none_or(|s| !s.valid.load(Ordering::Acquire)) {
                return Err(unavailable(Unavailable::Superseded));
            }
            return send(channel, ContentMsg::DeliveryReady { offer, delivery }).await;
        }
        if !matches!(request, ContentMsg::ReadRepresentation { .. }) {
            return self.serve_namespace(peer, request, channel).await;
        }
        let ContentMsg::ReadRepresentation {
            offer,
            representation,
            item,
            accepted_format,
            target,
            demand,
        } = request
        else {
            return Err(unavailable(Unavailable::InvalidData));
        };
        let source = self.0.lock().unwrap().get(&offer).and_then(Weak::upgrade);
        let Some(source) = source.filter(|s| s.valid.load(Ordering::Acquire)) else {
            return send(
                channel,
                ContentMsg::RepresentationFailed {
                    offer,
                    representation,
                    reason: Unavailable::Superseded,
                },
            )
            .await;
        };
        if crate::negotiation::selection(&source.provider.inventory(), representation, target)
            != Some((item, accepted_format))
        {
            return send(
                channel,
                ContentMsg::RepresentationFailed {
                    offer,
                    representation,
                    reason: Unavailable::Unsupported,
                },
            )
            .await;
        }
        let provider = source.provider.clone();
        let diagnostic = oink_foundation::drag_trace::Context {
            offer: offer.clip.seq,
            representation: representation.0,
            ..diagnostic
        }
        .with_request(channel.diagnostic_id());
        let queued =
            oink_foundation::drag_trace::Trace::new(oink_foundation::drag_trace::stage::SOURCE_OPEN, diagnostic);
        let open_context = queued.context();
        let reader = tokio::task::spawn_blocking(move || {
            let opened = oink_foundation::drag_trace::Trace::new(
                oink_foundation::drag_trace::stage::SOURCE_OPEN,
                oink_foundation::drag_trace::Context { aux: 1, ..open_context },
            );
            let result = opened.scope(|| {
                open_for_demand(
                    provider.as_ref(),
                    representation,
                    target,
                    matches!(demand, Demand::Hover(_)),
                )
            });
            opened.io(&result);
            result
        })
        .await
        .map_err(|_| unavailable(Unavailable::ReadFailed))?;
        queued.io(&reader);
        drop(queued);
        let mut reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                return send(
                    channel,
                    ContentMsg::RepresentationFailed {
                        offer,
                        representation,
                        reason: reason(&error),
                    },
                )
                .await;
            }
        };
        let validity = channel.effect_validity();
        let mut offset = 0;
        let mut hash = blake3::Hasher::new();
        loop {
            if !source.valid.load(Ordering::Acquire) || !validity() {
                return send(
                    channel,
                    ContentMsg::RepresentationFailed {
                        offer,
                        representation,
                        reason: Unavailable::Cancelled,
                    },
                )
                .await;
            }
            let remaining = REPRESENTATION_BYTES - offset;
            let (returned, read) = tokio::task::spawn_blocking(move || {
                let reading = oink_foundation::drag_trace::Trace::new(
                    oink_foundation::drag_trace::stage::SOURCE_READ,
                    diagnostic,
                );
                let mut bytes = vec![0; remaining.saturating_add(1).min(1024 * 1024) as usize];
                let result = reader.read(&mut bytes).map(|n| {
                    bytes.truncate(n);
                    bytes
                });
                reading.io(&result);
                (reader, result)
            })
            .await
            .map_err(|_| unavailable(Unavailable::ReadFailed))?;
            reader = returned;
            // Reading can block in a native provider. A permission change during
            // that call must win before its finished bytes enter the transport.
            if !source.valid.load(Ordering::Acquire) || !validity() {
                return send(
                    channel,
                    ContentMsg::RepresentationFailed {
                        offer,
                        representation,
                        reason: Unavailable::Cancelled,
                    },
                )
                .await;
            }

            let bytes = match read {
                Ok(bytes) if bytes.len() as u64 <= remaining => bytes,
                Ok(_) => {
                    return send(
                        channel,
                        ContentMsg::RepresentationFailed {
                            offer,
                            representation,
                            reason: Unavailable::LimitExceeded,
                        },
                    )
                    .await;
                }
                Err(error) => {
                    return send(
                        channel,
                        ContentMsg::RepresentationFailed {
                            offer,
                            representation,
                            reason: reason(&error),
                        },
                    )
                    .await;
                }
            };
            if bytes.is_empty() {
                return send(
                    channel,
                    ContentMsg::RepresentationEnd {
                        offer,
                        representation,
                        size: offset,
                        hash: *hash.finalize().as_bytes(),
                    },
                )
                .await;
            }
            hash.update(&bytes);
            let length = bytes.len() as u64;
            send(
                channel,
                ContentMsg::RepresentationBytes {
                    offer,
                    representation,
                    offset,
                    bytes,
                },
            )
            .await?;
            offset += length;
            self.progress(peer, offer, &source, 0, representation.0 as u64, offset);
        }
    }
}
async fn send(channel: &mut dyn BulkChannel, message: ContentMsg) -> io::Result<()> {
    let (offer, representation, aux) = match &message {
        ContentMsg::RepresentationBytes {
            offer, representation, ..
        } => (offer.clip.seq, representation.0, 1),
        ContentMsg::RepresentationEnd {
            offer, representation, ..
        } => (offer.clip.seq, representation.0, 2),
        ContentMsg::RepresentationFailed {
            offer,
            representation,
            reason,
        } => (offer.clip.seq, representation.0, 3 | ((*reason as u32) << 8)),
        _ => (0, 0, 0),
    };
    let trace = oink_foundation::drag_trace::Trace::new(
        oink_foundation::drag_trace::stage::REPLY_SEND,
        oink_foundation::drag_trace::Context {
            offer,
            representation,
            aux,
            ..oink_foundation::drag_trace::Context::current()
        }
        .with_request(channel.diagnostic_id()),
    );
    let result = channel.send(&message).await;
    trace.io(&result);
    result
}

impl Registry {
    async fn serve_namespace(
        &self,
        peer: DeviceId,
        request: ContentMsg,
        channel: &mut dyn BulkChannel,
    ) -> io::Result<()> {
        use crate::model::Demand;
        let (offer, _) = request.request().ok_or_else(|| unavailable(Unavailable::InvalidData))?;
        let node = match request {
            ContentMsg::ListDirectory { node, .. }
            | ContentMsg::SelectFile { node, .. }
            | ContentMsg::ReadFile { node, .. } => node,
            _ => return Err(unavailable(Unavailable::InvalidData)),
        };
        let directory = matches!(request, ContentMsg::ListDirectory { .. });
        let failure = |reason| {
            if directory {
                ContentMsg::DirectoryFailed { offer, node, reason }
            } else {
                ContentMsg::FileFailed { offer, node, reason }
            }
        };
        let source = self.0.lock().unwrap().get(&offer).and_then(Weak::upgrade);
        let Some(source) = source else {
            let missing = if matches!(request.request(), Some((_, Demand::Clipboard))) {
                Unavailable::NoSuchItem
            } else {
                Unavailable::Superseded
            };
            return send(channel, failure(missing)).await;
        };
        if !source.valid.load(Ordering::Acquire) {
            return send(channel, failure(Unavailable::Superseded)).await;
        }
        if matches!(request.request(), Some((_, Demand::Hover(_))))
            && source
                .provider
                .inventory()
                .items
                .iter()
                .flat_map(|item| &item.representations)
                .any(|rep| matches!(rep.data, RepresentationData::Promise(id, _) if id == node))
        {
            return send(channel, failure(Unavailable::Permission)).await;
        }
        let Some(files) = source.provider.namespace() else {
            return send(channel, failure(Unavailable::NoSuchItem)).await;
        };
        let validity = channel.effect_validity();
        match request {
            ContentMsg::ListDirectory { cursor, .. } => {
                let mut work = tokio::task::spawn_blocking(move || files.list(node, cursor));
                loop {
                    if !source.valid.load(Ordering::Acquire) || !validity() {
                        return send(channel, failure(Unavailable::Cancelled)).await;
                    }
                    tokio::select! {
                        result = &mut work => {
                            let response = match result.map_err(|_| unavailable(Unavailable::ReadFailed)).and_then(|r| r) {
                                Ok(page) if source.valid.load(Ordering::Acquire) && validity() => {
                                    self.progress(peer, offer, &source, 1, node.0, cursor.map_or(0, |c| c.offset) as u64 + page.entries.len() as u64 + 1);
                                    ContentMsg::Directory { offer, page }
                                },
                                Ok(_) => failure(Unavailable::Cancelled),
                                Err(error) => failure(reason(&error)),
                            };
                            return send(channel, response).await;
                        },
                        // A bounded preparing response is not a completed empty
                        // page and does not reset the target's no-progress timer.
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                            send(channel, ContentMsg::Preparing { offer, node }).await?;
                        }
                    }
                }
            }
            ContentMsg::SelectFile { demand, .. } => {
                if matches!(demand, Demand::Hover(_)) {
                    return send(channel, failure(Unavailable::Permission)).await;
                }
                let latest = matches!(demand, Demand::Clipboard) && files.real_clipboard_files();
                let result = tokio::task::spawn_blocking(move || files.select(node))
                    .await
                    .map_err(|_| unavailable(Unavailable::ReadFailed))
                    .and_then(|r| r);
                let response = match result {
                    Ok(entry) if source.valid.load(Ordering::Acquire) && validity() => {
                        self.progress(peer, offer, &source, 3, node.0, 1);
                        if latest {
                            ContentMsg::FileVersion { offer, entry }
                        } else {
                            ContentMsg::FileSelected { offer, entry }
                        }
                    }
                    Ok(_) => failure(Unavailable::Cancelled),
                    Err(error) => failure(reason(&error)),
                };
                send(channel, response).await
            }
            ContentMsg::ReadFile {
                version,
                offset,
                check,
                demand,
                ..
            } => {
                if matches!(demand, Demand::Hover(_)) {
                    return send(channel, failure(Unavailable::Permission)).await;
                }
                let opened = tokio::task::spawn_blocking(move || {
                    let entry = files.select(node)?;
                    let ContentSize::Known(size) = entry.size else {
                        return Err(unavailable(Unavailable::InvalidData));
                    };
                    if entry.version != version
                        || offset > size
                        || (offset != size && !offset.is_multiple_of(FILE_BLOCK))
                    {
                        return Err(unavailable(Unavailable::Superseded));
                    }
                    if offset > 0 {
                        let start = (offset - 1) / FILE_BLOCK * FILE_BLOCK;
                        let mut last = vec![0; (offset - start) as usize];
                        files.open(node, version, start)?.read_exact(&mut last)?;
                        if check != Some(*blake3::hash(&last).as_bytes()) {
                            return Err(unavailable(Unavailable::Superseded));
                        }
                    } else if check.is_some() {
                        return Err(unavailable(Unavailable::InvalidData));
                    }
                    Ok((files.open(node, version, offset)?, size))
                })
                .await
                .map_err(|_| unavailable(Unavailable::ReadFailed))
                .and_then(|r| r);
                let (mut reader, size) = match opened {
                    Ok(value) => value,
                    Err(error) => return send(channel, failure(reason(&error))).await,
                };
                let mut offset = offset;
                loop {
                    if !source.valid.load(Ordering::Acquire) || !validity() {
                        return send(channel, failure(Unavailable::Cancelled)).await;
                    }
                    let left = size - offset;
                    let (returned, read) = tokio::task::spawn_blocking(move || {
                        let result = if left > 0 {
                            let mut bytes = vec![0; left.min(FILE_BLOCK) as usize];
                            reader.read_exact(&mut bytes).map(|()| bytes)
                        } else {
                            let mut byte = [0];
                            match reader.read(&mut byte) {
                                Ok(0) => Ok(Vec::new()),
                                Ok(_) => Err(unavailable(Unavailable::Superseded)),
                                Err(error) => Err(error),
                            }
                        };
                        (reader, result)
                    })
                    .await
                    .map_err(|_| unavailable(Unavailable::ReadFailed))?;
                    reader = returned;
                    if !source.valid.load(Ordering::Acquire) || !validity() {
                        return send(channel, failure(Unavailable::Cancelled)).await;
                    }

                    match read {
                        Ok(bytes) if bytes.is_empty() => {
                            return send(
                                channel,
                                ContentMsg::FileEnd {
                                    offer,
                                    node,
                                    version,
                                    size,
                                },
                            )
                            .await;
                        }
                        Ok(bytes) => {
                            let length = bytes.len() as u64;
                            send(
                                channel,
                                ContentMsg::FileBlock {
                                    offer,
                                    node,
                                    version,
                                    offset,
                                    hash: *blake3::hash(&bytes).as_bytes(),
                                    bytes,
                                },
                            )
                            .await?;
                            offset += length;
                            self.progress(peer, offer, &source, 2, node.0, offset);
                        }
                        Err(error) => return send(channel, failure(reason(&error))).await,
                    }
                }
            }
            _ => Err(unavailable(Unavailable::InvalidData)),
        }
    }
}
const FILE_BLOCK: u64 = 1024 * 1024;

#[cfg(all(test, debug_assertions))]
mod activity_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn clipboard_delivery_pins_evicted_offer_until_its_authenticated_receipt() {
        let registry = Arc::new(Registry::default());
        let provider = Arc::new(super::super::tests::fixture());
        let offer = provider.inventory().id;
        let lease = registry.register(provider).unwrap();
        let peer = DeviceId([2; 32]);
        let delivery = DeliveryId {
            offer,
            receiver: peer,
            sequence: 9,
        };
        registry.begin_clipboard(peer, delivery).unwrap();
        drop(lease); // Three later copies can evict the original history owner.
        assert!(registry.0.lock().unwrap()[&offer].upgrade().is_some());
        assert!(!registry.clipboard_receipt(DeviceId([3; 32]), delivery, Some([1; 32])));
        assert!(!registry.clipboard_receipt(
            peer,
            DeliveryId {
                sequence: 8,
                ..delivery
            },
            Some([1; 32])
        ));
        assert!(registry.clipboard_receipt(peer, delivery, Some([1; 32])));
        assert!(registry.0.lock().unwrap()[&offer].upgrade().is_none());
        assert!(registry.clipboard_receipt(peer, delivery, Some([1; 32])));
        assert!(!registry.clipboard_receipt(peer, delivery, Some([2; 32])));
    }

    #[tokio::test(start_paused = true)]
    async fn clipboard_receiver_progress_does_not_extend_another_receivers_obligation() {
        let registry = Arc::new(Registry::default());
        let provider = Arc::new(super::super::tests::fixture());
        let offer = provider.inventory().id;
        let lease = registry.register(provider).unwrap();
        let first = DeviceId([2; 32]);
        let second = DeviceId([3; 32]);
        for peer in [first, second] {
            registry
                .begin_clipboard(
                    peer,
                    DeliveryId {
                        offer,
                        receiver: peer,
                        sequence: 1,
                    },
                )
                .unwrap();
        }
        let period = std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS);
        tokio::task::yield_now().await;
        tokio::time::advance(period / 2).await;
        registry.progress(first, offer, &lease.0, 2, 1, 1024);
        tokio::time::advance(period / 2).await;
        tokio::task::yield_now().await;
        assert!(registry.1.lock().unwrap().contains_key(&(offer, first)));
        assert!(!registry.1.lock().unwrap().contains_key(&(offer, second)));
        registry.progress(first, offer, &lease.0, 2, 1, 1024); // repeated prefix is not new progress
        tokio::time::advance(period / 2).await;
        tokio::task::yield_now().await;
        assert!(registry.1.lock().unwrap().is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn obligation_deadline_tracks_new_bytes_and_not_repeated_prefixes() {
        let registry = Registry::default();
        let provider = Arc::new(super::super::tests::fixture());
        let offer = provider.inventory().id;
        let lease = registry.register(provider).unwrap();
        let timeout = registry.obligation(offer).unwrap();
        assert!(registry.obligation(offer).is_none());
        let task = tokio::spawn(timeout);
        let period = std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS);
        tokio::time::advance(period / 2).await;
        lease.0.progress(2, 1, 1024);
        tokio::time::advance(period / 2).await;
        lease.0.progress(2, 1, 1024);
        assert!(!task.is_finished());
        tokio::time::advance(period / 2).await;
        assert!(task.await.unwrap());
    }
}

#[cfg(all(test, debug_assertions))]
mod retained_tests {
    use super::*;
    use crate::content::namespace::{Local, Namespace};
    use crate::model::NodeId;
    use std::fs;

    fn source(registry: &Registry, path: &std::path::Path, sequence: u64) -> (SourceLease, Arc<Local>) {
        let files = Local::clipboard(vec![(NodeId(1), path.to_owned())]).unwrap();
        let mut id = super::super::tests::fixture().inventory().id;
        id.clip.seq = sequence;
        let provider = identified(with_files(Arc::new(super::super::tests::fixture()), files.clone()), id);
        (registry.register_clipboard(provider).unwrap(), files)
    }

    #[test]
    fn fifo_holds_exactly_1024_file_records_and_drops_oldest_even_with_an_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"original").unwrap();
        let registry = Registry::default();
        let (old_owner, oldest) = source(&registry, &path, 1);
        for seq in 2..=1024 {
            drop(source(&registry, &path, seq));
        }
        assert_eq!(registry.3.lock().unwrap().len(), 1024);
        assert!(oldest.select(NodeId(1)).is_ok());
        drop(source(&registry, &path, 1025));
        assert_eq!(registry.3.lock().unwrap().len(), 1024);
        assert_eq!(reason(&oldest.select(NodeId(1)).unwrap_err()), Unavailable::NoSuchItem);
        drop(old_owner);
        assert!(
            registry.0.lock().unwrap()[&super::super::tests::fixture().inventory().id]
                .upgrade()
                .is_none()
        );
    }

    #[test]
    fn evicting_parent_metadata_does_not_revoke_a_retained_child_identity() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let path = folder.join("file");
        fs::write(&path, b"original").unwrap();
        let registry = Registry::default();
        let (_, files) = source(&registry, &folder, 1);
        let child = files.list(NodeId(1), None).unwrap().entries[0].id;
        for seq in 2..=1024 {
            drop(source(&registry, &path, seq));
        }
        assert_eq!(registry.3.lock().unwrap().len(), 1024);
        assert_eq!(
            reason(&files.list(NodeId(1), None).unwrap_err()),
            Unavailable::NoSuchItem
        );
        let current = files.select(child).unwrap();
        assert_eq!(
            read_all(&mut files.open(child, current.version, 0).unwrap()).unwrap(),
            b"original"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retained_references_have_no_age_or_delivery_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"original").unwrap();
        let registry = Registry::default();
        let (owner, files) = source(&registry, &path, 1);
        drop(owner);
        tokio::time::advance(std::time::Duration::from_secs(30 * 24 * 60 * 60)).await;
        assert!(files.select(NodeId(1)).is_ok());
        assert_eq!(registry.3.lock().unwrap().len(), 1);
    }

    #[test]
    fn real_file_is_served_after_clipboard_change_without_copying_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"original").unwrap();
        let registry = Registry::default();
        let (lease, _) = source(&registry, &path, 1);
        let id = lease.inventory().id;
        drop(lease);
        let source = registry.0.lock().unwrap()[&id].upgrade().unwrap();
        let files = source.provider.namespace().unwrap();
        let current = files.select(NodeId(1)).unwrap();
        let mut bytes = Vec::new();
        files
            .open(NodeId(1), current.version, 0)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        registry.cancel_all();
        assert!(files.select(NodeId(1)).is_err());
        assert!(registry.3.lock().unwrap().is_empty());
    }
}
