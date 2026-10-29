use super::*;
use oink_clipboard::model::FileNode;
use oink_routing::DndPreparation;

pub(super) struct PreparedStart {
    pub spec: DragSpec,
    pub prepared_at: std::time::Instant,
    #[cfg(target_os = "macos")]
    pub announced_roots: bool,
    #[cfg(target_os = "macos")]
    pub fallback: Option<Fallback>,
}
#[cfg(target_os = "macos")]
pub(super) struct Fallback {
    pub registration: Result<oink_clipboard::file_provider::Registration, oink_clipboard::ClipboardError>,
    pub started: std::time::Instant,
    pub timing: oink_clipboard::provider_preparation::PreparationTiming,
}
pub(super) enum BeginState {
    Pending,
    Ready(Option<Box<PreparedStart>>),
}

#[cfg(target_os = "macos")]
pub(super) struct Slot {
    _update_activity: Option<oink_foundation::maintenance::Lease>,
    token: DndPreparation,
    roots: Vec<FileNode>,
    cancellation: oink_clipboard::file_provider::PreparationCancellation,
    choice: super::preparation_choice::Choice<
        Result<Arc<oink_clipboard::file_provider::PreparedDrag>, oink_clipboard::FileProviderReason>,
    >,
}

#[derive(Debug, PartialEq, Eq)]
struct Failure {
    stage: u8,
    reason: oink_clipboard::model::Unavailable,
    timed_out: bool,
}
async fn wait_prepared<T>(
    deadline: std::time::Instant,
    ready: tokio::sync::oneshot::Receiver<Result<T, oink_clipboard::model::Unavailable>>,
    stage: &std::sync::atomic::AtomicU8,
) -> Result<T, Failure> {
    use oink_clipboard::model::Unavailable;
    let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), ready).await;
    let timed_out = result.is_err() || tokio::time::Instant::now() >= tokio::time::Instant::from_std(deadline);
    let reason = if timed_out {
        Unavailable::NoProgress
    } else {
        match result {
            Ok(Ok(Ok(value))) => return Ok(value),
            Ok(Ok(Err(reason))) => reason,
            _ => Unavailable::ReadFailed,
        }
    };
    Err(Failure {
        stage: stage.load(Ordering::Acquire),
        reason,
        timed_out,
    })
}

