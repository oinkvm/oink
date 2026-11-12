//! Native clipboard ownership, copy ordering and delivery lifetime.
use crate::api::{ClipActivity, ClipKind, ClipOutcome, FileTransferPhase, FileTransferStatus};
use crate::streams::StreamModule;
use oink_clipboard::{
    ClipboardBackend,
    content::{self, Provider},
    model::{ClipId, Demand, Inventory, Unavailable},
    policy::Settings as ClipboardSettings,
};
use oink_foundation::DeviceId;
use oink_foundation::clipboard_dedupe::{self as dedupe, Fingerprint, Identity, Source};
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
mod dedupe_tests;
#[cfg(all(test, debug_assertions))]
mod fetch_tests;
#[cfg(all(test, debug_assertions))]
mod health_tests;
#[cfg(all(test, debug_assertions))]
mod native_transfer_tests;
#[cfg(all(test, debug_assertions))]
mod pause_tests;
#[cfg(all(test, debug_assertions))]
mod promise_tests;
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
    provider_unavailable: bool,
    requires_enable: bool,
    result: Result<u64, oink_clipboard::ClipboardError>,
}
enum Input {
    Reconfigure,
    LocalChange(u64),
    Captured(
        u64,
        Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError>,
        Option<[u8; 32]>,
        Option<CapturedIdentity>,
    ),
    Progress(ClipId, oink_clipboard::store::BatchId, content::TransferProgress),
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
/// What the capture worker learned for duplicate detection (`clipboard_dedupe`).
pub(crate) struct CapturedIdentity {
    /// Content fingerprint of local data. None when it was not computed:
    /// deduplication off, or the marker named a known clip.
    fingerprint: Option<Fingerprint>,
    /// Universal Clipboard content: promised by another device, never read.
    external: bool,
    /// An Oink clip marker naming a clip this device knows.
    marker: Option<ClipId>,
}
/// Clipboard state before an uncaptured local change, kept until the capture
/// decides whether the change duplicates the content it replaced.
struct EchoBase {
    identity: Option<Identity>,
    current: Option<(DeviceId, dto::ClipboardOffer)>,
}
/// A clip this device recently announced or wrote; its marker may come back.
struct Recent {
    clip: ClipId,
    identity: Option<Identity>,
}
const RECENT_CLIPS: usize = 8;
struct Shared {
    _native_connection: Option<content::linked::Connection>,
    native: Arc<content::Registry>,
    native_target: Mutex<Option<(ClipId, content::Remote)>>,
    links: Arc<crate::streams::clipboard::files::FileLinks>,
    settings: Mutex<ClipboardSettings>,
    policy: oink_clipboard::policy::SharingPolicy,
    receive_policy: oink_clipboard::policy::SharingPolicy,
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
        // Deduplication is read at each capture: toggling it must not revoke
        // and re-announce the current clipboard like a sharing change does.
        let dedupe_only = ClipboardSettings {
            dedupe: settings.dedupe,
            ..*current
        } == settings;
        *current = settings;
        drop(current);
        if !dedupe_only {
            self.reconfigure();
        }
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
    /// A group-state change (layout, members, this device's pause publication).
    /// It only wakes the clipboard to re-read the owner's pause decision. The
    /// device-control owner records each decision and pauses or resumes the
    /// clipboard directly before it publishes it (an emergency only after the
    /// native safety fence), so the published pause can be older than the
    /// clipboard's state while other group-state changes keep arriving.
    pub(crate) fn config_state_changed(&self, node: &crate::node::Node) {
        self.set_paused(node.fully_paused());
    }
    fn reconfigure(&self) {
        let active = self.shared.enabled.load(Ordering::Acquire) && !self.shared.paused.load(Ordering::Acquire);
        let settings = self.settings();
        self.shared.policy.update(active && settings.send);
        self.shared.receive_policy.update(active && settings.receive);
        self.shared.native.cancel_all();
        if let Some((_, remote)) = self.shared.native_target.lock().unwrap().take() {
            remote.cancel();
        }
        self.shared.links.cancel_all();
        self.shared.links.pause_changed(!active);
        let _ = self.tx.send(Input::Reconfigure);
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
    copy_report: Option<crate::telemetry::Stamp>,
    apply_report: Option<(ClipId, crate::telemetry::Stamp)>,
    last_write: Option<u64>,
    remote_scope: Option<(ClipId, DeviceId, oink_network::link::StreamId)>,
    cancelled: Option<ClipId>,
    provider_retry: Option<ClipId>,
    fetch_requested: Option<ClipId>,
    native_write: Option<NativeWrite>,
    queued_write: Option<(ClipId, oink_clipboard::NativeSpec)>,
    /// Content identity of the native clipboard (`clipboard_dedupe`). Known
    /// only for this device's own captures; None for content Oink wrote (it
    /// is provided lazily by its source and never read to compare), after an
    /// uncaptured local change and after a failed capture.
    identity: Option<Identity>,
    echo_base: Option<EchoBase>,
    /// Clips whose Oink marker identifies a write-back (newest last).
    recent: VecDeque<Recent>,
    on_transfer: OnTransfer,
    on_provider_required: Arc<dyn Fn() + Send + Sync>,
    on_activity: OnActivity,
    name_of: NameOf,
}
fn offer_clip(offer: &dto::ClipboardOffer) -> Option<ClipId> {
    offer
        .id()
        .ok()
        .and_then(|id| oink_clipboard::wire::local_offer_id(id).ok())
        .map(|id| id.clip)
}
/// Capture worker: read the Oink clip marker (only that small type), remove it
/// from the captured provider (it never leaves this device), then fingerprint
/// local content when deduplication may compare it. Universal Clipboard
/// content is promised by another device and is never read.
fn captured_identity(
    captured: &mut Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError>,
    known: &[ClipId],
    compare: bool,
) -> Option<CapturedIdentity> {
    let provider = captured.as_mut().ok()?.content.take()?;
    let marker = oink_clipboard::dedupe::read_marker(provider.as_ref())
        .map(|id| id.clip)
        .filter(|clip| known.contains(clip));
    match oink_clipboard::dedupe::without_marker(provider) {
        Ok(provider) => {
            let external = oink_clipboard::dedupe::external(&provider.inventory());
            let fingerprint =
                (compare && marker.is_none()).then(|| oink_clipboard::dedupe::fingerprint(provider.as_ref(), external));
            if let Ok(captured) = captured.as_mut() {
                captured.content = Some(provider);
            }
            Some(CapturedIdentity {
                fingerprint,
                external,
                marker,
            })
        }
        Err(error) => {
            *captured = Err(oink_clipboard::ClipboardError::ContentUnavailable(content::reason(
                &error,
            )));
            None
        }
    }
}
fn copy_event(id: Option<oink_clipboard::model::OfferId>, stage: oink_clipboard::copy_diagnostics::Stage, count: u64) {
    oink_clipboard::copy_diagnostics::event(
        id,
        stage,
        oink_clipboard::copy_diagnostics::Values {
            count,
            ..Default::default()
        },
    );
}
impl Task {
    fn active(&self) -> bool {
        self.shared.enabled.load(Ordering::Acquire) && !self.shared.paused.load(Ordering::Acquire)
    }
    fn sending(&self) -> bool {
        self.active() && self.shared.policy.enabled()
    }
    fn receiving(&self) -> bool {
        self.active() && self.shared.receive_policy.enabled()
    }
    fn capture(&mut self) {
        if !self.sending() {
            return;
        }
        if self.capture.is_some() {
            self.capture_again = true;
            return;
        }
        let known = self.known_clips();
        let compare = self.shared.settings.lock().unwrap().dedupe;
        let backend = self.backend.clone();
        let data = self.data_dir.clone();
        let tx = self.tx.clone();
        self.capture = Some(tokio::task::spawn_blocking(move || {
            let count = backend.change_count();
            copy_event(None, oink_clipboard::copy_diagnostics::Stage::CaptureBegin, count);
            let started = Instant::now();
            let mut captured = backend.capture(&data);
            let identity = captured_identity(&mut captured, &known, compare);
            oink_clipboard::copy_diagnostics::event(
                None,
                oink_clipboard::copy_diagnostics::Stage::CaptureEnd,
                oink_clipboard::copy_diagnostics::Values {
                    count,
                    elapsed_us: started.elapsed().as_micros() as u64,
                    result: if captured.is_ok() { 1 } else { 2 },
                    reason: captured
                        .as_ref()
                        .err()
                        .map_or(0, oink_clipboard::copy_diagnostics::backend_reason),
                    ..Default::default()
                },
            );
            use oink_clipboard::copy_diagnostics::{Stage, capture_handoff};
            capture_handoff(Stage::ClipboardKeyBegin, count, 0, 0);
            let key_started = Instant::now();
            let key = backend.clipboard_key().ok();
            capture_handoff(
                Stage::ClipboardKeyEnd,
                count,
                if key.is_some() { 1 } else { 2 },
                key_started.elapsed().as_micros() as u64,
            );
            let sent = tx.send(Input::Captured(count, captured, key, identity)).is_ok();
            // The actor may consume the message before this event is emitted;
            // correlate by count, not by assuming stage 18 precedes stage 19.
            capture_handoff(Stage::CapturedSent, count, if sent { 1 } else { 2 }, 0);
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
        self.fetch_requested = None;
        self.provider_retry = None;
        self.remote_scope = None;
        self.cancelled = None;
        self.copy_report = crate::telemetry::producer().and_then(|p| p.stamp());
        self.apply_report = None;
        copy_event(None, oink_clipboard::copy_diagnostics::Stage::Observed, count);
        let base = EchoBase {
            identity: self.identity.clone(),
            current: self.ordering.current_with_source(),
        };
        if self.ordering.local_change(count, Instant::now()) {
            // Keep the state from before the FIRST uncaptured change: the
            // intermediate clipboards were never announced.
            self.echo_base.get_or_insert(base);
            self.identity = None;
        }
        if !self.sending() {
            self.ordering.revoke_local();
            // No capture will settle this change.
            self.echo_base = None;
        }
        self.streams.set_offer(None);
        self.set_transfer(None);
    }
    fn persist(&mut self) -> bool {
        if self.clock.save(self.ordering.clock()).is_ok() {
            return true;
        }
        self.fault = Some(dto::ResourceFaultCode::BackendFailed);
        self.shared.policy.update(false);
        self.shared.receive_policy.update(false);
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
    fn capture_discard(&self, reason: oink_clipboard::copy_diagnostics::CaptureDiscard, count: u64) {
        oink_clipboard::copy_diagnostics::capture_discard(
            reason,
            count,
            self.backend.change_count(),
            self.ordering.written_count(),
        );
    }
    fn captured(
        &mut self,
        before: u64,
        result: Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError>,
        native_key: Option<[u8; 32]>,
        captured_identity: Option<CapturedIdentity>,
    ) {
        use oink_clipboard::copy_diagnostics::CaptureDiscard as D;
        self.capture = None;
        // Retries below keep the base; every other outcome settles it.
        let base = self.echo_base.take();
        if !self.sending() {
            self.capture_discard(D::Inactive, before);
            return;
        }
        if std::mem::take(&mut self.capture_again) {
            self.echo_base = base;
            self.capture_discard(D::CaptureAgain, before);
            if self.ordering.deadline().is_none() {
                self.capture();
            }
            return;
        }
        // Errors belong to the clipboard slot inspected by this worker, too.
        // A remote write or local copy may replace it before capture completes.
        let current = self.backend.change_count();
        if current != before {
            self.echo_base = base;
            self.capture_discard(D::BeforeChanged, before);
            self.observe(current);
            return;
        }
        let captured = match result {
            Ok(value) => value,
            Err(oink_clipboard::ClipboardError::Busy) => {
                self.echo_base = base;
                self.capture_discard(D::Busy, before);
                self.ordering.local_change(self.backend.change_count(), Instant::now());
                return;
            }
            Err(oink_clipboard::ClipboardError::ContentUnavailable(reason)) => {
                self.capture_discard(D::ContentUnavailable, before);
                // Deleted/inaccessible files and invalid item metadata belong to
                // this copy, not the lifetime of the native clipboard service.
                self.fault = None;
                self.ordering.revoke_local();
                self.streams.set_offer(None);
                tracing::warn!(?reason, "clipboard content could not be captured");
                return;
            }
            Err(error) => {
                self.capture_discard(D::BackendFailure, before);
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
            self.echo_base = base;
            self.capture_discard(D::SnapshotChanged, count);
            self.observe(self.backend.change_count());
            return;
        }
        self.observed = count;
        let Some(provider) = captured.content else {
            self.capture_discard(D::MissingProvider, count);
            self.fault = Some(dto::ResourceFaultCode::Unsupported);
            self.ordering.revoke_local();
            self.streams.set_offer(None);
            return;
        };
        self.fault = None;
        let identity_source = provider.namespace();
        let provider = self.shared.policy.provider(provider);
        let Ok(inventory) = oink_clipboard::wire::inventory(&provider.inventory()) else {
            self.capture_discard(D::InvalidInventory, count);
            return;
        };
        let captured_identity = captured_identity.unwrap_or(CapturedIdentity {
            fingerprint: None,
            external: false,
            marker: None,
        });
        if let Some(base) = base
            && self.duplicate(base, &captured_identity, &provider, native_key)
        {
            // Nothing new was announced: no publish interval to report.
            self.copy_report = None;
            return;
        }
        let offer = match self.ordering.capture(
            inventory,
            count,
            captured.snapshot.sensitivity,
            crate::layout_sync::now_ms(),
        ) {
            Ok(offer) => offer,
            Err(_) => {
                self.capture_discard(D::OrderingFailure, count);
                self.shared.policy.update(false);
                self.shared.receive_policy.update(false);
                self.shared.native.cancel_all();
                self.streams.set_offer(None);
                return;
            }
        };
        let Some(offer) = offer else {
            self.capture_discard(
                if count != 0 && count <= self.ordering.written_count() {
                    D::OwnWrite
                } else {
                    D::EmptyInventory
                },
                count,
            );
            self.streams.set_offer(None);
            return;
        };
        if !self.persist() {
            self.capture_discard(D::PersistFailure, count);
            return;
        }
        let Ok(id) = offer.id().and_then(oink_clipboard::wire::local_offer_id) else {
            self.capture_discard(D::InvalidOfferId, count);
            return;
        };
        copy_event(Some(id), oink_clipboard::copy_diagnostics::Stage::OfferReady, count);
        let provider = content::identified(provider, id);
        let lease = match self
            .shared
            .native
            .register_clipboard_identified(provider, native_key, identity_source)
        {
            Ok(lease) => lease,
            Err(_) => {
                self.capture_discard(D::RegistrationFailure, count);
                return;
            }
        };
        self.native_source = Some(lease);
        let identity = captured_identity.fingerprint.map(|fingerprint| Identity {
            fingerprint,
            source: if captured_identity.external {
                Source::External
            } else {
                Source::Device(self.me)
            },
        });
        self.remember(id.clip, identity.clone());
        self.identity = identity;
        self.record(
            id.clip.origin,
            &oink_clipboard::wire::local_inventory(&offer.inventory().unwrap()).unwrap(),
            ClipOutcome::Synced,
        );
        self.streams.set_offer(Some(offer));
        if let Some(stamp) = self.copy_report.take()
            && let Some(p) = crate::telemetry::producer()
        {
            p.interval(stamp, oink_telemetry::Metric::ClipboardPublish, Instant::now());
        }
    }
    /// `clipboard_dedupe` on the sending side: a local change naming a known
    /// clip by its Oink marker, or (with deduplication on) a format subset of
    /// this device's own previous copy, is a duplicate. It is not announced
    /// and the previous clip identity stays current. When that identity is our
    /// own announced offer, peers keep reading it, so it is rebound to the new
    /// native clipboard; if it cannot be, the change is announced normally.
    fn duplicate(
        &mut self,
        base: EchoBase,
        captured: &CapturedIdentity,
        provider: &Arc<dyn Provider>,
        native_key: Option<[u8; 32]>,
    ) -> bool {
        let source = if captured.external {
            Source::External
        } else {
            Source::Device(self.me)
        };
        let base_clip = base.current.as_ref().and_then(|(_, offer)| offer_clip(offer));
        let (identity, current, comparison) = match captured.marker {
            Some(clip) if base_clip == Some(clip) => (base.identity, base.current, "marker"),
            Some(clip) => {
                // An earlier clip of ours came back (a synchronizer delivering
                // late, or a clipboard history): the clipboard is that clip
                // again. Not announced; it cannot be rebound (its source is gone).
                let Some(recent) = self.recent.iter().find(|recent| recent.clip == clip) else {
                    return false;
                };
                tracing::debug!(
                    direction = "local",
                    new_source = ?source,
                    marker = true,
                    comparison = "marker-earlier-clip",
                    "clipboard duplicate ignored: not announced"
                );
                self.identity = recent.identity.clone();
                self.ordering.restore(None);
                return true;
            }
            None => {
                if !self.shared.settings.lock().unwrap().dedupe {
                    tracing::trace!(direction = "local", "clipboard deduplication is off");
                    return false;
                }
                let Some(identity) = base.identity else {
                    // Content Oink wrote is read lazily from its source device:
                    // never read to compare. Protocol v1 offers carry no digests.
                    tracing::trace!(direction = "local", new_source = ?source, "clipboard change not compared: previous content unknown");
                    return false;
                };
                let Some(fingerprint) = &captured.fingerprint else {
                    return false;
                };
                if let Err(fresh) = dedupe::echo(fingerprint, &identity) {
                    if captured.external {
                        tracing::debug!(direction = "local", new_source = ?source, comparison = ?fresh, "clipboard change not compared: Universal Clipboard content is not read");
                    } else {
                        tracing::trace!(direction = "local", new_source = ?source, current_source = ?identity.source, comparison = ?fresh, "clipboard change is a new copy");
                    }
                    return false;
                }
                (Some(identity), base.current, "subset")
            }
        };
        let own = current.as_ref().is_some_and(|(origin, _)| *origin == self.me);
        if own {
            let Some(lease) = self.native_source.take() else {
                tracing::debug!(
                    direction = "local",
                    comparison,
                    "clipboard duplicate announced: own offer has no source"
                );
                return false;
            };
            match content::rebound(lease.inventory(), provider.clone()) {
                Ok(rebound) => match self.shared.native.replace_clipboard(lease, rebound, native_key) {
                    Ok(lease) => self.native_source = Some(lease),
                    Err(error) => {
                        // The old lease is gone with the replaced native slot.
                        tracing::debug!(direction = "local", comparison, kind = ?error.kind(), "clipboard duplicate announced: own offer could not be rebound");
                        return false;
                    }
                },
                Err(error) => {
                    self.native_source = Some(lease);
                    tracing::debug!(direction = "local", comparison, reason = ?content::reason(&error), "clipboard duplicate announced: own offer cannot be served from the new clipboard");
                    return false;
                }
            }
        }
        tracing::debug!(
            direction = "local",
            new_source = ?source,
            current_source = ?identity.as_ref().map(|identity| identity.source),
            marker = comparison == "marker",
            comparison,
            rebound = own,
            "clipboard duplicate ignored: not announced"
        );
        if let Some((_, offer)) = current.as_ref().filter(|(origin, _)| *origin == self.me) {
            // A local change withdrew it from the streams; peers already hold
            // this offer and their ordering fence drops the repeat.
            self.streams.set_offer(Some(offer.clone()));
        }
        self.ordering.restore(current);
        self.identity = identity;
        true
    }
    /// Clips whose marker identifies a write-back: recent ones and the current.
    fn known_clips(&self) -> Vec<ClipId> {
        let mut known: Vec<ClipId> = self.recent.iter().map(|recent| recent.clip).collect();
        known.extend(
            self.echo_base
                .as_ref()
                .and_then(|base| base.current.as_ref().map(|(_, offer)| offer))
                .or(self.ordering.current())
                .and_then(offer_clip),
        );
        known
    }
    fn remember(&mut self, clip: ClipId, identity: Option<Identity>) {
        self.recent.retain(|recent| recent.clip != clip);
        if self.recent.len() == RECENT_CLIPS {
            self.recent.pop_front();
        }
        self.recent.push_back(Recent { clip, identity });
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
        self.receiving()
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
        let provider = self.shared.receive_policy.provider(Arc::new(remote.clone()));
        let inventory = provider.inventory();
        if inventory.wanted(target).next().is_none() && inventory.namespace_roots_for(target).is_empty() {
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
            let batch = remote.batch_id();
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
        oink_clipboard::received::remember_source(id.clip.origin, (self.name_of)(id.clip.origin));
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
        if self
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
        let report = self
            .apply_report
            .take()
            .filter(|(current, _)| *current == id)
            .map(|(_, stamp)| stamp);
        let backend = self.backend.clone();
        let before = backend.change_count();
        let policy = self.shared.receive_policy.publication_guard();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let check = ticket.clone();
            let clipboard = backend.clone();
            let preparing_ticket = ticket.clone();
            let preparing_policy = policy.clone();
            let current: content::ReaderGuard = Arc::new(move || {
                if preparing_ticket.valid.load(Ordering::Acquire) {
                    preparing_policy()
                } else {
                    Err(content::unavailable(Unavailable::Cancelled))
                }
            });
            let guard: content::ReaderGuard = Arc::new(move || {
                check.publishing.store(true, Ordering::Release);
                if check.valid.load(Ordering::Acquire) && policy().is_ok() && clipboard.change_count() == before {
                    Ok(())
                } else {
                    Err(content::unavailable(Unavailable::Cancelled))
                }
            });
            let mut inventory = spec.inventory.clone();
            let mut files = !spec.files.is_empty();
            let materialized = files && spec.file_contents.is_none();
            let non_files = spec.clone().without_files().ok().flatten();
            copy_event(
                Some(inventory.id),
                oink_clipboard::copy_diagnostics::Stage::DeclareBegin,
                before,
            );
            let mut result = if ticket.valid.load(Ordering::Acquire) {
                backend.write_native_cancellable(spec, guard.clone(), current.clone())
            } else {
                Err(oink_clipboard::ClipboardError::Busy)
            };
            let provider_unavailable = matches!(
                &result,
                Err(oink_clipboard::ClipboardError::FileProviderUnavailable
                    | oink_clipboard::ClipboardError::FileProvider(_))
            );
            let requires_enable = match &result {
                Err(oink_clipboard::ClipboardError::FileProvider(reason)) => reason.needs_enable(),
                Err(oink_clipboard::ClipboardError::FileProviderUnavailable) => true,
                _ => false,
            };
            if provider_unavailable && let Some(non_files) = non_files {
                inventory = non_files.inventory.clone();
                files = false;
                result = backend.write_native_cancellable(non_files, guard, current);
            }
            oink_clipboard::copy_diagnostics::event(
                Some(inventory.id),
                oink_clipboard::copy_diagnostics::Stage::DeclareEnd,
                oink_clipboard::copy_diagnostics::Values {
                    count: result.as_ref().copied().unwrap_or(before),
                    result: if result.is_ok() { 1 } else { 2 },
                    reason: result
                        .as_ref()
                        .err()
                        .map_or(0, oink_clipboard::copy_diagnostics::backend_reason),
                    ..Default::default()
                },
            );
            if let Some(stamp) = report
                && let Some(p) = crate::telemetry::producer()
            {
                let result = if !ticket.valid.load(Ordering::Acquire) {
                    oink_telemetry::OperationResult::Cancelled
                } else if result.is_ok() {
                    oink_telemetry::OperationResult::Completed
                } else {
                    oink_telemetry::OperationResult::Failed
                };
                p.interval(stamp, oink_telemetry::Metric::ClipboardApply { result }, Instant::now());
            }
            let _ = tx.send(Input::Applied {
                id,
                ticket,
                publication: Publication {
                    inventory,
                    files,
                    materialized,
                    provider_unavailable,
                    requires_enable,
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
            // The clipboard now holds Oink's write, whatever happens to its
            // ticket. A newer local copy observed below replaced this write
            // (never read to compare, see `written`), not the content before
            // it: deduplicating against that would drop a real copy.
            self.identity = None;
            self.echo_base = None;
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
                Err(_) if publication.provider_unavailable => {}
                Err(error) => self.record(
                    id.origin,
                    &publication.inventory,
                    ClipOutcome::Failed {
                        message: error.to_string(),
                    },
                ),
            }
        }
        if publication.provider_unavailable && ticket.valid.load(Ordering::Acquire) && self.may_write(id) {
            self.provider_retry = Some(id);
            self.set_transfer(self.status(id, FileTransferPhase::Failed, Some("File provider unavailable".into())));
            if publication.requires_enable {
                (self.on_provider_required)();
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
        self.echo_base = None;
        // Content Oink wrote is provided lazily by its source device: it is
        // never read to compare, so the next local change is always announced
        // unless its Oink marker names this clip.
        self.identity = None;
        self.remember(id, None);
        self.ordering.written(count);
        self.provider_retry = None;
        self.native_source = None;
        self.cancelled = self.cancelled.filter(|old| *old != id);
    }
    fn fetch(&mut self) {
        let Some(id) = self.current_id().filter(|_| self.active()) else {
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
        let Ok(spec) = self.spec(&offer) else {
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
        self.fetch_requested = None;
        if self.provider_retry == Some(id) {
            self.set_transfer(self.status(id, FileTransferPhase::Failed, Some("File provider unavailable".into())));
            return;
        }
        remote.start_delivery();
    }

    fn progress(&self, id: ClipId, batch: oink_clipboard::store::BatchId, value: content::TransferProgress) {
        // Clearing the current file offer retires its diagnostic record. Native
        // readers may outlive it, but queued progress must not re-admit it.
        if self.provider_retry == Some(id)
            || self.shared.transfer.lock().unwrap().is_none()
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
            .is_some_and(|(clip, r)| *clip == id && r.batch_id() == batch)
        {
            return;
        }
        // Windows never fetches clipboard files ahead of demand (model::automatic_pull
        // admits file roots only for Mac targets): a stored delivery is a paste.
        #[cfg(windows)]
        if matches!(value.phase, content::TransferPhase::Stored) {
            crate::usage_days::mark(crate::usage_days::Feature::Paste);
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
        self.echo_base = None;
        self.identity = None;
        self.cancel_native_write();
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
