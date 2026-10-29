//! Native drag execution, source pins and host delivery lifetimes.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use oink_clipboard::dnd::{DndBackend, DragOperation, DragOps, DragSpec, PlatformDndEvent};
use oink_clipboard::sync::files::TargetOs;
use oink_foundation::DeviceId;
use oink_foundation::clipboard::ClipData;
use oink_foundation::content::ClipId;
use oink_routing::dnd::DND_SEQ_BIT;
use oink_routing::{DndCmd, DndEvent, DndSourceEffect, DndTarget};

use crate::node::{Inner, Node};

/// A drag whose native content originates on this device.
struct LocalDrag {
    _update_activity: Option<oink_foundation::maintenance::Lease>,
    operation: DragOperation,
    exported: bool,
    owned: bool,
    _native: Option<oink_clipboard::content::SourceLease>,
    items: Vec<ClipData>,
}

#[cfg(all(test, debug_assertions))]
mod cancel_tests;
mod files;
mod link;
mod link_platform;
mod preparation;
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
mod preparation_choice;
#[cfg(any(target_os = "windows", all(test, debug_assertions)))]
mod source_release;
pub(crate) mod timing;
#[cfg(debug_assertions)]
mod watchdog;
pub(crate) mod wire;
pub(crate) use files::DndFiles;
pub(crate) use link::{Handle as LinkHandle, start_link_runtime};

pub(crate) struct DndGlue {
    data_dir: std::path::PathBuf,
    runtime: tokio::runtime::Handle,
    target_images: Mutex<HashMap<(ClipId, u64), oink_foundation::dnd::DragImage>>,
    native_targets: Arc<Mutex<HashMap<(ClipId, u64), oink_clipboard::content::Remote>>>,
    me: DeviceId,
    backend: Arc<dyn DndBackend>,
    input_node: Weak<Inner>,
    files: Arc<DndFiles>,
    link_files: Option<Arc<crate::streams::clipboard::files::FileLinks>>,
    input_backend: Option<Arc<dyn oink_input::InputBackend>>,
    local: Mutex<HashMap<ClipId, LocalDrag>>,
    seq: AtomicU64,
    active_operation: Mutex<Option<(DragOperation, ClipId, u64)>>,
    active_target: Mutex<Option<(DragOperation, DndTarget)>>,
    update_targets: Mutex<HashMap<(ClipId, u64), oink_foundation::maintenance::Lease>>,
    #[cfg(target_os = "macos")]
    preparations: Mutex<HashMap<DeviceId, Arc<preparation::Slot>>>,
    pending_begins: Mutex<HashMap<(ClipId, u64), preparation::BeginState>>,
    active_source: Mutex<Option<(ClipId, DragOperation)>>,
    export_epoch: Mutex<Option<(DragOperation, u64)>>,
    source_physical: std::sync::atomic::AtomicBool,
    source_epoch: AtomicU64,
    source_origin: Mutex<Option<(ClipId, oink_foundation::Point)>>,
    source_releases: Mutex<Option<(ClipId, Result<(), String>)>>,
    hold_observations: Mutex<HashMap<DragOperation, (ClipId, bool)>>,
}