impl DndGlue {
    /// Source preparation is not an input operation. Only the eventual native
    /// Begin belongs behind the injection fence. In particular a slow provider
    /// must not retain the fence while Up/Cancel is queued behind it.
    pub(super) fn defer_begin(
        self: &Arc<Self>,
        command: &DndCmd,
        input: Arc<crate::threads::input::InputThread>,
    ) -> bool {
        let DndCmd::Begin { grant, id, epoch, .. } = command else {
            return false;
        };
        if !grant.begin_current() {
            return false;
        }
        {
            let mut starts = self.pending_begins.lock().unwrap();
            match starts.get(&(*id, *epoch)) {
                Some(BeginState::Ready(_)) => return false,
                Some(BeginState::Pending) => return true,
                None => {
                    starts.insert((*id, *epoch), BeginState::Pending);
                }
            }
        }
        let deferred_at = std::time::Instant::now();
        if let Some(entered) = grant.entered_at() {
            crate::telemetry::projections::drag_target_stage(
                oink_telemetry::PrepareStage::EntryToBegin,
                entered,
                deferred_at,
            );
        }
        let glue = self.clone();
        let command = command.clone();
        let stage = Arc::new(std::sync::atomic::AtomicU8::new(1));
        let worker_stage = stage.clone();
        let (done, ready) = tokio::sync::oneshot::channel();
        let worker_glue = glue.clone();
        let worker_command = command.clone();
        self.runtime.spawn_blocking(move || {
            let worker_at = std::time::Instant::now();
            crate::telemetry::projections::drag_target_stage(
                oink_telemetry::PrepareStage::PreparationQueue,
                deferred_at,
                worker_at,
            );
            let DndCmd::Begin {
                ref grant,
                id,
                epoch,
                at,
                ref items,
                ref retained_inventory,
            } = worker_command
            else {
                unreachable!()
            };
            let current = || worker_glue.begin_pending_current(grant, id, epoch);
            #[cfg(target_os = "macos")]
            let _ = worker_glue.prepared(grant, &current);
            worker_stage.store(2, Ordering::Release);
            let prepared = if current() {
                worker_glue
                    .prepare_spec(id, epoch, at, items.clone(), retained_inventory.clone())
                    .and_then(|prepared| {
                        if prepared.spec.files.is_some() {
                            worker_stage.store(3, Ordering::Release);
                            let remote = worker_glue
                                .native_targets
                                .lock()
                                .unwrap()
                                .get(&(id, epoch))
                                .cloned()
                                .ok_or(oink_clipboard::model::Unavailable::Cancelled)?;
                            let namespace_at = std::time::Instant::now();
                            let result = oink_clipboard::content::namespace::Namespace::prepare(&remote);
                            oink_clipboard::dnd::preparation_timing(
                                oink_clipboard::dnd::PreparationStage::Namespace,
                                namespace_at,
                                std::time::Instant::now(),
                            );
                            result.map_err(|error| oink_clipboard::content::reason(&error))?;
                        }
                        Ok(prepared)
                    })
            } else {
                Err(oink_clipboard::model::Unavailable::Cancelled)
            };
            #[cfg(target_os = "macos")]
            let prepared = prepared.map(|mut prepared| {
                if current()
                    && worker_glue.backend.uses_file_provider()
                    && !prepared.announced_roots
                    && worker_glue
                        .preparations
                        .lock()
                        .unwrap()
                        .get(&id.origin)
                        .is_none_or(|slot| slot.token != grant.preparation)
                    && let Some(contents) = prepared.spec.files.clone()
                {
                    // Retained/local copies without an announcement still need
                    // OS URLs. Their fallback belongs to this same background
                    // budget, never the ordered/native Begin callback.
                    worker_stage.store(1, Ordering::Release);
                    let entries = prepared
                        .spec
                        .items
                        .iter()
                        .find_map(|item| match item {
                            ClipData::Files(entries) => Some(entries),
                            _ => None,
                        })
                        .cloned()
                        .unwrap_or_default();
                    let started = std::time::Instant::now();
                    let mut timing = oink_clipboard::file_provider::spare_status();
                    let registration = oink_clipboard::file_provider::register_drag_measured(
                        id,
                        epoch,
                        &mut timing,
                        started,
                        &entries,
                        contents,
                        &current,
                    );
                    prepared.fallback = Some(Fallback {
                        registration,
                        started,
                        timing,
                    });
                }
                prepared
            });
            crate::telemetry::projections::drag_target_stage(
                oink_telemetry::PrepareStage::TargetPreparation,
                worker_at,
                std::time::Instant::now(),
            );
            if done.send(prepared).is_err() {
                // A timed-out worker may finish after its native readers were retired.
                worker_glue.target_ended(id, epoch, false);
            }
        });
        let deadline = grant.preparation_deadline();
        self.runtime.spawn(async move {
            let DndCmd::Begin {
                ref grant, id, epoch, ..
            } = command
            else {
                unreachable!()
            };
            let grant = grant.clone();
            // User decision (2026-10-27): cross-device preparation gets only 3s
            // after target entry. Native callbacks must not wait on networking
            // or File Provider. Drop-time byte transfer keeps its own lifetime.
            let prepared = wait_prepared(deadline, ready, &stage).await;
            let failure = prepared.as_ref().err();
            if let Some(failure) = failure {
                tracing::warn!(target: "oink_agent::dnd",
                    dnd_phase = 55_u64, file_offer = id.seq, epoch,
                    reason = ?failure.reason, dnd_preparation_stage = failure.stage,
                    dnd_preparation_timeout = failure.timed_out,
                    "cross-device drag preparation failed");
            }
            let failed = prepared.is_err();
            let prepared = prepared.ok();
            let glue = glue.clone();
            glue.runtime.clone().spawn_blocking(move || {
                if failed && glue.begin_pending_current(&grant, id, epoch) {
                    grant.revoke();
                    glue.revoke_preparation(grant.preparation.clone());
                    glue.target_ended(id, epoch, false);
                    input.send(crate::threads::input::InputCommand::DndEvent(DndEvent::BeginFailed {
                        id,
                        epoch,
                    }));
                    return;
                }
                let mut starts = glue.pending_begins.lock().unwrap();
                if glue.begin_pending_current(&grant, id, epoch) && starts.contains_key(&(id, epoch)) {
                    starts.insert((id, epoch), BeginState::Ready(prepared.map(Box::new)));
                    drop(starts);
                    input.send(crate::threads::input::InputCommand::DndPrepared(command));
                } else {
                    drop(starts);
                    glue.target_ended(id, epoch, false);
                    input.send(crate::threads::input::InputCommand::DndEvent(DndEvent::BeginFailed {
                        id,
                        epoch,
                    }));
                }
            });
        });
        true
    }
    fn begin_pending_current(&self, grant: &DndTarget, id: ClipId, epoch: u64) -> bool {
        grant.begin_current()
            && self.input_node.upgrade().is_some_and(|inner| {
                Node { inner }
                    .input_status()
                    .is_ok_and(|status| !status.paused && status.dnd_host_pending == Some((id, epoch)))
            })
    }
    pub(super) fn release_target(&self, grant: &DndTarget, physical: bool) -> bool {
        if !grant.claim_up() {
            return true;
        }
        let result = self.submit_source_release(physical);
        if let Some(inner) = self.input_node.upgrade()
            && let Some(trace) = inner.input_trace.get()
        {
            trace.dnd_native("LeftButton", None, Some(false), result.is_ok());
        }
        result.is_ok()
    }
    pub(super) fn release_active_target(&self, physical: bool) -> Result<(), String> {
        let grant = self.active_target.lock().unwrap().as_ref().map(|(_, g)| g.clone());
        if grant.is_none_or(|grant| self.release_target(&grant, physical)) {
            Ok(())
        } else {
            Err("target UP submission unconfirmed".into())
        }
    }
    pub(super) fn prepare(&self, token: DndPreparation, roots: Vec<FileNode>) {
        #[cfg(not(target_os = "macos"))]
        let _ = (token, roots);
        #[cfg(target_os = "macos")]
        {
            if !self.backend.uses_file_provider() || !token.current() {
                return;
            }
            let mut preparations = self.preparations.lock().unwrap();
            let source = token.key.drag.offer.clip.origin;
            if let Some(inner) = self.input_node.upgrade() {
                oink_clipboard::received::remember_source(source, (crate::node::Node { inner }).device_name(source));
            }
            if preparations.get(&source).is_some_and(|slot| slot.token == token) {
                return;
            }
            if let Some(old) = preparations.remove(&source) {
                old.revoke();
            }
            let Some(activity) =
                oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::Drag)
            else {
                return;
            };
            let slot = Arc::new(Slot::new(token, roots, Some(activity)));
            preparations.insert(source, slot.clone());
            self.runtime.spawn_blocking(move || {
                let result = oink_clipboard::file_provider::prepare_announced_with_cancel(
                    fp_key(&slot.token),
                    slot.roots.clone(),
                    &slot.cancellation,
                    &|| slot.token.current(),
                )
                .map(Arc::new)
                .map_err(|e| oink_clipboard::file_provider::failure_reason(&e));
                if !slot.token.current()
                    && let Ok(prepared) = &result
                {
                    oink_clipboard::file_provider::revoke_prepared(prepared);
                }
                slot.choice.complete(result);
            });
        }
    }
    pub(super) fn revoke_preparation(&self, token: DndPreparation) {
        token.revoke();
        #[cfg(target_os = "macos")]
        {
            let mut slots = self.preparations.lock().unwrap();
            let source = token.key.drag.offer.clip.origin;
            if slots.get(&source).is_some_and(|slot| slot.token == token)
                && let Some(slot) = slots.remove(&source)
            {
                slot.revoke();
            }
        }
    }
    #[cfg(target_os = "macos")]
    pub(super) fn preparation_activated(&self, grant: &DndTarget) {
        let mut slots = self.preparations.lock().unwrap();
        let source = grant.preparation.key.drag.offer.clip.origin;
        if slots.get(&source).is_some_and(|slot| slot.token == grant.preparation) {
            // Registration/readers now own the activated namespace. Dropping
            // this handle does not revoke late reads after a legitimate Drop.
            slots.remove(&source);
        }
    }
    #[cfg(target_os = "macos")]
    pub(super) fn prepared(
        &self,
        grant: &DndTarget,
        current: &dyn Fn() -> bool,
    ) -> Option<Result<Arc<oink_clipboard::file_provider::PreparedDrag>, oink_clipboard::FileProviderReason>> {
        let slot = self
            .preparations
            .lock()
            .unwrap()
            .get(&grant.preparation.key.drag.offer.clip.origin)
            .cloned()?;
        if slot.token != grant.preparation {
            return None;
        }
        let current = || grant.begin_current() && current();
        if !current() {
            return Some(Err(oink_clipboard::FileProviderReason::Cancelled));
        }
        Some(slot.choice.select(|| {
            let slot = slot.clone();
            let grant = grant.clone();
            let input_node = self.input_node.clone();
            // macOS 14/26 can take seconds to reconcile typed slot renames.
            // Keep the existing Announced publication running: its final URLs
            // may arrive first. Both workers have separate namespace identities;
            // only the first result can be activated, and the loser is retired.
            // This is preparation only; Begin and motion retain their own gates.
            self.runtime.spawn_blocking(move || {
                let current = || {
                    grant.begin_current()
                        && slot.choice.pending()
                        && input_node.upgrade().is_some_and(|inner| {
                            Node { inner }.input_status().is_ok_and(|status| {
                                !status.paused
                                    && status.dnd_host_pending
                                        == Some((grant.preparation.key.drag.offer.clip, grant.holder.epoch))
                            })
                        })
                };
                let result = oink_clipboard::file_provider::try_prepare_enter_placeholder(
                    oink_clipboard::file_provider::Activation {
                        key: fp_key(&slot.token),
                        holder: grant.holder,
                        grant_revision: grant.revision,
                    },
                    slot.roots.clone(),
                    &current,
                );
                let result = match result {
                    Ok(Some(prepared)) => Ok(Arc::new(prepared)),
                    Ok(None) => return,
                    Err(error) => Err(oink_clipboard::file_provider::failure_reason(&error)),
                };
                if slot.choice.complete(result) {
                    // Cancellation stops only the losing metadata helper; it
                    // never revokes the selected namespace or its content grant.
                    slot.cancellation.cancel();
                }
            });
        }))
    }
}
#[cfg(target_os = "macos")]
fn fp_key(token: &DndPreparation) -> oink_clipboard::file_provider::PreparationKey {
    oink_clipboard::file_provider::PreparationKey {
        offer: token.key.drag.offer,
        native_generation: token.key.drag.native_generation,
        source_session: token.source_session,
        metadata_revision: token.key.metadata_revision,
    }
}
#[cfg(target_os = "macos")]
impl Slot {
    fn new(token: DndPreparation, roots: Vec<FileNode>, activity: Option<oink_foundation::maintenance::Lease>) -> Self {
        Self {
            _update_activity: activity,
            token,
            roots,
            cancellation: oink_clipboard::file_provider::PreparationCancellation::new(),
            choice: super::preparation_choice::Choice::new(),
        }
    }
    pub(super) fn revoke(&self) {
        self.token.revoke();
        // Wake a deferred Begin even if the native helper has not returned.
        self.choice.complete(Err(oink_clipboard::FileProviderReason::Cancelled));
        self.cancellation.cancel();
        self.choice.with_result(|result| {
            if let Ok(prepared) = result {
                oink_clipboard::file_provider::revoke_prepared(prepared);
            }
        });
    }
}

