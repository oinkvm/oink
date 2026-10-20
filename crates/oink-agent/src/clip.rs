//! Native clipboard ownership, copy ordering and delivery lifetime.
use crate::api::{ClipActivity, ClipKind, ClipOutcome, ClipboardSettings, FileTransferPhase, FileTransferStatus};
use crate::streams::StreamModule;
use oink_clipboard::{
    ClipboardBackend,
    content::{self, Provider},
    model::{ClipId, Demand, Inventory, Unavailable},
};
use oink_foundation::DeviceId;
use oink_protocol::v1::dto;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{
    mpsc::{UnboundedSender, unbounded_channel},
    oneshot,
};
mod clock;
#[cfg(all(test, debug_assertions))]
mod fetch_tests;
#[cfg(all(test, debug_assertions))]
mod health_tests;
#[cfg(all(test, debug_assertions))]
mod native_transfer_tests;
mod runtime;
#[cfg(all(test, debug_assertions))]
mod transfer_tests;
pub(crate) use runtime::start_link_runtime;
const ACTIVITY_MAX: usize = 50;
pub fn default_settings() -> ClipboardSettings {
    ClipboardSettings::default()
}
pub type NameOf = Arc<dyn Fn(DeviceId) -> String + Send + Sync>;
pub type OnActivity = Arc<dyn Fn(&ClipActivity) + Send + Sync>;
pub type OnTransfer = Arc<dyn Fn(Option<FileTransferStatus>) + Send + Sync>;
struct WriteTicket {
    valid: AtomicBool,
    publishing: AtomicBool,
}
struct NativeWrite {
    id: ClipId,
    ticket: Arc<WriteTicket>,
    contents: Arc<dyn Provider>,
}
struct Publication {
    inventory: Inventory,
    files: bool,
    materialized: bool,
    result: Result<u64, oink_clipboard::ClipboardError>,
}
enum Input {
    Reconfigure,
    LocalChange(u64),
    Captured(
        u64,
        Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError>,
    ),
    Progress(ClipId, oink_clipboard::store::BatchId, content::TransferProgress),
    Prefetched {
        id: ClipId,
        ticket: Arc<()>,
        before: u64,
        result: Result<oink_clipboard::NativeSpec, Unavailable>,
    },
    Applied {
        id: ClipId,
        ticket: Arc<WriteTicket>,
        publication: Publication,
    },
    FetchFiles,
    CancelFiles,
    #[cfg(target_os = "macos")]
    FileProviderAvailable,
    Barrier(oneshot::Sender<()>),
    Stop(oneshot::Sender<()>),
}
struct Shared {
    native: Arc<content::Registry>,
    native_target: Mutex<Option<(ClipId, content::Remote)>>,
    links: Arc<crate::streams::clipboard::files::FileLinks>,
    settings: Mutex<ClipboardSettings>,
    policy: oink_clipboard::policy::SharingPolicy,
    enabled: AtomicBool,
    paused: AtomicBool,
    activity: Mutex<VecDeque<ClipActivity>>,
    transfer: Mutex<Option<FileTransferStatus>>,
    dnd_files: Mutex<Option<Arc<crate::dnd::DndFiles>>>,
}
#[derive(Clone)]
pub struct ClipHandle {
    tx: UnboundedSender<Input>,
    shared: Arc<Shared>,
}
impl std::fmt::Debug for ClipHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipHandle").finish_non_exhaustive()
    }
}
impl ClipHandle {
    pub fn settings(&self) -> ClipboardSettings {
        *self.shared.settings.lock().unwrap()
    }
    pub fn set_settings(&self, settings: ClipboardSettings) {
        let mut current = self.shared.settings.lock().unwrap();
        if *current == settings {
            return;
        }
        *current = settings;
        drop(current);
        self.reconfigure();
    }
    /// The caller durably commits owner option3 before applying it here.
    pub fn set_enabled(&self, enabled: bool) {
        if self.shared.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            self.reconfigure();
        }
    }
    pub fn set_paused(&self, paused: bool) {
        if self.shared.paused.swap(paused, Ordering::AcqRel) != paused {
            self.reconfigure();
        }
    }
    fn reconfigure(&self) {
        let active = self.shared.enabled.load(Ordering::Acquire) && !self.shared.paused.load(Ordering::Acquire);
        self.shared.policy.update(active, self.settings());
        self.shared.native.cancel_all();
        if let Some((_, remote)) = self.shared.native_target.lock().unwrap().take() {
            remote.cancel();
        }
        self.shared.links.cancel_all();
        self.shared.links.pause_changed(!active);
        let _ = self.tx.send(Input::Reconfigure);
    }
    pub fn native_transfer_progress(&self) -> Option<tokio::sync::watch::Receiver<content::TransferProgress>> {
        self.shared
            .native_target
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, r)| r.progress())
    }
    pub fn fetch_files(&self) {
        let _ = self.tx.send(Input::FetchFiles);
    }
    pub fn cancel_files(&self) {
        if let Some((_, remote)) = self.shared.native_target.lock().unwrap().as_ref() {
            remote.cancel();
        }
        let _ = self.tx.send(Input::CancelFiles);
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn file_provider_available(&self) {
        let _ = self.tx.send(Input::FileProviderAvailable);
    }
    pub fn transfer(&self) -> Option<FileTransferStatus> {
        self.shared.transfer.lock().unwrap().clone()
    }
    pub fn activity(&self) -> Vec<ClipActivity> {
        self.shared.activity.lock().unwrap().iter().rev().cloned().collect()
    }
    pub(crate) fn set_dnd_files(&self, files: Arc<crate::dnd::DndFiles>) {
        *self.shared.dnd_files.lock().unwrap() = Some(files);
    }
    pub async fn settle(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Barrier(tx)).is_ok() {
            let _ = rx.await;
        }
    }
    pub async fn shutdown(&self) {
        self.set_paused(true);
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Stop(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}
struct Task {
    streams: crate::streams::clipboard::ClipboardStreams,
    ordering: oink_clipboard::sync::Ordering,
    clock: clock::Clock,
    fault: Option<dto::ResourceFaultCode>,
    native_source: Option<content::SourceLease>,
    capture: Option<tokio::task::JoinHandle<()>>,
    capture_again: bool,
    backend: Arc<dyn ClipboardBackend>,
    data_dir: PathBuf,
    me: DeviceId,
    tx: UnboundedSender<Input>,
    shared: Arc<Shared>,
    observed: u64,
    last_write: Option<u64>,
    remote_scope: Option<(ClipId, DeviceId, oink_network::link::StreamId)>,
    cancelled: Option<ClipId>,
    prefetch: Option<(ClipId, Arc<()>)>,
    provider_retry: Option<ClipId>,
    fetch_requested: Option<ClipId>,
    native_write: Option<NativeWrite>,
    queued_write: Option<(ClipId, oink_clipboard::NativeSpec)>,
    on_transfer: OnTransfer,
    on_activity: OnActivity,
    name_of: NameOf,
}
impl Task {
    fn active(&self) -> bool {
        self.shared.policy.enabled()
    }
    fn capture(&mut self) {
        if !self.active() {
            return;
        }
        if self.capture.is_some() {
            self.capture_again = true;
            return;
        }
        let backend = self.backend.clone();
        let data = self.data_dir.clone();
        let tx = self.tx.clone();
        self.capture = Some(tokio::task::spawn_blocking(move || {
            let count = backend.change_count();
            let _ = tx.send(Input::Captured(count, backend.capture(&data)));
        }));
    }
    fn observe(&mut self, count: u64) {
        if self
            .native_write
            .as_ref()
            .is_some_and(|write| write.ticket.publishing.load(Ordering::Acquire))
        {
            return;
        }
        if count == self.observed {
            return;
        }
        self.observed = count;
        if self.last_write == Some(count) {
            return;
        }
        self.cancel_native_write();
        self.prefetch = None;
        self.fetch_requested = None;
        self.provider_retry = None;
        self.remote_scope = None;
        self.cancelled = None;
        self.ordering.local_change(count, Instant::now());
        self.streams.set_offer(None);
        self.set_transfer(None);
    }
    fn persist(&mut self) -> bool {
        if self.clock.save(self.ordering.clock()).is_ok() {
            return true;
        }
        self.fault = Some(dto::ResourceFaultCode::BackendFailed);
        self.shared.policy.update(false, *self.shared.settings.lock().unwrap());
        self.shared.native.cancel_all();
        if let Some((_, remote)) = self.shared.native_target.lock().unwrap().take() {
            remote.cancel();
        }
        self.shared.links.cancel_all();
        self.streams.set_offer(None);
        tracing::error!(
            reason = "clock_write_failed",
            "clipboard identity could not be persisted"
        );
        false
    }
    fn captured(
        &mut self,
        before: u64,
        result: Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError>,
    ) {
        self.capture = None;
        if !self.active() {
            return;
        }
        if std::mem::take(&mut self.capture_again) {
            if self.ordering.deadline().is_none() {
                self.capture();
            }
            return;
        }
        // Errors belong to the clipboard slot inspected by this worker, too.
        // A remote write or local copy may replace it before capture completes.
        let current = self.backend.change_count();
        if current != before {
            self.observe(current);
            return;
        }
        let captured = match result {
            Ok(value) => value,
            Err(oink_clipboard::ClipboardError::Busy) => {
                self.ordering.local_change(self.backend.change_count(), Instant::now());
                return;
            }
            Err(oink_clipboard::ClipboardError::ContentUnavailable(reason)) => {
                // Deleted/inaccessible files and invalid item metadata belong to
                // this copy, not the lifetime of the native clipboard service.
                self.fault = None;
                self.ordering.revoke_local();
                self.streams.set_offer(None);
                tracing::warn!(?reason, "clipboard content could not be captured");
                return;
            }
            Err(error) => {
                self.fault = Some(if matches!(error, oink_clipboard::ClipboardError::Unsupported) {
                    dto::ResourceFaultCode::Unsupported
                } else {
                    dto::ResourceFaultCode::BackendFailed
                });
                tracing::warn!(fault = ?self.fault, "clipboard backend capture failed");
                self.ordering.revoke_local();
                self.streams.set_offer(None);
                return;
            }
        };
        let count = captured.snapshot.change_count;
        if self.backend.change_count() != count {
            self.observe(self.backend.change_count());
            return;
        }
        self.observed = count;
        let Some(provider) = captured.content else {
            self.fault = Some(dto::ResourceFaultCode::Unsupported);
            self.ordering.revoke_local();
            self.streams.set_offer(None);
            return;
        };
        self.fault = None;
        let provider = self.shared.policy.provider(provider);
        let Ok(inventory) = oink_clipboard::wire::inventory(&provider.inventory()) else {
            return;
        };
        let offer = match self.ordering.capture(
            inventory,
            count,
            captured.snapshot.sensitivity,
            crate::layout_sync::now_ms(),
        ) {
            Ok(offer) => offer,
            Err(_) => {
                self.shared.policy.update(false, *self.shared.settings.lock().unwrap());
                self.shared.native.cancel_all();
                self.streams.set_offer(None);
                return;
            }
        };
        let Some(offer) = offer else {
            self.streams.set_offer(None);
            return;
        };
        if !self.persist() {
            return;
        }
        let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id) else {
            return;
        };
        let provider = content::identified(provider, id);
        let lease = match self.shared.native.register_clipboard(provider) {
            Ok(lease) => lease,
            Err(_) => return,
        };
        self.native_source = Some(lease);
        self.record(
            id.clip.origin,
            &oink_clipboard::wire::local_inventory(&offer.inventory().unwrap()).unwrap(),
            ClipOutcome::Synced,
        );
        self.streams.set_offer(Some(offer));
    }
    fn current_id(&self) -> Option<ClipId> {
        self.ordering
            .current()?
            .id()
            .ok()
            .and_then(|id| oink_clipboard::wire::local_offer_id(id).ok())
            .map(|id| id.clip)
    }
    fn may_write(&mut self, id: ClipId) -> bool {
        self.observe(self.backend.change_count());
        self.active()
            && self.cancelled != Some(id)
            && self.current_id() == Some(id)
            && self.remote_scope.is_some_and(|(current, peer, stream)| {
                current == id
                    && self.streams.inbound_id(peer) == Some(stream)
                    && self.shared.links.offer_current(peer, id)
            })
    }
    fn spec(&self, offer: &dto::ClipboardOffer) -> Result<oink_clipboard::NativeSpec, oink_clipboard::ClipboardError> {
        use oink_clipboard::ClipboardError as E;
        let raw = offer.inventory().map_err(|_| E::Unsupported)?;
        let id = offer
            .id()
            .and_then(oink_clipboard::wire::local_offer_id)
            .map_err(|_| E::Unsupported)?;
        let target = if cfg!(target_os = "macos") {
            oink_foundation::clipboard::ClipPlatform::Mac
        } else {
            oink_foundation::clipboard::ClipPlatform::Windows
        };
        let existing = self
            .shared
            .native_target
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(clip, _)| *clip == id.clip)
            .filter(|(_, r)| {
                !matches!(
                    r.progress().borrow().phase,
                    content::TransferPhase::Failed | content::TransferPhase::Cancelled
                )
            })
            .map(|(_, r)| r.clone());
        let (remote, created) = if let Some(remote) = existing {
            (remote, false)
        } else {
            let remote = content::Remote::on_wire_runtime(
                &self.data_dir,
                oink_clipboard::store::BatchId::random().map_err(|_| E::Unsupported)?,
                raw,
                target,
                self.shared.links.clone(),
                Demand::Clipboard,
                tokio::runtime::Handle::current(),
            )
            .map_err(|_| E::Unsupported)?;
            remote.set_receiver(self.me);
            self.set_transfer(None);
            *self.shared.native_target.lock().unwrap() = Some((id.clip, remote.clone()));
            (remote, true)
        };
        let provider = self.shared.policy.provider(Arc::new(remote.clone()));
        let inventory = provider.inventory();
        if inventory.wanted(target).next().is_none() && inventory.roots.is_empty() {
            return Err(E::Unsupported);
        }
        let (files, file_contents) = if inventory.namespace_roots_for(target).is_empty() {
            (vec![], None)
        } else {
            let files = content::namespace::Files::new(
                provider.namespace().ok_or(E::Unsupported)?,
                oink_clipboard::sync::files::TargetOs::current(),
            )
            .map_err(|_| E::Unsupported)?;
            (
                files.roots().to_vec(),
                Some(files as Arc<dyn oink_clipboard::FileContents>),
            )
        };
        // Representation readers also use Remote, but their Offered state is
        // not a file delivery. Admit file diagnostics only after reconstruction
        // has produced a nonempty, policy-approved native file namespace.
        if created && !files.is_empty() {
            self.set_transfer(Some(FileTransferStatus {
                id: format!("{}:{}", id.clip.origin, id.clip.seq),
                from_name: (self.name_of)(id.clip.origin),
                done: 0,
                total: 0,
                files_done: 0,
                files: files.len(),
                phase: FileTransferPhase::Pending,
                message: None,
            }));
            let tx = self.tx.clone();
            let mut progress = remote.progress();
            let batch = remote.batch().id();
            tokio::spawn(async move {
                loop {
                    let value = progress.borrow_and_update().clone();
                    let terminal = matches!(
                        value.phase,
                        content::TransferPhase::Stored
                            | content::TransferPhase::Failed
                            | content::TransferPhase::Cancelled
                    );
                    if tx.send(Input::Progress(id.clip, batch, value)).is_err() || terminal {
                        break;
                    }
                    if progress.changed().await.is_err() {
                        break;
                    }
                }
            });
        }
        remote.automatic_pull();
        Ok(oink_clipboard::NativeSpec {
            inventory,
            contents: provider,
            files,
            file_contents,
            sensitivity: oink_clipboard::wire::local_sensitivity(offer.sensitivity().map_err(|_| E::Unsupported)?)
                .map_err(|_| E::Unsupported)?,
        })
    }
    fn apply(&mut self, offer: dto::ClipboardOffer) {
        let Some(id) = self.current_id() else {
            return;
        };
        if !self.may_write(id) {
            return;
        }
        if self.prefetch.as_ref().is_some_and(|(current, _)| *current == id)
            || self
                .native_write
                .as_ref()
                .is_some_and(|write| write.id == id && write.ticket.valid.load(Ordering::Acquire))
        {
            return;
        }
        if self.native_write.is_none() && self.queued_write.as_ref().is_some_and(|(current, _)| *current == id) {
            let (_, spec) = self.queued_write.take().unwrap();
            self.enqueue_native_write(id, spec);
            return;
        }
        let spec = match self.spec(&offer) {
            Ok(spec) => spec,
            Err(e) => {
                tracing::debug!(%e,"clipboard reconstruction unavailable");
                return;
            }
        };
        self.enqueue_native_write(id, spec);
    }
    fn cancel_native_write(&mut self) {
        if let Some((_, spec)) = self.queued_write.take() {
            spec.contents.cancel();
        }
        if let Some(write) = &self.native_write {
            write.ticket.valid.store(false, Ordering::Release);
            if !write.ticket.publishing.load(Ordering::Acquire) {
                write.contents.cancel();
            }
        }
    }
    fn enqueue_native_write(&mut self, id: ClipId, spec: oink_clipboard::NativeSpec) {
        if self.native_write.is_some() {
            self.cancel_native_write();
            self.queued_write = Some((id, spec));
            return;
        }
        let ticket = Arc::new(WriteTicket {
            valid: AtomicBool::new(true),
            publishing: AtomicBool::new(false),
        });
        self.native_write = Some(NativeWrite {
            id,
            ticket: ticket.clone(),
            contents: spec.contents.clone(),
        });
        let backend = self.backend.clone();
        let before = backend.change_count();
        let policy = self.shared.policy.publication_guard();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let check = ticket.clone();
            let clipboard = backend.clone();
            let guard: content::ReaderGuard = Arc::new(move || {
                check.publishing.store(true, Ordering::Release);
                if check.valid.load(Ordering::Acquire) && policy().is_ok() && clipboard.change_count() == before {
                    Ok(())
                } else {
                    Err(content::unavailable(Unavailable::Cancelled))
                }
            });
            let inventory = spec.inventory.clone();
            let files = !spec.files.is_empty();
            let materialized = files && spec.file_contents.is_none();
            let result = if ticket.valid.load(Ordering::Acquire) {
                backend.write_native_guarded(spec, guard)
            } else {
                Err(oink_clipboard::ClipboardError::Busy)
            };
            let _ = tx.send(Input::Applied {
                id,
                ticket,
                publication: Publication {
                    inventory,
                    files,
                    materialized,
                    result,
                },
            });
        });
    }
    fn applied(&mut self, id: ClipId, ticket: Arc<WriteTicket>, publication: Publication) {
        if !self
            .native_write
            .as_ref()
            .is_some_and(|write| write.id == id && Arc::ptr_eq(&write.ticket, &ticket))
        {
            return;
        }
        self.native_write = None;
        // Watch callbacks can run before the blocking backend returns its count.
        // Mark that exact write before observing any newer physical local copy.
        if let Ok(count) = &publication.result {
            self.last_write = Some(*count);
        }
        self.observe(self.backend.change_count());
        if ticket.valid.load(Ordering::Acquire) && self.may_write(id) {
            match publication.result {
                Ok(count) => {
                    self.written(id, count);
                    self.record(id.origin, &publication.inventory, ClipOutcome::Synced);
                    if publication.files {
                        self.set_transfer(self.status(
                            id,
                            if publication.materialized {
                                FileTransferPhase::Ready
                            } else {
                                FileTransferPhase::Pending
                            },
                            None,
                        ));
                    }
                }
                Err(oink_clipboard::ClipboardError::FileProviderUnavailable) => {
                    self.provider_retry = Some(id);
                    self.set_transfer(self.status(id, FileTransferPhase::Pending, Some("等待文件下载请求".into())));
                }
                Err(error) => self.record(
                    id.origin,
                    &publication.inventory,
                    ClipOutcome::Failed {
                        message: error.to_string(),
                    },
                ),
            }
        }
        if self.fetch_requested == Some(id) && self.may_write(id) {
            self.fetch();
        }
        if let Some((next, spec)) = self.queued_write.take()
            && self.may_write(next)
        {
            self.enqueue_native_write(next, spec);
        }
    }
    fn written(&mut self, id: ClipId, count: u64) {
        self.observed = count;
        self.last_write = Some(count);
        self.ordering.written(count);
        self.provider_retry = None;
        self.native_source = None;
        self.cancelled = self.cancelled.filter(|old| *old != id);
    }
    fn fetch(&mut self) {
        let Some(id) = self
            .current_id()
            .filter(|_| self.active() && self.shared.settings.lock().unwrap().files)
        else {
            return;
        };
        self.fetch_requested = Some(id);
        // A previous publication can still own the native worker while this
        // offer is queued. Preserve demand until this offer's provider result
        // tells us whether download must also materialize the clipboard files.
        if self.native_write.as_ref().is_some_and(|write| write.id == id)
            || self.queued_write.as_ref().is_some_and(|(queued, _)| *queued == id)
        {
            return;
        }
        self.cancelled = None;
        let Some(offer) = self.ordering.current().cloned() else {
            return;
        };
        let Ok(mut spec) = self.spec(&offer) else {
            return;
        };
        if spec.files.is_empty() {
            self.fetch_requested = None;
            return;
        }
        let Some((_, remote)) = self
            .shared
            .native_target
            .lock()
            .unwrap()
            .clone()
            .filter(|(clip, _)| *clip == id)
        else {
            return;
        };
        if self.provider_retry != Some(id) {
            remote.start_delivery();
            self.fetch_requested = None;
            return;
        }
        if self.prefetch.is_some() {
            return;
        }
        self.fetch_requested = None;
        let ticket = Arc::new(());
        self.prefetch = Some((id, ticket.clone()));
        let before = self.backend.change_count();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = async {
                let local = remote
                    .prefetch_bounded(64 * 1024 * 1024)
                    .await
                    .map_err(|e| content::reason(&e))?;
                tokio::task::spawn_blocking(move || {
                    spec.files = local.materialize().map_err(|e| content::reason(&e))?;
                    spec.inventory = local.inventory();
                    spec.contents = Arc::new(local);
                    spec.file_contents = None;
                    Ok(spec)
                })
                .await
                .map_err(|_| Unavailable::ReadFailed)?
            }
            .await;
            let _ = tx.send(Input::Prefetched {
                id,
                ticket,
                before,
                result,
            });
        });
    }
    fn progress(&self, id: ClipId, batch: oink_clipboard::store::BatchId, value: content::TransferProgress) {
        // Clearing the current file offer retires its diagnostic record. Native
        // readers may outlive it, but queued progress must not re-admit it.
        if self.shared.transfer.lock().unwrap().is_none()
            || (self.cancelled == Some(id) && value.phase != content::TransferPhase::Cancelled)
        {
            return;
        }
        if !self
            .shared
            .native_target
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(clip, r)| *clip == id && r.batch().id() == batch)
        {
            return;
        }
        let phase = Self::transfer_phase(value.phase);
        let Some(mut status) = self.status(id, phase, None) else {
            return;
        };
        status.done = value.bytes;
        status.total = value.total_bytes.unwrap_or(0);
        status.files_done = value.files_done as usize;
        if let Some(files) = value.total_files {
            status.files = files as usize;
        }
        if value.enumerating {
            status.message = Some("正在准备文件…".into());
        }
        self.set_transfer(Some(status));
    }
    fn transfer_phase(phase: content::TransferPhase) -> FileTransferPhase {
        match phase {
            content::TransferPhase::Offered => FileTransferPhase::Pending,
            content::TransferPhase::Preparing
            | content::TransferPhase::Receiving
            | content::TransferPhase::Reconnecting => FileTransferPhase::Downloading,
            content::TransferPhase::Stored => FileTransferPhase::Ready,
            content::TransferPhase::Failed => FileTransferPhase::Failed,
            content::TransferPhase::Cancelled => FileTransferPhase::Cancelled,
        }
    }
    fn reconfigure(&mut self) {
        self.cancel_native_write();
        self.prefetch = None;
        self.fetch_requested = None;
        self.provider_retry = None;
        self.cancelled = None;
        self.remote_scope = None;
        self.native_source = None;
        self.ordering.revoke_local();
        self.streams.set_offer(None);
        self.streams.pause_changed(!self.active());
        self.set_transfer(None);
        let _ = self.backend.set_watch_paused(!self.active());
        if self.active() {
            self.capture();
        }
    }
    fn status(&self, id: ClipId, phase: FileTransferPhase, message: Option<String>) -> Option<FileTransferStatus> {
        let mut status = self
            .shared
            .transfer
            .lock()
            .unwrap()
            .clone()
            .filter(|status| status.id == format!("{}:{}", id.origin, id.seq))?;
        let target = self.shared.native_target.lock().unwrap();
        let (_, remote) = target.as_ref().filter(|(clip, _)| *clip == id)?;
        let progress = remote.progress().borrow().clone();
        status.done = progress.bytes;
        status.total = progress.total_bytes.unwrap_or(0);
        status.files_done = progress.files_done as usize;
        // Keep the offered root count until enumeration knows the regular-file
        // total. An enumerated empty directory legitimately contains no files.
        if let Some(files) = progress.total_files {
            status.files = files as usize;
        }
        // Native publication can finish after delivery's terminal progress
        // event. It must not reset that completed record back to Pending.
        status.phase = if phase == FileTransferPhase::Pending {
            Self::transfer_phase(progress.phase)
        } else {
            phase
        };
        status.message = message;
        Some(status)
    }
    fn set_transfer(&self, value: Option<FileTransferStatus>) {
        *self.shared.transfer.lock().unwrap() = value.clone();
        (self.on_transfer)(value);
    }
    fn record(&self, from: DeviceId, inventory: &Inventory, outcome: ClipOutcome) {
        use oink_clipboard::model::{RepresentationData as D, StandardFormat as F};
        let mut kinds = vec![];
        let mut bytes = 0u64;
        for rep in inventory.items.iter().flat_map(|i| &i.representations) {
            let kind = if matches!(rep.data, D::File(_) | D::Promise(..)) {
                ClipKind::Files
            } else {
                match rep.standard {
                    Some(F::Text) => ClipKind::Text,
                    Some(F::Html) => ClipKind::Html,
                    Some(F::Rtf) => ClipKind::Rtf,
                    Some(F::Png) => ClipKind::Image,
                    _ => ClipKind::Custom,
                }
            };
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
            if let Some(oink_clipboard::model::ContentSize::Known(n)) = rep.data.size() {
                bytes = bytes.saturating_add(n);
            }
        }
        let value = ClipActivity {
            at_ms: crate::layout_sync::now_ms() as i64,
            from_name: (self.name_of)(from),
            kinds,
            bytes,
            outcome,
        };
        let mut activity = self.shared.activity.lock().unwrap();
        if activity.len() >= ACTIVITY_MAX {
            activity.pop_front();
        }
        activity.push_back(value.clone());
        drop(activity);
        (self.on_activity)(&value);
    }
}