impl DndGlue {
    #[cfg(target_os = "macos")]
    fn begin_without_files(&self, spec: DragSpec) -> Result<(), oink_clipboard::ClipboardError> {
        let spec = crate::file_provider::without_files(spec)?.ok_or(oink_clipboard::ClipboardError::Unsupported)?;
        self.backend.begin(spec)
    }
    fn target_preparation_current(&self, id: ClipId, epoch: u64) -> bool {
        let valid = self.active_target.lock().unwrap().as_ref().is_some_and(|(_, grant)| {
            grant.preparation.key.drag.offer.clip == id && grant.holder.epoch == epoch && grant.begin_current()
        });
        valid
            && self.input_node.upgrade().is_some_and(|inner| {
                Node { inner }
                    .input_status()
                    .is_ok_and(|status| !status.paused && status.dnd_host_pending == Some((id, epoch)))
            })
    }
    fn target_ended(&self, id: ClipId, epoch: u64, dropped: bool) {
        self.pending_begins.lock().unwrap().remove(&(id, epoch));
        // Retain admission until deferred delivery has acquired its own lease.
        let _update_activity = self.update_targets.lock().unwrap().remove(&(id, epoch));
        if let Some((_, grant)) = self
            .active_target
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(_, g)| g.preparation.key.drag.offer.clip == id && g.holder.epoch == epoch)
        {
            if dropped {
                grant.delivered();
            } else {
                grant.revoke();
            }
        }
        let remote = self.native_targets.lock().unwrap().get(&(id, epoch)).cloned();
        if let Some(remote) = remote {
            remote.record_drop_accepted(dropped);
            if dropped {
                remote.set_demand(oink_clipboard::model::Demand::Drop(oink_foundation::content::Holder {
                    device: self.me,
                    epoch,
                }));
                remote.start_delivery();
            } else {
                self.native_targets.lock().unwrap().remove(&(id, epoch));
                remote.cancel();
            }
        }
    }
    fn cancel_host(&self, id: ClipId, epoch: u64, physical: bool, released: Option<&dyn Fn()>) -> bool {
        // The ordinary queue may contain cancellation for an already retired host.
        // A new operation must never inherit its Esc or UP.
        let active = *self.active_operation.lock().unwrap();
        if !active.is_some_and(|(_, active, generation)| active == id && generation == epoch) {
            // Preparation may still be waiting on a remote source, without a
            // native operation to cancel. Retire its cached readers as well.
            self.target_ended(id, epoch, false);
            return false;
        }
        self.target_ended(id, epoch, false);
        let ok = self.backend.cancel_physical(&|| {
            self.release_active_target(physical)?;
            // macOS may spend hundreds of milliseconds waiting for native Ended
            // after this successful UP. Keep its reject-window recognition and
            // release ordered, then let ordinary input proceed during that wait.
            // Backends using the default cancel_physical never call this closure
            // and retain their existing full-command fence.
            if let Some(released) = released {
                released();
            }
            Ok(())
        });
        *self.active_operation.lock().unwrap() = None;
        ok
    }
    fn cancel_targets(&self) {
        self.pending_begins.lock().unwrap().clear();
        #[cfg(target_os = "macos")]
        {
            // A Begin waiting for metadata has no native target yet. Pause and
            // shutdown must wake it as well as cancelling activated readers.
            let slots = std::mem::take(&mut *self.preparations.lock().unwrap());
            for slot in slots.into_values() {
                slot.revoke();
            }
        }
        for (_, remote) in self.native_targets.lock().unwrap().drain() {
            remote.cancel();
        }
    }
    fn set_source_anchor(&self, anchor: Option<oink_foundation::dnd::DragAnchor>) -> Result<(), String> {
        if let Some(input) = &self.input_backend {
            input.set_drag_anchor(anchor).map_err(|error| error.to_string())
        } else {
            let inner = self.input_node.upgrade().ok_or_else(|| "service stopped".to_string())?;
            Node { inner }
                .input_backend()
                .set_drag_anchor(anchor)
                .map_err(|error| error.to_string())
        }
    }

    fn source_operation(&self, id: ClipId, epoch: u64) -> Option<DragOperation> {
        let active = self.active_source.lock().unwrap();
        let (_, operation) = active.filter(|(active, _)| *active == id)?;
        if epoch < self.source_epoch.load(Ordering::Acquire) {
            return None;
        }
        self.source_epoch.store(epoch, Ordering::Release);
        Some(operation)
    }
    fn source_release(&self, id: ClipId, physical: bool) -> Result<(), String> {
        if let Some((_, operation)) = self.active_source.lock().unwrap().filter(|(active, _)| *active == id) {
            self.backend.retire_source_image_nudges(operation);
        }
        let mut releases = self.source_releases.lock().unwrap();
        if let Some((released, result)) = &*releases {
            if *released == id {
                return result.clone();
            }
            if released.seq > id.seq {
                return Err("obsolete original-UP command".into());
            }
        }
        #[cfg(target_os = "windows")]
        let result = source_release::at_origin(id, *self.source_origin.lock().unwrap(), |at| {
            if let Some(input) = &self.input_backend {
                input.release_drag_at(at).map_err(|error| error.to_string())
            } else {
                let inner = self.input_node.upgrade().ok_or_else(|| "service stopped".to_string())?;
                Node { inner }
                    .input_backend()
                    .release_drag_at(at)
                    .map_err(|error| error.to_string())
            }
        });
        #[cfg(target_os = "windows")]
        let _ = physical;
        #[cfg(not(target_os = "windows"))]
        let result = self.submit_source_release(physical);
        *releases = Some((id, result.clone()));
        drop(releases);
        if let Some(inner) = self.input_node.upgrade()
            && let Ok(input) = (Node { inner }).input_thread()
        {
            input.send(crate::threads::input::InputCommand::DndEvent(
                DndEvent::SourceButtonReleased { id, ok: result.is_ok() },
            ));
        }
        result
    }
    fn submit_source_release(&self, physical: bool) -> Result<(), String> {
        let trace = oink_foundation::drag_trace::Trace::new(
            oink_foundation::drag_trace::stage::INJECT_BUTTON,
            oink_foundation::drag_trace::Context {
                aux: 1 | (u32::from(physical) << 9),
                ..oink_foundation::drag_trace::Context::current()
            },
        );
        let result: Result<(), String> = trace.scope(|| {
            if physical {
                return self.release_physical_input();
            }
            let event = oink_foundation::input::InjectEvent::Button {
                button: oink_foundation::input::MouseButton::Left,
                pressed: false,
            };
            if let Some(input) = &self.input_backend {
                input.inject(&event).map_err(|e| e.to_string())
            } else {
                let inner = self.input_node.upgrade().ok_or_else(|| "service stopped".to_string())?;
                Node { inner }.input_backend().inject(&event).map_err(|e| e.to_string())
            }
        });
        trace.result(if result.is_ok() { 0 } else { -1 });
        result
    }
    fn release_physical_input(&self) -> Result<(), String> {
        let result = if let Some(input) = &self.input_backend {
            input.release_physical_drag().map_err(|error| error.to_string())
        } else {
            let inner = self.input_node.upgrade().ok_or_else(|| "service stopped".to_string())?;
            Node { inner }
                .input_backend()
                .release_physical_drag()
                .map_err(|error| error.to_string())
        };
        if let Some(inner) = self.input_node.upgrade()
            && let Some(trace) = inner.input_trace.get()
        {
            trace.dnd_native("SourceRelease", None, Some(false), result.is_ok());
        }
        result
    }
    /// Register native source content before notifying the router.
    fn detected(&self, operation: DragOperation, mut items: Vec<ClipData>, _ops: DragOps) -> DndEvent {
        let id = self
            .active_source
            .lock()
            .unwrap()
            .filter(|(_, current)| *current == operation)
            .map(|(id, _)| id)
            .or_else(|| {
                self.local
                    .lock()
                    .unwrap()
                    .iter()
                    .find_map(|(id, drag)| (drag.operation == operation).then_some(*id))
            })
            .unwrap_or_else(|| ClipId {
                origin: self.me,
                seq: DND_SEQ_BIT | self.seq.fetch_add(1, Ordering::AcqRel),
            });
        timing::bind(operation, id);
        if let Some(inner) = self.input_node.upgrade()
            && let Some(diagnostics) = inner.diagnostics.get()
        {
            diagnostics.recorder.dnd_native_operation(id, operation.sequence, false);
        }
        let mut l = self.local.lock().unwrap_or_else(|e| e.into_inner());
        // Unheld probes have no responsibility. Owned offers live until an
        // explicit terminal receipt/cancel, never a recent-N eviction.
        l.retain(|_, drag| drag.owned);
        l.remove(&id);
        let native = self.backend.source_content(operation).and_then(|provider| {
            let provider = oink_clipboard::content::identified(
                provider,
                oink_foundation::content::OfferId {
                    clip: id,
                    generation: id.seq,
                },
            );
            let roots = provider.inventory().roots;
            if !roots.is_empty() && !items.iter().any(|item| matches!(item, ClipData::FileRoots(_))) {
                items.push(ClipData::FileRoots(roots));
            }
            self.files.native.register(provider).ok()
        });
        let image = self.backend.source_image().filter(|image| image.valid());
        l.insert(
            id,
            LocalDrag {
                _update_activity: None,
                operation,
                exported: false,
                owned: self
                    .active_source
                    .lock()
                    .unwrap()
                    .is_some_and(|(active, _)| active == id),
                _native: native,
                items: items.clone(),
            },
        );
        match image {
            Some(image) => DndEvent::DetectedWithImage { id, items, image },
            None => DndEvent::Detected { id, items },
        }
    }

    fn prepare_spec(
        &self,
        id: ClipId,
        epoch: u64,
        at: oink_foundation::Point,
        items: Vec<ClipData>,
        retained_inventory: Option<Vec<u8>>,
    ) -> Result<preparation::PreparedStart, oink_clipboard::model::Unavailable> {
        use oink_clipboard::{content::reason, model::Unavailable};
        let prepared_at = std::time::Instant::now();
        let operation = DragOperation::new();
        let exported = self
            .local
            .lock()
            .unwrap()
            .get(&id)
            .filter(|drag| drag.exported)
            .map(|drag| drag.items.clone());
        if id.origin == self.me && exported.is_none() {
            // Returning to C resumes its native session through ResumeSource.
            // A missing source session is failure, never a copied replacement.
            return Err(Unavailable::Superseded);
        }
        let local_export = exported.is_some();
        let items = exported.unwrap_or(items);
        #[cfg(target_os = "macos")]
        let announced_roots = !local_export && items.iter().any(|i| matches!(i, ClipData::FileRoots(_)));
        let spec = {
            if !local_export && let Some(files) = &self.link_files {
                files.accept(id.origin, id);
                if !files.offer_current(id.origin, id) {
                    return Err(Unavailable::Superseded);
                }
            }
            let mut files: Option<Arc<dyn oink_clipboard::FileContents>> = None;
            let mut items = items;
            // The retained v1 inventory carries the source platform even when
            // a namespace has roots and no native representation descriptors.
            let inventory = wire::native_inventory(
                oink_foundation::content::OfferId {
                    clip: id,
                    generation: id.seq,
                },
                &items,
                retained_inventory.as_deref(),
            )
            .and_then(|wire| Ok((oink_clipboard::wire::local_inventory(&wire)?, wire)));
            let native = if let Ok((inventory, wire_inventory)) = inventory {
                let link: Option<Arc<dyn oink_clipboard::sync::files::BulkLink>> = if local_export {
                    Some(self.files.local(self.me, inventory.id))
                } else {
                    self.link_files
                        .clone()
                        .map(|link| link as Arc<dyn oink_clipboard::sync::files::BulkLink>)
                };
                let link = link.ok_or(Unavailable::Unsupported)?;
                let batch = match oink_clipboard::store::BatchId::random() {
                    Ok(id) => id,
                    Err(error) => {
                        return Err(reason(&error));
                    }
                };
                let target = if cfg!(target_os = "macos") {
                    oink_foundation::clipboard::ClipPlatform::Mac
                } else {
                    oink_foundation::clipboard::ClipPlatform::Windows
                };
                let remote = match oink_clipboard::content::Remote::on_wire_runtime(
                    &self.data_dir,
                    batch,
                    wire_inventory,
                    target,
                    link,
                    oink_clipboard::model::Demand::Hover(oink_foundation::content::Holder { device: self.me, epoch }),
                    self.runtime.clone(),
                ) {
                    Ok(remote) => remote,
                    Err(error) => {
                        return Err(reason(&error));
                    }
                };
                let inventory = oink_clipboard::content::Provider::inventory(&remote);
                if inventory.namespace_roots_for(target).is_empty() {
                    items = oink_clipboard::policy::items_without_files(items);
                    inventory.wanted(target).next().ok_or(Unavailable::Unsupported)?;
                }
                if !inventory.namespace_roots_for(target).is_empty() {
                    let contents = match oink_clipboard::content::namespace::Files::new(
                        Arc::new(remote.clone()),
                        TargetOs::current(),
                    ) {
                        Ok(contents) => contents,
                        Err(error) => {
                            return Err(reason(&error));
                        }
                    };
                    // Native presentation table contains selected roots only. It
                    // is never shared as a flattened source/network manifest.
                    items.push(ClipData::Files(contents.roots().to_vec()));
                    files = Some(contents);
                }
                self.native_targets.lock().unwrap().insert((id, epoch), remote.clone());
                let targets = Arc::downgrade(&self.native_targets);
                let mut progress = remote.progress();
                self.runtime.spawn(async move {
                    loop {
                        use oink_clipboard::content::TransferPhase;
                        if matches!(
                            progress.borrow().phase,
                            TransferPhase::Stored | TransferPhase::Failed | TransferPhase::Cancelled
                        ) {
                            if let Some(targets) = targets.upgrade() {
                                targets.lock().unwrap().remove(&(id, epoch));
                            }
                            break;
                        }
                        if progress.changed().await.is_err() {
                            break;
                        }
                    }
                });
                let entries = items
                    .iter()
                    .filter_map(|item| match item {
                        ClipData::Files(files) => Some(files.as_slice()),
                        _ => None,
                    })
                    .flatten()
                    .cloned()
                    .collect();
                if let Some(inner) = self.input_node.upgrade() {
                    oink_clipboard::received::remember_source(id.origin, (Node { inner }).device_name(id.origin));
                }
                Some(oink_clipboard::NativeSpec {
                    inventory,
                    contents: Arc::new(remote),
                    files: entries,
                    file_contents: files.clone(),
                    sensitivity: oink_clipboard::Sensitivity::Normal,
                })
            } else {
                return Err(Unavailable::InvalidData);
            };
            DragSpec {
                native,
                operation,
                at,
                items,
                files,
                ops: DragOps::COPY,
            }
        };
        // Hover artwork can wait for the live source, just like other native
        // preparation. An Up/leave/cancel during that read cannot start a drag.
        let preview_at = std::time::Instant::now();
        oink_clipboard::dnd::preparation_timing(
            oink_clipboard::dnd::PreparationStage::Metadata,
            prepared_at,
            preview_at,
        );
        self.backend.prepare_preview(&spec);
        oink_clipboard::dnd::preparation_timing(
            oink_clipboard::dnd::PreparationStage::Preview,
            preview_at,
            std::time::Instant::now(),
        );
        Ok(preparation::PreparedStart {
            spec,
            prepared_at,
            #[cfg(target_os = "macos")]
            announced_roots,
            #[cfg(target_os = "macos")]
            fallback: None,
        })
    }

    fn begin(
        &self,
        grant: DndTarget,
        at: oink_foundation::Point,
        items: Vec<ClipData>,
        retained_inventory: Option<Vec<u8>>,
        report: Option<crate::telemetry::Stamp>,
    ) -> Vec<DndEvent> {
        let id = grant.preparation.key.drag.offer.clip;
        let epoch = grant.holder.epoch;
        if !grant.begin_current() {
            self.target_ended(id, epoch, false);
            return vec![DndEvent::BeginFailed { id, epoch }];
        }
        let pending = self.pending_begins.lock().unwrap().remove(&(id, epoch));
        let prepared = match pending {
            Some(preparation::BeginState::Ready(prepared)) => prepared.map(|prepared| *prepared),
            Some(preparation::BeginState::Pending) => None,
            None => self.prepare_spec(id, epoch, at, items, retained_inventory).ok(),
        };
        let Some(preparation::PreparedStart {
            spec,
            prepared_at,
            #[cfg(target_os = "macos")]
            fallback,
            ..
        }) = prepared
        else {
            self.target_ended(id, epoch, false);
            return vec![DndEvent::BeginFailed { id, epoch }];
        };
        let operation = spec.operation;
        *self.active_target.lock().unwrap() = Some((operation, grant.clone()));
        if !self.target_preparation_current(id, epoch) {
            self.target_ended(id, epoch, false);
            self.backend.set_target_image(None);
            return vec![DndEvent::BeginFailed { id, epoch }];
        }
        *self.active_operation.lock().unwrap_or_else(|e| e.into_inner()) = Some((operation, id, epoch));
        if let Some(inner) = self.input_node.upgrade()
            && let Some(diagnostics) = inner.diagnostics.get()
        {
            diagnostics.recorder.dnd_native_operation(id, operation.sequence, true);
        }
        let report_end = std::time::Instant::now();
        crate::telemetry::projections::drag_preparation(
            oink_clipboard::dnd::PreparationStage::Normalization,
            prepared_at,
            report_end,
        );
        tracing::debug!(
            dnd_phase = 10_u64,
            elapsed_us = report_end.saturating_duration_since(prepared_at).as_micros() as u64,
            "drag metadata and lazy readers prepared"
        );
        timing::arm_target(self.backend.as_ref(), operation, grant.entered_at());
        let spec_at = spec.at;
        #[cfg(target_os = "macos")]
        let result = if spec.files.is_some() && self.backend.uses_file_provider() {
            let entries = spec.items.iter().find_map(|item| match item {
                ClipData::Files(entries) => Some(entries),
                _ => None,
            });
            match entries {
                Some(entries) => {
                    let mut started = std::time::Instant::now();
                    let current = || self.target_preparation_current(id, epoch);
                    use crate::diagnostics::Event;
                    use oink_clipboard::provider_preparation::{PreparationPath, PreparationTiming};
                    use oink_clipboard::{ClipboardError, FileProviderReason as Reason};
                    let mut timing = PreparationTiming {
                        registration_start_us: started.duration_since(prepared_at).as_micros() as u64,
                        ..oink_clipboard::file_provider::spare_status()
                    };
                    let items = entries.len() as u64;
                    let bytes = entries.iter().fold(0u64, |sum, e| sum.saturating_add(e.size));
                    oink_clipboard::file_provider::request_files();
                    let known = self
                        .input_node
                        .upgrade()
                        .and_then(|inner| crate::node::lock(&inner.file_provider).snapshot().reason)
                        .filter(|reason| {
                            matches!(
                                reason,
                                Reason::DomainDisabled | Reason::ExtensionNotRegistered | Reason::DomainDisconnected
                            )
                        });
                    let registration = if let Some(reason) = known {
                        Err(ClipboardError::FileProvider(reason))
                    } else if let Some(prepared) = self.prepared(&grant, &current) {
                        prepared.map_err(ClipboardError::FileProvider).and_then(|prepared| {
                            if !current() || !grant.begin_current() {
                                return Err(ClipboardError::FileProvider(Reason::Cancelled));
                            }
                            let namespace = self
                                .native_targets
                                .lock()
                                .unwrap()
                                .get(&(id, epoch))
                                .cloned()
                                .ok_or(ClipboardError::FileProvider(Reason::Cancelled))?;
                            started = prepared.started();
                            timing = prepared.timing();
                            let content_grant = grant.clone();
                            let registration = oink_clipboard::file_provider::activate_prepared(
                                &prepared,
                                oink_clipboard::file_provider::Activation {
                                    key: prepared.key(),
                                    holder: grant.holder,
                                    grant_revision: grant.revision,
                                },
                                Arc::new(namespace),
                                Arc::new(move || content_grant.content_current()),
                            );
                            if registration.is_ok() {
                                self.preparation_activated(&grant);
                            }
                            registration
                        })
                    } else if let Some(fallback) = fallback {
                        started = fallback.started;
                        timing = fallback.timing;
                        fallback.registration
                    } else {
                        // Every File Provider publication must already be ready
                        // on the background preparation worker.
                        Err(ClipboardError::FileProvider(Reason::Cancelled))
                    };
                    if let Some(p) = crate::telemetry::producer()
                        && let Some(stamp) = p.stamp_at(started)
                    {
                        // registration_us is a real interval; the remaining
                        // values are offsets from that origin, never spans.
                        if let Some(end) = started.checked_add(std::time::Duration::from_micros(timing.registration_us))
                        {
                            p.interval(
                                stamp,
                                oink_telemetry::Metric::DragPrepareStage {
                                    stage: oink_telemetry::PrepareStage::Registration,
                                },
                                end,
                            );
                        }
                        for (stage, value) in [
                            (oink_telemetry::PrepareStage::ParentVisible, timing.parent_visible_us),
                            (
                                oink_telemetry::PrepareStage::ChildrenEnumerated,
                                timing.children_enumerated_us,
                            ),
                            (oink_telemetry::PrepareStage::FirstUrl, timing.first_url_us),
                        ] {
                            if let Some(value) = value {
                                crate::telemetry::projections::distribution_value(
                                    p,
                                    stamp.generation,
                                    oink_telemetry::Metric::DragPrepareStage { stage },
                                    value,
                                    None,
                                );
                            }
                        }
                    }
                    tracing::debug!(
                        dnd_phase = 11_u64,
                        elapsed_us = started.elapsed().as_micros() as u64,
                        success = registration.is_ok(),
                        "drag File Provider registration finished"
                    );
                    let mut reason = Reason::UrlsReady;
                    // Both direct registration and Announced timing transfer
                    // select the path actually used, not an available spare.
                    let mut path = preparation_path(&timing);
                    let result = match registration {
                        Ok(registration) if current() => self.backend.begin_file_provider(spec, registration),
                        Ok(registration) => {
                            registration.cancel();
                            reason = Reason::Cancelled;
                            Err(ClipboardError::FileProvider(reason))
                        }
                        Err(e) => {
                            reason = oink_clipboard::file_provider::failure_reason(&e);
                            if current() && reason.needs_enable() {
                                if let Some(inner) = self.input_node.upgrade() {
                                    let notify = crate::node::lock(&inner.file_provider).require_drag(id);
                                    if notify {
                                        Node { inner }.emit(crate::api::ApiEvent::FileProviderRequired);
                                    }
                                }
                                self.begin_without_files(spec)
                            } else {
                                if current()
                                    && let Some(inner) = self.input_node.upgrade()
                                {
                                    tracing::warn!(?reason, "drag file preparation failed");
                                    Node { inner }.emit(crate::api::ApiEvent::DndPreparationFailed(reason.detail()));
                                }
                                Err(e)
                            }
                        }
                    };
                    if result.is_err() {
                        path = PreparationPath::Failed;
                        if reason == Reason::UrlsReady {
                            reason = if current() {
                                Reason::RegistrationFailed
                            } else {
                                Reason::Cancelled
                            };
                        }
                    }
                    if let Some(inner) = self.input_node.upgrade()
                        && let Some(d) = inner.diagnostics.get()
                    {
                        let mut identity = [0u8; 44];
                        identity[..4].copy_from_slice(b"drag");
                        identity[4..36].copy_from_slice(&id.origin.0);
                        identity[36..].copy_from_slice(&id.seq.to_le_bytes());
                        let recorder = d.recorder.clone();
                        let drag = recorder.anonymizer().control_identity(&identity);
                        // AppKit willBegin can follow the native Begin return.
                        // Subscribe without delaying input; the backend resolves
                        // once at willBegin, or with None at cancellation/end.
                        self.backend.observe_native_begin(
                            operation,
                            result.is_ok(),
                            Box::new(move |at| {
                                timing.native_begin_us = at
                                    .and_then(|at| at.checked_duration_since(started))
                                    .map(|elapsed| elapsed.as_micros() as u64);
                                recorder.record(Event::FileProviderPreparation {
                                    drag,
                                    epoch,
                                    path,
                                    reason,
                                    timing,
                                    items,
                                    bytes,
                                });
                            }),
                        );
                    }
                    result
                }
                None => Err(oink_clipboard::ClipboardError::Unsupported),
            }
        } else {
            self.backend.begin(spec)
        };
        #[cfg(not(target_os = "macos"))]
        let result = self.backend.begin(spec);
        if result.is_err() {
            self.backend.target_start_timing(operation, None);
        }
        if let Some(stamp) = report
            && let Some(p) = crate::telemetry::producer()
        {
            if result.is_ok() {
                p.interval(
                    stamp,
                    oink_telemetry::Metric::DragStart {
                        boundary: oink_telemetry::StartBoundary::ApiBeginAccepted,
                    },
                    std::time::Instant::now(),
                );
            }
            self.backend.observe_native_timing(
                operation,
                result.is_ok(),
                oink_clipboard::dnd::NativeBeginTiming {
                    generation: stamp.generation,
                    queued_at: stamp.at,
                    record: crate::telemetry::projections::drag_native_begin,
                },
            );
        }
        if let Some(inner) = self.input_node.upgrade()
            && let Some(trace) = inner.input_trace.get()
        {
            trace.dnd_native("BeginResult", Some(spec_at), None, result.is_ok());
        }
        match result {
            Ok(()) => {
                let image = self.target_images.lock().unwrap().remove(&(id, epoch));
                if let Some(image) = image {
                    self.backend.update_target_image(operation, image);
                }
                Vec::new()
            }
            Err(e) => {
                self.target_ended(id, epoch, false);
                let mut active = self.active_operation.lock().unwrap_or_else(|e| e.into_inner());
                if *active == Some((operation, id, epoch)) {
                    *active = None;
                }
                tracing::warn!(%e, ?id, "starting the drag failed");
                vec![DndEvent::BeginFailed { id, epoch }]
            }
        }
    }
}