#[cfg(all(debug_assertions, all(test, target_os = "macos")))]
mod tests {
    use super::*;
    use oink_foundation::content::OfferId;
    use oink_routing::dnd::{DragKey, PreparationKey};

    fn token(revision: u64) -> DndPreparation {
        DndPreparation::new(
            PreparationKey {
                drag: DragKey {
                    offer: OfferId {
                        clip: ClipId {
                            origin: DeviceId([1; 32]),
                            seq: 1,
                        },
                        generation: 1,
                    },
                    native_generation: 1,
                },
                metadata_revision: revision,
            },
            1,
        )
    }

    #[test]
    fn pending_publication_does_not_fence_input_or_native_cancellation() {
        crate::test_watchdog::run(
            module_path!(),
            "pending_publication_does_not_fence_input_or_native_cancellation",
            Some(2),
            async {
                use crate::threads::inject::{InjectItem, start_with_backend};
                use oink_clipboard::dnd::{DndSink, Injector};
                use oink_foundation::input::{InjectEvent, MouseButton};
                struct Backend(std::sync::mpsc::Sender<()>);
                impl DndBackend for Backend {
                    fn release(&self) -> bool {
                        true
                    }
                    fn start(&self, _: DndSink, _: Injector) -> Result<(), oink_clipboard::ClipboardError> {
                        Ok(())
                    }
                    fn probe(&self, _: oink_foundation::Point) {
                        let _ = self.0.send(());
                    }
                    fn unprobe(&self) {}
                    fn begin(&self, _: DragSpec) -> Result<(), oink_clipboard::ClipboardError> {
                        panic!("retired preparation started a native drag")
                    }
                    fn cancel(&self) -> bool {
                        true
                    }
                    fn uses_file_provider(&self) -> bool {
                        true
                    }
                }
                let root = tempfile::tempdir().unwrap();
                let native = Arc::new(oink_test_support::input::FakeInput::new());
                let node = Node::start(crate::node::NodeOptions {
                    net: oink_test_support::network_config(root.path().into(), "drag-prepare", 0),
                    input: native.clone(),
                    displays: vec![],
                    display_backend: None,
                    clipboard: None,
                    mode_switch_in_callback: true,
                    capture_reports_key_repeat: false,
                    cursor_trail: false,
                })
                .await
                .unwrap();
                let (probed, probe) = std::sync::mpsc::channel();
                node.start_dnd_backend(Box::new(Backend(probed))).unwrap();
                let handle = node.inner.dnd.get().unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !node.input_thread().unwrap().opening_state(true).iter().any(|message| {
                        matches!(
                            message,
                            oink_routing::Packet::Dnd(oink_routing::dnd::DndMsg::Caps { enabled: true })
                        )
                    }) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let glue = handle.glue.lock().unwrap().clone().unwrap();
                let id = ClipId {
                    origin: DeviceId([1; 32]),
                    seq: 1,
                };
                let grant = crate::dnd::test_target(id, 1);
                let slot = Arc::new(Slot::new(grant.preparation.clone(), Vec::new(), None));
                glue.preparations.lock().unwrap().insert(id.origin, slot.clone());
                // Hold the real selection lock with a pending result. No helper or
                // File Provider domain is touched, including by the deferred worker.
                let waiting = slot.clone();
                let (entered, enter) = std::sync::mpsc::channel();
                let waiter = std::thread::spawn(move || waiting.choice.select(|| entered.send(()).unwrap()));
                enter.recv_timeout(Duration::from_secs(2)).unwrap();
                node.input_thread().unwrap().pending_drag_for_test(id, 1);
                let injector = start_with_backend(native.clone(), Box::new(|_| {})).unwrap();
                let main = node.main_sender_for_test();
                injector.push(InjectItem::Dnd {
                    command: DndCmd::Begin {
                        grant: grant.clone(),
                        id,
                        epoch: 1,
                        at: oink_foundation::Point::new(0., 0.),
                        items: Vec::new(),
                        retained_inventory: None,
                    },
                    main: main.clone(),
                });
                let up = InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false,
                };
                injector.push(InjectItem::Inject {
                    event: up.clone(),
                    from: None,
                });
                injector.push(InjectItem::Dnd {
                    command: DndCmd::Probe {
                        at: oink_foundation::Point::new(1., 1.),
                    },
                    main,
                });
                let observed = tokio::task::spawn_blocking(move || probe.recv_timeout(Duration::from_secs(2)))
                    .await
                    .unwrap();
                let injected = native.take_injected();
                let still_pending = slot.choice.pending();
                let native_unstarted = glue.active_target.lock().unwrap().is_none();
                // Always release the stalled fixture before assertions/shutdown.
                grant.revoke();
                glue.cancel_targets();
                assert!(waiter.join().unwrap().is_err());
                // Deterministically place a completed result behind an already
                // retired grant. This is the worker-complete / queued-Up race,
                // independent of scheduling or the earlier publication waiter.
                let late_id = ClipId { seq: 2, ..id };
                let late = crate::dnd::test_target(late_id, 2);
                late.revoke();
                glue.pending_begins
                    .lock()
                    .unwrap()
                    .insert((late_id, 2), BeginState::Ready(None));
                node.input_thread()
                    .unwrap()
                    .send(crate::threads::input::InputCommand::DndPrepared(DndCmd::Begin {
                        grant: late,
                        id: late_id,
                        epoch: 2,
                        at: oink_foundation::Point::new(0., 0.),
                        items: Vec::new(),
                        retained_inventory: None,
                    }));
                tokio::time::timeout(Duration::from_secs(2), async {
                    while glue.pending_begins.lock().unwrap().contains_key(&(late_id, 2)) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("retired ready result must clean up without native Begin");
                // Also cover Up after the input owner has requeued Begin but
                // before the ordinary owner gets to consume the ready spec.
                let late = crate::dnd::test_target(late_id, 3);
                late.revoke();
                glue.pending_begins
                    .lock()
                    .unwrap()
                    .insert((late_id, 3), BeginState::Ready(None));
                assert!(matches!(
                    glue.begin(late, oink_foundation::Point::new(0., 0.), Vec::new(), None, None)
                        .as_slice(),
                    [DndEvent::BeginFailed { .. }]
                ));
                assert!(!glue.pending_begins.lock().unwrap().contains_key(&(late_id, 3)));
                node.shutdown().await;
                drop(injector);
                assert!(observed.is_ok(), "ordinary owner remained blocked behind publication");
                assert!(still_pending, "test did not hold publication pending");
                assert!(native_unstarted, "Begin entered native state before metadata was ready");
                assert!(injected.contains(&up), "input UP was fenced by metadata publication");
            },
        );
    }

    #[test]
    fn independent_ordinary_cancellation_preserves_grant_and_new_announcement() {
        let old = Slot::new(token(1), Vec::new(), None);
        let grant = DndTarget::new(
            old.token.clone(),
            oink_foundation::content::Holder {
                device: DeviceId([2; 32]),
                epoch: 3,
            },
            1,
            DeviceId([1; 32]),
        );
        let new = Slot::new(token(2), Vec::new(), None);
        old.cancellation.cancel();
        assert!(old.cancellation.is_cancelled());
        assert!(!new.cancellation.is_cancelled());
        assert!(old.token.current());
        assert!(grant.begin_current());
        grant.delivered();
        old.cancellation.cancel_and_wait();
        grant.leave();
        assert!(
            grant.content_current(),
            "ordinary cancellation cannot revoke a granted delivery"
        );
        assert!(!grant.begin_current());
        assert!(new.token.current());
        assert!(!new.cancellation.is_cancelled());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn definitive_source_rejection_keeps_reason_and_does_not_wait_for_deadline() {
        use oink_clipboard::model::Unavailable;
        for reason in [
            Unavailable::ReadFailed,
            Unavailable::Permission,
            Unavailable::NoSuchItem,
            Unavailable::InvalidData,
            Unavailable::Cancelled,
        ] {
            let start = tokio::time::Instant::now();
            let (done, ready) = tokio::sync::oneshot::channel::<Result<(), Unavailable>>();
            done.send(Err(reason)).unwrap();
            assert_eq!(
                wait_prepared(
                    (start + Duration::from_secs(3)).into_std(),
                    ready,
                    &std::sync::atomic::AtomicU8::new(3)
                )
                .await,
                Err(Failure {
                    stage: 3,
                    reason,
                    timed_out: false
                })
            );
            assert_eq!(tokio::time::Instant::now(), start);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn preparation_worker_loss_is_a_definitive_failure() {
        let (done, ready) = tokio::sync::oneshot::channel::<Result<(), oink_clipboard::model::Unavailable>>();
        drop(done);
        let result = wait_prepared(
            (tokio::time::Instant::now() + Duration::from_secs(3)).into_std(),
            ready,
            &std::sync::atomic::AtomicU8::new(2),
        )
        .await;
        assert!(
            result.is_err(),
            "worker failure must not be erased into an empty success"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn deadline_reports_exact_blocked_stage_and_fast_result_remains_immediate() {
        for blocked in 1..=3 {
            let start = tokio::time::Instant::now();
            let (_done, ready) = tokio::sync::oneshot::channel::<Result<(), oink_clipboard::model::Unavailable>>();
            let stage = std::sync::atomic::AtomicU8::new(blocked);
            assert_eq!(
                wait_prepared((start + Duration::from_secs(3)).into_std(), ready, &stage).await,
                Err(Failure {
                    stage: blocked,
                    reason: oink_clipboard::model::Unavailable::NoProgress,
                    timed_out: true
                })
            );
            assert_eq!(tokio::time::Instant::now(), start + Duration::from_secs(3));
        }
        let start = tokio::time::Instant::now();
        let (done, ready) = tokio::sync::oneshot::channel();
        done.send(Ok(7)).unwrap();
        assert_eq!(
            wait_prepared(
                (start + Duration::from_secs(3)).into_std(),
                ready,
                &std::sync::atomic::AtomicU8::new(3)
            )
            .await,
            Ok(7)
        );
        assert_eq!(tokio::time::Instant::now(), start);
    }
}
