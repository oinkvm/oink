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
    Option<grants::SourceGrants>,
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
    grants: Option<grants::SourceGrants>,
    offer: OfferId,
    provider: Arc<dyn Provider>,
    valid: AtomicBool,
    activity: Mutex<Activity>,
    clipboard_source: std::sync::OnceLock<Arc<dyn namespace::Namespace>>,
}
struct Activity {
    started: bool,
    last: tokio::time::Instant,
    // High-water marks stop retries of the same data extending the obligation.
    progress: HashMap<(u8, u64), u64>,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.discard_pending();
    }
}
impl Source {
    fn discard_pending(&self) {
        if let Some(grants) = &self.grants
            && grants.discard_pending(self.offer).is_err()
        {
            tracing::warn!("provisional source grant removal could not be persisted");
        }
    }
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
    #[cfg(debug_assertions)]
    pub fn watchdog_snapshot_for_test(&self) -> String {
        format!(
            "valid={} source_refs={} activity={:?}",
            self.0.valid.load(Ordering::Acquire),
            Arc::strong_count(&self.0),
            self.0.activity.try_lock().map(|a| (a.started, a.progress.len()))
        )
    }

    pub fn cancel(&self) {
        self.0.valid.store(false, Ordering::Release);
        self.0.discard_pending();
        self.0.provider.cancel();
    }
    pub fn inventory(&self) -> Inventory {
        self.0.provider.inventory()
    }
}
impl Registry {
    #[cfg(debug_assertions)]
    pub fn watchdog_snapshot_for_test(&self) -> String {
        // Never upgrade a weak source here. Dropping its last strong reference
        // would run source retirement, including disk I/O, on the watchdog.
        let sources = self.0.try_lock().map(|sources| {
            sources
                .iter()
                .map(|(offer, source)| (*offer, source.strong_count()))
                .collect::<Vec<_>>()
        });
        format!(
            "source_registry leases={sources:?} obligations={:?} receipts={:?}\n{}",
            self.1.try_lock().map(|v| v.len()),
            self.2.try_lock().map(|v| v.len()),
            self.4
                .as_ref()
                .map_or_else(|| "grants=none".into(), |grants| grants.watchdog_snapshot_for_test())
        )
    }