impl DndGlue {
    fn exported(
        &self,
        operation: DragOperation,
        result: Result<(), oink_foundation::dnd::ExportFailure>,
    ) -> Option<DndEvent> {
        let (id, _) = self
            .active_source
            .lock()
            .unwrap()
            .filter(|(_, current)| *current == operation)?;
        let (_, epoch) = self
            .export_epoch
            .lock()
            .unwrap()
            .take()
            .filter(|(current, _)| *current == operation)?;
        let mut local = self.local.lock().unwrap();
        let drag = local.get_mut(&id)?;
        let items = if result.is_ok() {
            drag._native = None;
            self.backend.source_content(operation).and_then(|provider| {
                let provider = oink_clipboard::content::identified(
                    provider,
                    oink_foundation::content::OfferId {
                        clip: id,
                        generation: id.seq,
                    },
                );
                let items = oink_clipboard::content::items(&provider.inventory());
                drag._native = self.files.native.register(provider).ok();
                drag.exported = drag._native.is_some();
                drag.items = items.clone();
                drag.exported.then_some(items)
            })
        } else {
            None
        };
        Some(DndEvent::SourceExported {
            id,
            epoch,
            items: items.ok_or_else(|| result.err().unwrap_or(oink_foundation::dnd::ExportFailure::Failed)),
        })
    }
    fn execute_reported(
        &self,
        cmd: DndCmd,
        report: Option<crate::telemetry::Stamp>,
        released: Option<&dyn Fn()>,
    ) -> Vec<DndEvent> {
        let admission = if matches!(
            &cmd,
            DndCmd::Prepare { .. }
                | DndCmd::Begin { .. }
                | DndCmd::Probe { .. }
                | DndCmd::HoldSource { .. }
                | DndCmd::ExportSource { .. }
        ) {
            let Some(lease) =
                oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::Drag)
            else {
                return Vec::new();
            };
            Some(lease)
        } else {
            None
        };
        tracing::debug!(?cmd, "dnd command");
        match cmd {
            DndCmd::Integrity { id, record } => {
                if let Some(inner) = self.input_node.upgrade()
                    && let Some(diagnostics) = inner.diagnostics.get()
                {
                    diagnostics.recorder.dnd_integrity(id, record);
                }
                Vec::new()
            }
            DndCmd::ExportFailed { failure } => {
                tracing::warn!(?failure, "drag file export failed");
                if let Some(inner) = self.input_node.upgrade() {
                    Node { inner }.emit(crate::api::ApiEvent::DndExportFailed(failure));
                }
                Vec::new()
            }
            DndCmd::Prepare { preparation, roots } => {
                self.prepare(preparation, roots);
                Vec::new()
            }
            DndCmd::RevokePreparation { preparation } => {
                self.revoke_preparation(preparation);
                Vec::new()
            }
            DndCmd::PrepareDrop { id, epoch } => {
                if let Some(remote) = self.native_targets.lock().unwrap().get(&(id, epoch)) {
                    remote.record_drop_start();
                    remote.set_demand(oink_clipboard::model::Demand::Drop(oink_foundation::content::Holder {
                        device: self.me,
                        epoch,
                    }));
                }
                let grant = self
                    .active_target
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|(_, g)| g.preparation.key.drag.offer.clip == id && g.holder.epoch == epoch)
                    .map(|(operation, g)| (*operation, g.clone()));
                if let Some((operation, grant)) = grant {
                    self.backend.observe_target_drop(operation);
                    if !self.release_target(&grant, false) {
                        return vec![DndEvent::HostReleaseFailed { id, epoch }];
                    }
                }
                Vec::new()
            }
            DndCmd::HoldSource {
                origin,
                id,
                epoch,
                physical,
                target,
                crossing,
            } => {
                // Keep the original press independently of native authority. A lost
                // operation must never turn cleanup into an UP at the live cursor.
                let mut recorded = self.source_origin.lock().unwrap();
                if recorded.as_ref().is_none_or(|(previous, _)| previous.seq < id.seq) {
                    *recorded = origin.map(|at| (id, at));
                }
                drop(recorded);
                let operation = self.local.lock().unwrap().get_mut(&id).map(|drag| {
                    drag._update_activity = admission;
                    drag.owned = true;
                    drag.operation
                });
                let Some(operation) = operation else {
                    return vec![DndEvent::SourceHeld { id, epoch, ok: false }];
                };
                if self
                    .active_source
                    .lock()
                    .unwrap()
                    .is_some_and(|(active, _)| active == id)
                    && epoch < self.source_epoch.load(Ordering::Acquire)
                {
                    return Vec::new();
                }
                *self.active_source.lock().unwrap() = Some((id, operation));
                self.source_epoch.store(epoch, Ordering::Release);
                self.source_physical.store(physical, Ordering::Release);
                if cfg!(target_os = "macos") {
                    self.hold_observations.lock().unwrap().insert(operation, (id, physical));
                }
                let held = self
                    .backend
                    .hold_source(operation)
                    .map_err(|error| {
                        tracing::warn!(?operation, %error, dnd_hold_stage = 1_u64, "native source hold failed");
                        DndSourceEffect::Hold
                    })
                    .and_then(|anchor| {
                        if self.backend.source_hold_installs_anchor() {
                            return Ok(());
                        }
                        self.set_source_anchor(Some(anchor)).map_err(|error| {
                            tracing::warn!(?operation, %error, dnd_hold_stage = 2_u64, "source input anchor failed");
                            DndSourceEffect::Anchor
                        })
                    });
                if let Err(effect) = held {
                    let resumed = self.backend.resume_source(operation);
                    let cleared = self.set_source_anchor(None).is_ok();
                    *self.active_source.lock().unwrap() = None;
                    if effect == DndSourceEffect::Hold || !resumed {
                        self.hold_observations.lock().unwrap().remove(&operation);
                    }
                    return vec![DndEvent::SourceEffectFailed {
                        id,
                        epoch,
                        effect: if cleared { effect } else { DndSourceEffect::Anchor },
                    }];
                }
                self.backend.source_hold_ready(operation);
                if self
                    .backend
                    .source_requires_export(operation, target == oink_routing::Os::Windows)
                {
                    *self.export_epoch.lock().unwrap() = Some((operation, epoch));
                    if self
                        .backend
                        .export_source(operation, crossing, &|| self.source_release(id, physical))
                    {
                        return Vec::new();
                    }
                    self.backend
                        .finish_source(operation, false, &|| self.source_release(id, physical));
                    return vec![DndEvent::SourceHeld { id, epoch, ok: false }];
                }
                vec![DndEvent::SourceHeld { id, epoch, ok: true }]
            }
            DndCmd::ExportSource { id, epoch, physical } => {
                let Some(operation) = self.source_operation(id, epoch) else {
                    return vec![DndEvent::SourceExported {
                        id,
                        epoch,
                        items: Err(oink_foundation::dnd::ExportFailure::Failed),
                    }];
                };
                *self.export_epoch.lock().unwrap() = Some((operation, epoch));
                if self.backend.source_requires_export(operation, true)
                    && self.backend.export_source(operation, std::time::Instant::now(), &|| {
                        self.source_release(id, physical)
                    })
                {
                    Vec::new()
                } else {
                    vec![DndEvent::SourceExported {
                        id,
                        epoch,
                        items: Err(oink_foundation::dnd::ExportFailure::Failed),
                    }]
                }
            }
            DndCmd::ResumeSource { id, epoch, at } => {
                let operation = self.source_operation(id, epoch);
                let ok = operation.is_some_and(|operation| self.backend.resume_source_at(operation, at));
                if ok {
                    let _ = self.set_source_anchor(None);
                }
                if ok {
                    vec![DndEvent::SourceResumed { id, epoch }]
                } else {
                    vec![DndEvent::SourceEffectFailed {
                        id,
                        epoch,
                        effect: DndSourceEffect::Resume,
                    }]
                }
            }
            DndCmd::ReleaseSourceButton { id, physical } => {
                let _ = self.source_release(id, physical);
                Vec::new()
            }
            DndCmd::FinishSource {
                id,
                epoch,
                accept,
                physical,
                cancel_at,
                cancel_geometry,
            } => {
                if self
                    .active_source
                    .lock()
                    .unwrap()
                    .is_some_and(|(active, _)| active == id)
                    && epoch < self.source_epoch.load(Ordering::Acquire)
                {
                    return Vec::new();
                }
                let operation = self.source_operation(id, epoch);
                let Some(operation) = operation else {
                    let _ = self.source_release(id, physical);
                    return vec![DndEvent::SourceEffectFailed {
                        id,
                        epoch,
                        effect: DndSourceEffect::Finish,
                    }];
                };
                if let Some(geometry) = cancel_geometry {
                    self.backend.observe_source_cancel_geometry(operation, geometry);
                }
                let ok = self.backend.finish_source_at(
                    operation,
                    accept,
                    cancel_at,
                    &|at| {
                        let input = self
                            .input_backend
                            .clone()
                            .or_else(|| {
                                self.input_node
                                    .upgrade()
                                    .map(|inner| Node { inner }.input_backend().clone())
                            })
                            .ok_or_else(|| "input backend unavailable".to_string())?;
                        input
                            .move_held_drag_observed(operation.sequence, at, &|posted| {
                                self.backend.observe_source_cancel_posted(operation, posted);
                            })
                            .map_err(|e| e.to_string())
                    },
                    &|| self.source_release(id, physical),
                );
                if ok && !self.backend.source_completion_is_async() {
                    vec![DndEvent::SourceEnded { id, accepted: accept }]
                } else {
                    if !ok {
                        let _ = self.source_release(id, physical);
                        tracing::warn!(accepted = accept, "native source end is unconfirmed");
                        return vec![DndEvent::SourceEffectFailed {
                            id,
                            epoch,
                            effect: DndSourceEffect::Finish,
                        }];
                    }
                    Vec::new()
                }
            }
            DndCmd::ReleaseData { id } => {
                if let Some(source) = self.local.lock().unwrap().remove(&id) {
                    if let Some(native) = &source._native {
                        native.cancel();
                    }
                    self.backend.forget_content(source.operation);
                }
                Vec::new()
            }
            DndCmd::ForgetSource { id } => {
                let operation = {
                    let mut active = self.active_source.lock().unwrap();
                    if active.is_some_and(|(active, _)| active == id) {
                        active.take().map(|(_, operation)| operation)
                    } else {
                        None
                    }
                };
                if let Some(operation) = operation {
                    *self.export_epoch.lock().unwrap() = None;
                    self.backend.unprobe_source(operation);
                    let _ = self.set_source_anchor(None);
                }
                Vec::new()
            }
            DndCmd::UpdateImage { id, epoch, image } => {
                // The action worker owns this cache. Early images never delay
                // Begin and are applied after the native session exists.
                self.target_images
                    .lock()
                    .unwrap()
                    .retain(|(found, generation), _| *found == id && *generation == epoch);
                self.target_images.lock().unwrap().insert((id, epoch), image.clone());
                let operation = self
                    .active_operation
                    .lock()
                    .unwrap()
                    .filter(|(_, active, generation)| *active == id && *generation == epoch)
                    .map(|(operation, _, _)| operation);
                if let Some(operation) = operation {
                    self.backend.update_target_image(operation, image);
                }
                Vec::new()
            }
            DndCmd::SetImage(image) => {
                self.backend.set_target_image(image);
                Vec::new()
            }
            DndCmd::Probe { at } => {
                self.backend.probe(at);
                Vec::new()
            }
            DndCmd::Unprobe => {
                self.backend.unprobe();
                let mut local = self.local.lock().unwrap();
                local.retain(|_, drag| drag.owned);
                Vec::new()
            }
            DndCmd::LeaveHost { id, epoch, physical } => {
                let ok = self.cancel_host(id, epoch, physical, released);
                vec![DndEvent::HostLeft { id, epoch, ok }]
            }
            DndCmd::Cancel { id, epoch, physical } => {
                self.cancel_host(id, epoch, physical, released);
                Vec::new()
            }
            DndCmd::Begin {
                grant,
                id,
                epoch,
                at,
                items,
                retained_inventory,
            } => {
                self.update_targets
                    .lock()
                    .unwrap()
                    .insert((id, epoch), admission.expect("begin admission"));
                let events = self.begin(grant, at, items, retained_inventory, report);
                if !self
                    .active_operation
                    .lock()
                    .unwrap()
                    .is_some_and(|(_, current, e)| current == id && e == epoch)
                {
                    self.update_targets.lock().unwrap().remove(&(id, epoch));
                }
                events
            }
        }
    }
}

/// 启动拖放：平台准备好之后（macOS 要等界面的事件循环开始运行）把执行者接到服务上，路由随之通告对端。
/// 拖放序号的起点：启动时的 Unix 毫秒 × 1024（重启后不与上次的重复，接收端的缓存不会误认）。
fn dnd_seq_base() -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    (ms << 10) & !DND_SEQ_BIT
}

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn preparation_path(
    timing: &oink_clipboard::provider_preparation::PreparationTiming,
) -> oink_clipboard::provider_preparation::PreparationPath {
    use oink_clipboard::provider_preparation::PreparationPath;
    if timing.placeholder.used {
        PreparationPath::Placeholder
    } else if timing.parent_visible_us == Some(0) {
        PreparationPath::Spare
    } else {
        PreparationPath::NewParent
    }
}

#[cfg(debug_assertions)]
#[test]
fn preparation_path_uses_the_consumed_namespace_for_direct_and_announced_timing() {
    use oink_clipboard::provider_preparation::{PlaceholderFallback, PreparationPath, PreparationTiming};
    let mut timing = PreparationTiming {
        spare_ready: true,
        ..Default::default()
    };
    assert_eq!(preparation_path(&timing), PreparationPath::NewParent);
    timing.parent_visible_us = Some(0);
    assert_eq!(preparation_path(&timing), PreparationPath::Spare);
    timing.placeholder.enabled = true;
    timing.placeholder.used = true;
    assert_eq!(preparation_path(&timing), PreparationPath::Placeholder);
    // A failed placeholder attempt may retain its slot counts while the actual final
    // namespace comes from the ordinary path. Availability is not consumption.
    timing.placeholder.used = false;
    timing.placeholder.prepared_files = 5;
    timing.placeholder.fallback = PlaceholderFallback::PublicationFailed;
    assert_eq!(preparation_path(&timing), PreparationPath::Spare);
    timing.parent_visible_us = Some(120);
    assert_eq!(preparation_path(&timing), PreparationPath::NewParent);
}

#[cfg(all(test, debug_assertions))]
pub(crate) fn test_target(id: ClipId, epoch: u64) -> oink_routing::DndTarget {
    use oink_foundation::content::OfferId;
    use oink_routing::dnd::{DragKey, PreparationKey};
    oink_routing::DndTarget::new(
        oink_routing::DndPreparation::new(
            PreparationKey {
                drag: DragKey {
                    offer: OfferId {
                        clip: id,
                        generation: id.seq,
                    },
                    native_generation: id.seq,
                },
                metadata_revision: 1,
            },
            1,
        ),
        oink_foundation::content::Holder {
            device: DeviceId([2; 32]),
            epoch,
        },
        1,
        id.origin,
    )
}