    /// Clipboard and drag registries share one durable, roster-bound authority.
    pub fn with_source_grants(grants: grants::SourceGrants) -> Self {
        Self(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Some(grants),
        )
    }

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
            source.discard_pending();
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
            grants: self.4.clone(),
            offer: inventory.id,
            provider,
            valid: AtomicBool::new(true),
            clipboard_source: std::sync::OnceLock::new(),
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
        let files = provider.namespace();
        let key = files.as_ref().map(|files| files.clipboard_key()).transpose()?.flatten();
        self.register_clipboard_identified(provider, key, files)
    }
    /// The capture worker supplies native metadata before sharing-policy
    /// filtering. The retained source is only an identity probe, never a route
    /// around the provider's policy checks for content reads.
    pub fn register_clipboard_identified(
        &self,
        provider: Arc<dyn Provider>,
        key: Option<[u8; 32]>,
        identity_source: Option<Arc<dyn namespace::Namespace>>,
    ) -> io::Result<SourceLease> {
        let lease = self.register(provider)?;
        if let Some(files) = identity_source {
            let _ = lease.0.clipboard_source.set(files);
        }
        if let Some(grants) = &self.4 {
            grants.observe_clipboard(Some((lease.0.offer, key)))?;
        }
        if let Some(files) = lease.0.provider.namespace() {
            files.retain_files(FileRetention {
                table: Arc::downgrade(&self.3),
                source: Arc::downgrade(&lease.0),
            });
        }
        Ok(lease)
    }

    pub fn check_clipboard(&self, backend: &dyn crate::ClipboardBackend) -> io::Result<()> {
        if let Some(grants) = &self.4 {
            // Do not acquire registry -> grants in the opposite order from
            // cancel_all/discard_pending while probing the durable state.
            let sources = self.0.lock().unwrap().clone();
            grants.check_clipboard_with(|offer| {
                let source = sources.get(&offer).and_then(Weak::upgrade);
                if let Some(files) = source.and_then(|source| source.clipboard_source.get().cloned()) {
                    match files.clipboard_key() {
                        Ok(Some(key)) => return Ok((key, true)),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Err(error),
                        _ => {}
                    }
                }
                backend.clipboard_key().map(|key| (key, false))
            })?;
        }
        Ok(())
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

    /// Legacy peers may still request ordinary representations, but cannot use
    /// old namespace operations or promise controls to bypass the cloud contract.
    pub fn non_file_request(&self, request: &ContentMsg) -> bool {
        let ContentMsg::ReadRepresentation {
            offer, representation, ..
        } = request
        else {
            return false;
        };
        self.0
            .lock()
            .unwrap()
            .get(offer)
            .and_then(Weak::upgrade)
            .is_some_and(|source| {
                super::find(&source.provider.inventory(), *representation).is_some_and(|rep| {
                    !matches!(rep.data, RepresentationData::File(_) | RepresentationData::Promise(..))
                })
            })
    }
    /// Local source policy, selected using the authenticated requesting device.
    pub fn set_peer_file_filter(&self, peer: DeviceId, omit_ds_store: bool) {
        if let Some(grants) = &self.4 {
            grants.set_peer_file_filter(peer, omit_ds_store);
        }
    }
    pub fn set_peer_safe_symlinks(&self, peer: DeviceId, enabled: bool) {
        if let Some(grants) = &self.4 {
            grants.set_peer_safe_symlinks(peer, enabled);
        }
    }
    pub fn set_peer_exec_bits(&self, peer: DeviceId, enabled: bool) {
        if let Some(grants) = &self.4 {
            grants.set_peer_exec_bits(peer, enabled);
        }
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
        if request.persistent_request() {
            if !crate::negotiation::namespace_supported(source_profile, target_profile) {
                return Err(unavailable(Unavailable::Unsupported));
            }
            return self.serve_traced(peer, request, channel, diagnostic).await;
        }
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
        if let ContentMsg::CloudLinkRequest { offer, roots, demand } = request {
            let source = self.0.lock().unwrap().get(&offer).and_then(Weak::upgrade);
            let grants = self.4.clone();
            let validity = channel.effect_validity();
            let result = tokio::task::spawn_blocking(move || {
                if matches!(demand, Demand::Hover(holder) | Demand::Drop(holder) if holder.device != peer) {
                    return Err(unavailable(Unavailable::Permission));
                }
                let source = source
                    .filter(|source| source.valid.load(Ordering::Acquire))
                    .ok_or_else(|| unavailable(Unavailable::Superseded))?;
                let grants = grants.ok_or_else(|| unavailable(Unavailable::Unsupported))?;
                if !validity() {
                    return Err(unavailable(Unavailable::Cancelled));
                }
                let files = source
                    .provider
                    .namespace()
                    .ok_or_else(|| unavailable(Unavailable::NoSuchItem))?;
                let token = grants.issue_for(peer, Some(offer), files.as_ref(), &roots, demand)?;
                if !source.valid.load(Ordering::Acquire) || !validity() {
                    if !source.valid.load(Ordering::Acquire) {
                        source.discard_pending();
                    } else {
                        // This preparation left its host; the native source is
                        // still live and may cross to another holder next.
                        grants.abandon(peer, token)?;
                    }
                    return Err(unavailable(Unavailable::Cancelled));
                }
                let files = grants.open(peer, token)?;
                let roots = files.roots();
                let native = files.native_file_metadata();
                let fulfilled = grants.fulfilled_roots(peer, token)?;
                Ok((token, roots, fulfilled, native))
            })
            .await
            .map_err(|_| unavailable(Unavailable::ReadFailed))
            .and_then(|r| r);
            let token = result.as_ref().ok().map(|(token, _, _, _)| *token);
            let symlinks = result
                .as_ref()
                .ok()
                .map(|(_, roots, _, _)| {
                    roots
                        .iter()
                        .filter(|root| root.kind.symlink_target().is_some())
                        .map(|root| root.id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let response = match result {
                Ok((token, roots, fulfilled, native)) => ContentMsg::CloudLinkOffer {
                    native,
                    offer,
                    token,
                    roots,
                    fulfilled,
                },
                Err(error) => {
                    let reason = reason(&error);
                    tracing::warn!(target: "oink_file_diagnostics", file_stage = 42_u8,
                            file_offer = offer.clip.seq, file_generation = offer.generation,
                            file_reason = ?reason, "source file namespace rejected");
                    ContentMsg::CloudLinkUnavailable { offer, reason }
                }
            };
            let result = send(channel, response).await;
            if let (Some(grants), Some(token)) = (&self.4, token) {
                if result.is_err() {
                    let _ = grants.abandon(peer, token);
                } else if !matches!(demand, Demand::Hover(_)) {
                    for node in symlinks {
                        grants.sent(peer, token, node)?;
                    }
                }
            }
            return result;
        }
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
        let reader = representation_work(channel, move || {
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
        .await?;
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
            let (returned, read) = representation_work(channel, move || {
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
            .await?;
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
                    },
                )
                .await;
            }
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
// Representation consumers send no further frames until completion. Observe
// their stream closing while a native source call runs, so a timed-out paste
// releases its serving operation immediately. An already executing OS call
// cannot be forcibly stopped; its eventual result is discarded with the task.
// File Provider / Cloud Files operations do not use this callback bound.
async fn representation_work<T: Send + 'static>(
    channel: &mut dyn BulkChannel,
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    let mut work = tokio::task::spawn_blocking(work);
    tokio::select! {
        result = &mut work => result.map_err(|_| unavailable(Unavailable::ReadFailed)),
        closed = channel.recv() => {
            work.abort(); // Also prevents a queued native call from starting.
            match closed {
                Ok(Some(_)) => Err(unavailable(Unavailable::InvalidData)),
                Ok(None) => Err(unavailable(Unavailable::Cancelled)),
                Err(error) => Err(error),
            }
        },
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

struct OneShotSend {
    grants: Option<super::grants::SourceGrants>,
    peer: DeviceId,
    token: Option<super::grants::LinkToken>,
    successful: bool,
}
impl OneShotSend {
    fn finish(&mut self, node: Option<crate::model::NodeId>) -> io::Result<()> {
        self.successful = true;
        if let (Some(grants), Some(token), Some(node)) = (&self.grants, self.token, node) {
            grants.sent(self.peer, token, node)?;
        }
        Ok(())
    }
}
impl Drop for OneShotSend {
    fn drop(&mut self) {
        if !self.successful
            && let (Some(grants), Some(token)) = (&self.grants, self.token)
        {
            let _ = grants.abandon(self.peer, token);
        }
    }
}
impl Registry {
    async fn serve_namespace(
        &self,
        peer: DeviceId,
        request: ContentMsg,
        channel: &mut dyn BulkChannel,
    ) -> io::Result<()> {
        use crate::model::Demand;
        let persistent = match &request {
            ContentMsg::CloudLinkList { token, .. }
            | ContentMsg::CloudLinkSelect { token, .. }
            | ContentMsg::CloudLinkRead { token, .. } => Some(*token),
            _ => None,
        };
        // A channel is the temporary read context. It never survives a current
        // link's revocation; a later channel must authenticate and open again.
        let request = match request {
            ContentMsg::CloudLinkList {
                offer, node, cursor, ..
            } => ContentMsg::ListDirectory {
                offer,
                node,
                cursor,
                demand: Demand::Clipboard,
            },
            ContentMsg::CloudLinkSelect { offer, node, .. } => ContentMsg::SelectFile {
                offer,
                node,
                demand: Demand::Clipboard,
            },
            ContentMsg::CloudLinkRead {
                offer,
                node,
                version,
                offset,
                ..
            } => ContentMsg::ReadFile {
                offer,
                node,
                version,
                offset,
                demand: Demand::Clipboard,
            },
            request => request,
        };
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
        let (source, files) = if let Some(token) = persistent {
            let result = self
                .4
                .as_ref()
                .ok_or_else(|| unavailable(Unavailable::Unsupported))
                .and_then(|grants| grants.open(peer, token));
            match result {
                Ok(files) => (None, files),
                Err(error) => return send(channel, failure(reason(&error))).await,
            }
        } else {
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
            (Some(source), files)
        };
        let active = || {
            source
                .as_ref()
                .is_none_or(|source| source.valid.load(Ordering::Acquire))
        };
        let progress = |kind, id, through| {
            if let Some(source) = &source {
                self.progress(peer, offer, source, kind, id, through);
            }
        };
        let validity = channel.effect_validity();
        let mut one_shot = OneShotSend {
            grants: self.4.clone(),
            peer,
            token: persistent,
            successful: false,
        };
        match request {
            ContentMsg::ListDirectory { cursor, .. } => {
                let grants = persistent.is_none().then(|| self.4.clone()).flatten();
                let strip_exec = persistent.is_none() && self.4.is_none();
                let mut work = tokio::task::spawn_blocking(move || match grants {
                    Some(grants) => grants.list_for_peer(peer, files.as_ref(), node, cursor),
                    None => files.list(node, cursor).map(|mut page| {
                        // Without a grant store there is no per-peer execution
                        // authority. Keep the ordinary transient file behavior.
                        if strip_exec {
                            for entry in &mut page.entries {
                                entry.exec_bits = None;
                            }
                        }
                        page
                    }),
                });
                loop {
                    if !active() || !validity() {
                        return send(channel, failure(Unavailable::Cancelled)).await;
                    }
                    tokio::select! {
                        result = &mut work => {
                            let response = match result.map_err(|_| unavailable(Unavailable::ReadFailed)).and_then(|r| r) {
                                Ok(page) if active() && validity() => {
                                    progress(1, node.0, cursor.map_or(0, |c| c.offset) as u64 + page.entries.len() as u64 + 1);
                                    ContentMsg::Directory { offer, page }
                                },
                                Ok(_) => failure(Unavailable::Cancelled),
                                Err(error) => failure(reason(&error)),
                            };
                            let completed = match &response {
                                ContentMsg::Directory { page, .. } if page.next.is_none() => Some(node),
                                _ => None,
                            };
                            let successful = matches!(response, ContentMsg::Directory { .. });
                            send(channel, response).await?;
                            if successful { one_shot.finish(completed)?; }
                            return Ok(());
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
                    Ok(mut entry) if active() && validity() => {
                        if let Some(grants) = &self.4 {
                            entry = grants.project_for_peer(peer, entry);
                        } else {
                            entry.exec_bits = None;
                        }
                        progress(3, node.0, 1);
                        if latest {
                            ContentMsg::FileVersion { offer, entry }
                        } else {
                            ContentMsg::FileSelected { offer, entry }
                        }
                    }
                    Ok(_) => failure(Unavailable::Cancelled),
                    Err(error) => failure(reason(&error)),
                };
                let successful = matches!(
                    response,
                    ContentMsg::FileSelected { .. } | ContentMsg::FileVersion { .. }
                );
                send(channel, response).await?;
                if successful {
                    one_shot.finish(None)?;
                }
                Ok(())
            }
            ContentMsg::ReadFile {
                version,
                offset,
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
                    // User decision (2026-10-29): TLS/QUIC AEAD covers byte integrity;
                    // the metadata version fingerprint binds resumed reads to this file.
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
                    if !active() || !validity() {
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
                    if !active() || !validity() {
                        return send(channel, failure(Unavailable::Cancelled)).await;
                    }

                    match read {
                        Ok(bytes) if bytes.is_empty() => {
                            // Close the source handle before deleting its cache
                            // (Windows does not permit unlinking an open file).
                            drop(reader);
                            send(
                                channel,
                                ContentMsg::FileEnd {
                                    offer,
                                    node,
                                    version,
                                    size,
                                },
                            )
                            .await?;
                            one_shot.finish(Some(node))?;
                            return Ok(());
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
                                    bytes,
                                },
                            )
                            .await?;
                            offset += length;
                            progress(2, node.0, offset);
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

#[cfg(all(test, debug_assertions))]
mod persistent_tests {
    use super::*;
    use crate::model::NodeId;
    use crate::sync::files::BoxFuture;
    const PEER: DeviceId = DeviceId([2; 32]);
    const MEMBERS: [(DeviceId, [u8; 32]); 1] = [(PEER, [9; 32])];
    #[derive(Default)]
    struct Replies {
        messages: Vec<ContentMsg>,
        revoke_after_block: Option<grants::SourceGrants>,
        fail_end: bool,
        validity: Option<Arc<AtomicBool>>,
    }
    impl BulkChannel for Replies {
        fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
            let validity = self.validity.clone();
            Arc::new(move || {
                validity
                    .as_ref()
                    .is_none_or(|validity| validity.load(Ordering::Acquire))
            })
        }
        fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                self.messages.push(message.clone());
                if self.fail_end && matches!(message, ContentMsg::FileEnd { .. }) {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                if matches!(message, ContentMsg::FileBlock { .. })
                    && let Some(grants) = self.revoke_after_block.take()
                {
                    grants.reconcile_members([])?;
                }
                Ok(())
            })
        }
        fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
            Box::pin(async { Ok(None) })
        }
        fn finish(&mut self) {}
    }
    #[tokio::test]
    async fn revoked_preparation_reply_preserves_the_live_one_shot_source_for_next_holder() {
        struct OneShot {
            files: Arc<namespace::Local>,
            request: Arc<AtomicBool>,
            released: AtomicBool,
        }
        impl namespace::Namespace for OneShot {
            fn one_shot_roots(&self) -> Vec<NodeId> {
                vec![NodeId(1)]
            }
            fn roots(&self) -> Vec<crate::model::FileNode> {
                self.files.roots()
            }
            fn list(
                &self,
                id: NodeId,
                cursor: Option<crate::model::DirectoryCursor>,
            ) -> io::Result<crate::model::DirectoryPage> {
                self.files.list(id, cursor)
            }
            fn select(&self, id: NodeId) -> io::Result<crate::model::FileNode> {
                self.files.select(id)
            }
            fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
                self.files.open(id, version, offset)
            }
            fn release_fulfilled(&self, roots: &[NodeId]) {
                self.files.release_fulfilled(roots);
                if !self.released.swap(true, Ordering::AcqRel) {
                    // Revoke exactly after adoption, before the preparation
                    // worker can publish its reply. The source remains live.
                    self.request.store(false, Ordering::Release);
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"image").unwrap();
        let grants = grants::SourceGrants::load(dir.path(), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let valid = Arc::new(AtomicBool::new(true));
        let files = Arc::new(OneShot {
            files: namespace::Local::new(vec![(NodeId(1), path)]).unwrap(),
            request: valid.clone(),
            released: AtomicBool::new(false),
        });
        let provider = with_files(Arc::new(super::super::tests::fixture()), files);
        let offer = provider.inventory().id;
        let lease = registry.register(provider).unwrap();
        let mut replies = Replies {
            validity: Some(valid.clone()),
            ..Default::default()
        };
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkRequest {
                    offer,
                    roots: vec![NodeId(1)],
                    demand: Demand::Hover(oink_foundation::content::Holder { device: PEER, epoch: 1 }),
                },
                &mut replies,
            )
            .await
            .unwrap();
        assert!(matches!(
            replies.messages.pop(),
            Some(ContentMsg::CloudLinkUnavailable {
                reason: Unavailable::Cancelled,
                ..
            })
        ));
        valid.store(true, Ordering::Release);
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkRequest {
                    offer,
                    roots: vec![NodeId(1)],
                    demand: Demand::Hover(oink_foundation::content::Holder { device: PEER, epoch: 2 }),
                },
                &mut replies,
            )
            .await
            .unwrap();
        assert!(matches!(
            replies.messages.pop(),
            Some(ContentMsg::CloudLinkOffer { .. })
        ));
        lease.cancel();
    }
    #[tokio::test]
    async fn one_shot_transport_completion_and_failure_release_without_receiver_ack() {
        struct OneShot(Arc<namespace::Local>);
        impl namespace::Namespace for OneShot {
            fn one_shot_roots(&self) -> Vec<NodeId> {
                vec![NodeId(1)]
            }
            fn roots(&self) -> Vec<crate::model::FileNode> {
                self.0.roots()
            }
            fn list(
                &self,
                id: NodeId,
                cursor: Option<crate::model::DirectoryCursor>,
            ) -> io::Result<crate::model::DirectoryPage> {
                self.0.list(id, cursor)
            }
            fn select(&self, id: NodeId) -> io::Result<crate::model::FileNode> {
                self.0.select(id)
            }
            fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
                self.0.open(id, version, offset)
            }
        }
        for fail_end in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source");
            std::fs::write(&path, b"body").unwrap();
            let links = dir.path().join("links");
            let grants = grants::SourceGrants::load(&links, MEMBERS).unwrap();
            let token = grants
                .issue(
                    PEER,
                    &OneShot(namespace::Local::new(vec![(NodeId(1), path.clone())]).unwrap()),
                    &[NodeId(1)],
                )
                .unwrap();
            let entry = grants.open(PEER, token).unwrap().select(NodeId(1)).unwrap();
            let registry = Arc::new(Registry::with_source_grants(grants.clone()));
            let offer = super::super::tests::fixture().inventory().id;
            let mut replies = Replies {
                fail_end,
                ..Default::default()
            };
            let result = registry
                .serve(
                    PEER,
                    ContentMsg::CloudLinkRead {
                        offer,
                        token,
                        node: NodeId(1),
                        version: entry.version,
                        offset: 0,
                    },
                    &mut replies,
                )
                .await;
            assert_eq!(result.is_err(), fail_end);
            assert!(
                replies
                    .messages
                    .iter()
                    .any(|message| matches!(message, ContentMsg::FileEnd { .. }))
            );
            assert!(
                std::fs::read_dir(links.join("fulfilled-sources-v1"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert!(grants.open(PEER, token).unwrap().select(NodeId(1)).is_err());
            assert_eq!(std::fs::read(path).unwrap(), b"body");
        }
    }
    async fn issue(registry: &Arc<Registry>, path: &Path) -> (OfferId, grants::LinkToken) {
        let files = namespace::Local::new(vec![(NodeId(1), path.to_owned())]).unwrap();
        let provider = with_files(Arc::new(super::super::tests::fixture()), files);
        let offer = provider.inventory().id;
        let lease = registry.register(provider).unwrap();
        let mut replies = Replies::default();
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkRequest {
                    offer,
                    roots: vec![NodeId(1)],
                    demand: Demand::Clipboard,
                },
                &mut replies,
            )
            .await
            .unwrap();
        let ContentMsg::CloudLinkOffer { token, .. } = replies.messages.pop().unwrap() else {
            panic!("grant not issued")
        };
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkRequest {
                    offer,
                    roots: vec![NodeId(1)],
                    demand: Demand::Clipboard,
                },
                &mut replies,
            )
            .await
            .unwrap();
        assert!(
            matches!(replies.messages.pop(), Some(ContentMsg::CloudLinkOffer { token: repeated, .. }) if repeated == token)
        );
        // The original native source may disappear immediately after issuance.
        drop(lease);
        assert!(registry.0.lock().unwrap()[&offer].upgrade().is_none());
        (offer, token)
    }

    #[tokio::test]
    async fn resumed_reads_rely_on_metadata_version_for_both_request_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let mut bytes = vec![1; FILE_BLOCK as usize];
        bytes.extend_from_slice(b"tail");
        std::fs::write(&path, bytes).unwrap();
        let grants = grants::SourceGrants::load(&dir.path().join("links"), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let (offer, token) = issue(&registry, &path).await;
        // Keep a live offer too, so both request types can resume the selected
        // metadata version without rereading or hashing the previous block.
        let _legacy = registry
            .register(with_files(
                Arc::new(super::super::tests::fixture()),
                namespace::Local::new(vec![(NodeId(1), path.clone())]).unwrap(),
            ))
            .unwrap();
        let mut replies = Replies::default();
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkSelect {
                    offer,
                    token,
                    node: NodeId(1),
                },
                &mut replies,
            )
            .await
            .unwrap();
        let ContentMsg::FileSelected { entry, .. } = replies.messages.pop().unwrap() else {
            panic!("selection")
        };
        let version = entry.version;
        for (legacy, version, allowed) in [
            (false, version, true),
            (true, version, true),
            (false, version + 1, false),
        ] {
            let request = if legacy {
                ContentMsg::ReadFile {
                    offer,
                    node: NodeId(1),
                    version,
                    offset: FILE_BLOCK,
                    demand: Demand::Clipboard,
                }
            } else {
                ContentMsg::CloudLinkRead {
                    offer,
                    token,
                    node: NodeId(1),
                    version,
                    offset: FILE_BLOCK,
                }
            };
            replies.messages.clear();
            registry.serve(PEER, request, &mut replies).await.unwrap();
            if allowed {
                assert!(matches!(&replies.messages[0], ContentMsg::FileBlock { bytes, .. } if bytes == b"tail"));
                assert!(matches!(replies.messages[1], ContentMsg::FileEnd { .. }));
            } else {
                assert!(matches!(
                    replies.messages.as_slice(),
                    [ContentMsg::FileFailed {
                        reason: Unavailable::Superseded,
                        ..
                    }]
                ));
            }
        }
    }
    #[tokio::test]
    async fn persistent_reads_survive_source_registry_restart_and_revalidate_device() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("original");
        std::fs::write(&file, b"persistent payload").unwrap();
        let grants = grants::SourceGrants::load(directory.path(), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let (offer, token) = issue(&registry, &file).await;
        drop(registry);
        std::fs::rename(&file, directory.path().join("renamed")).unwrap();
        let grants = grants::SourceGrants::load(directory.path(), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let mut replies = Replies::default();
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkSelect {
                    offer,
                    token,
                    node: NodeId(1),
                },
                &mut replies,
            )
            .await
            .unwrap();
        let ContentMsg::FileSelected { entry, .. } = replies.messages.pop().unwrap() else {
            panic!("not selected")
        };
        let request = ContentMsg::CloudLinkRead {
            offer,
            token,
            node: NodeId(1),
            version: entry.version,
            offset: 0,
        };
        registry
            .serve(DeviceId([3; 32]), request.clone(), &mut replies)
            .await
            .unwrap();
        assert!(matches!(
            replies.messages.pop(),
            Some(ContentMsg::FileFailed {
                reason: Unavailable::Permission,
                ..
            })
        ));
        registry.serve(PEER, request, &mut replies).await.unwrap();
        assert!(matches!(&replies.messages[0], ContentMsg::FileBlock { bytes, .. } if bytes == b"persistent payload"));
        assert!(matches!(replies.messages[1], ContentMsg::FileEnd { size: 18, .. }));
    }
    #[tokio::test]
    async fn persistent_stream_stops_after_roster_revocation_and_never_reports_complete() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("large");
        std::fs::write(&file, vec![42; 2 * FILE_BLOCK as usize]).unwrap();
        let grants = grants::SourceGrants::load(directory.path(), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants.clone()));
        let (offer, token) = issue(&registry, &file).await;
        let entry = grants.open(PEER, token).unwrap().select(NodeId(1)).unwrap();
        let mut replies = Replies {
            revoke_after_block: Some(grants),
            ..Default::default()
        };
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkRead {
                    offer,
                    token,
                    node: entry.id,
                    version: entry.version,
                    offset: 0,
                },
                &mut replies,
            )
            .await
            .unwrap();
        assert_eq!(replies.messages.len(), 2);
        assert!(matches!(replies.messages[0], ContentMsg::FileBlock { .. }));
        assert!(matches!(
            replies.messages[1],
            ContentMsg::FileFailed {
                reason: Unavailable::Permission,
                ..
            }
        ));
    }
    #[tokio::test]
    async fn provisional_grants_are_revoked_by_cancel_end_and_registry_shutdown() {
        for action in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("source");
            std::fs::write(&file, b"payload").unwrap();
            let grants = grants::SourceGrants::load(directory.path(), MEMBERS).unwrap();
            let registry = Arc::new(Registry::with_source_grants(grants.clone()));
            let files = namespace::Local::new(vec![(NodeId(1), file)]).unwrap();
            let provider = with_files(Arc::new(super::super::tests::fixture()), files);
            let offer = provider.inventory().id;
            let lease = registry.register(provider).unwrap();
            let mut replies = Replies::default();
            registry
                .serve(
                    PEER,
                    ContentMsg::CloudLinkRequest {
                        offer,
                        roots: vec![NodeId(1)],
                        demand: Demand::Hover(oink_foundation::content::Holder { device: PEER, epoch: 1 }),
                    },
                    &mut replies,
                )
                .await
                .unwrap();
            let ContentMsg::CloudLinkOffer { token, .. } = replies.messages.pop().unwrap() else {
                panic!("no provisional grant")
            };
            let files = grants.open(PEER, token).unwrap();
            let selected = files.select(NodeId(1)).unwrap();
            registry
                .serve(
                    PEER,
                    ContentMsg::CloudLinkRead {
                        offer,
                        token,
                        node: NodeId(1),
                        version: selected.version,
                        offset: 0,
                    },
                    &mut replies,
                )
                .await
                .unwrap();
            assert!(matches!(
                replies.messages.last(),
                Some(ContentMsg::FileFailed {
                    reason: Unavailable::Permission,
                    ..
                })
            ));
            match action {
                0 => lease.cancel(),
                1 => drop(lease),
                _ => registry.cancel_all(),
            }
            assert!(grants.open(PEER, token).is_err());
            assert!(files.select(NodeId(1)).is_err());
        }
    }
    #[tokio::test]
    async fn persistent_grant_reply_can_be_retried_after_source_rename() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("source");
        std::fs::write(&file, b"payload").unwrap();
        let grants = grants::SourceGrants::load(directory.path(), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let files = namespace::Local::new(vec![(NodeId(1), file.clone())]).unwrap();
        let provider = with_files(Arc::new(super::super::tests::fixture()), files);
        let offer = provider.inventory().id;
        let _lease = registry.register(provider).unwrap();
        let request = ContentMsg::CloudLinkRequest {
            offer,
            roots: vec![NodeId(1)],
            demand: Demand::Clipboard,
        };
        let mut replies = Replies::default();
        registry.serve(PEER, request.clone(), &mut replies).await.unwrap();
        let first = replies.messages.pop().unwrap();
        assert!(matches!(first, ContentMsg::CloudLinkOffer { .. }));
        std::fs::rename(&file, directory.path().join("renamed")).unwrap();
        registry.serve(PEER, request, &mut replies).await.unwrap();
        assert_eq!(replies.messages.pop(), Some(first));
    }
    #[tokio::test]
    async fn persistent_grants_require_live_offer_scope_and_authenticated_holder() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("source");
        std::fs::write(&file, b"payload").unwrap();
        let grants = grants::SourceGrants::load(directory.path(), MEMBERS).unwrap();
        let registry = Arc::new(Registry::with_source_grants(grants));
        let files = namespace::Local::new(vec![(NodeId(1), file)]).unwrap();
        let provider = with_files(Arc::new(super::super::tests::fixture()), files);
        let offer = provider.inventory().id;
        let lease = registry.register(provider).unwrap();
        for (roots, demand) in [
            (vec![NodeId(2)], Demand::Clipboard),
            (
                vec![NodeId(1)],
                Demand::Hover(oink_foundation::content::Holder {
                    device: DeviceId([3; 32]),
                    epoch: 1,
                }),
            ),
        ] {
            let mut replies = Replies::default();
            registry
                .serve(
                    PEER,
                    ContentMsg::CloudLinkRequest { offer, roots, demand },
                    &mut replies,
                )
                .await
                .unwrap();
            assert!(matches!(
                replies.messages[0],
                ContentMsg::CloudLinkUnavailable {
                    reason: Unavailable::Permission,
                    ..
                }
            ));
        }
        lease.cancel();
        let mut replies = Replies::default();
        registry
            .serve(
                PEER,
                ContentMsg::CloudLinkRequest {
                    offer,
                    roots: vec![NodeId(1)],
                    demand: Demand::Clipboard,
                },
                &mut replies,
            )
            .await
            .unwrap();
        assert!(matches!(
            replies.messages[0],
            ContentMsg::CloudLinkUnavailable {
                reason: Unavailable::Superseded,
                ..
            }
        ));
    }
}
