//! Ordinary-thread input commands and native handoff.
use super::*;
use crate::threads::{MainCommand, input::InputCommand};

impl Node {
    fn accept_ordinary_input(&self, work: &crate::threads::inject::OrdinaryInput) -> bool {
        if !self.input_backend().input_safety().accepts(work.safety) {
            return false;
        }
        if work.expired() {
            if let Ok(input) = self.input_thread() {
                input.report_input_backlog();
            }
            return false;
        }
        true
    }

    pub(crate) fn report_visual_health(&self) {
        if self.inner.native_surfaces {
            oink_locator::diagnostics::report_all();
        }
    }

    pub(super) fn start_main_commands(&self, receive: crate::threads::MainReceiver) -> std::io::Result<()> {
        let weak = Arc::downgrade(&self.inner);
        let worker = std::thread::Builder::new()
            .name("oink-main-commands".into())
            .spawn(move || {
                receive.bind();
                loop {
                    receive.begin_turn();
                    let Some(inner) = weak.upgrade() else { break };
                    if *inner.stop.borrow() {
                        // Finish any in-flight native command, then cancel on this
                        // same ordinary worker before discarding queued fences.
                        if let Some(dnd) = inner.dnd.get() {
                            dnd.stop_native();
                        }
                        break;
                    }
                    let node = Node { inner };
                    node.drain_input_trace();
                    match receive.try_recv() {
                        Ok(command) => node.main_command(command),
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            drop(node);
                            receive.wait();
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            })?;
        lock(&self.inner.ordinary_threads).push(worker);
        Ok(())
    }

    pub(crate) fn drain_input_trace(&self) {
        let diagnostics = self.inner.diagnostics.get();
        let dropped = oink_input::native_lifecycle::drain(|record| {
            if let Some(observation) = record.mt_contacts {
                if let Some(diagnostics) = diagnostics {
                    diagnostics
                        .recorder
                        .record_native(record.stamp, crate::diagnostics::contacts::mt_event(observation));
                }
                return;
            }
            if let Some(observation) = record.contacts {
                if let Some(diagnostics) = diagnostics {
                    diagnostics
                        .recorder
                        .record_native(record.stamp, crate::diagnostics::contacts::event(observation));
                }
                return;
            }
            if let Some(observation) = record.button {
                if let Some(diagnostics) = diagnostics {
                    diagnostics.recorder.record_native(
                        record.stamp,
                        crate::diagnostics::Event::ButtonTransition {
                            sequence: record.sequence,
                            holder: record.focus.map(|(id, _)| diagnostics.recorder.anonymizer().device(id)),
                            epoch: record.focus.map(|(_, epoch)| epoch),
                            observation,
                        },
                    );
                }
                return;
            }
            if let Some(gesture) = record.gesture {
                if let Some(diagnostics) = diagnostics {
                    let event = diagnostics.recorder.native_gesture(gesture);
                    diagnostics.recorder.record_native(record.stamp, event);
                }
                return;
            }
            if let Some(accounting) = record.pointer {
                if let Some(diagnostics) = diagnostics {
                    let event = crate::diagnostics::Event::PointerTransition {
                        sequence: record.sequence,
                        holder: record.focus.map(|(id, _)| diagnostics.recorder.anonymizer().device(id)),
                        epoch: record.focus.map(|(_, epoch)| epoch),
                        accounting,
                    };
                    diagnostics.recorder.record_native(record.stamp, event);
                }
                return;
            }
            let holder = record
                .focus
                .and_then(|(id, _)| diagnostics.map(|d| d.recorder.anonymizer().device(id)));
            let event = crate::diagnostics::Event::NativeInputLifecycle {
                at_us: record.at_us,
                sequence: record.sequence,
                kind: record.kind,
                data: record.data,
                holder,
                epoch: record.focus.map(|(_, epoch)| epoch),
            };
            // Formatting and subscribers run here, never on the native hook thread.
            tracing::info!(?event, "native input lifecycle");
            if let Some(diagnostics) = diagnostics {
                diagnostics.recorder.record_native(record.stamp, event);
            }
        });
        if dropped != 0 {
            tracing::warn!(dropped, "native input lifecycle queue overflow");
            if let Some(diagnostics) = diagnostics {
                diagnostics.recorder.record(crate::diagnostics::Event::Log {
                    module: "native_input_lifecycle_overflow".into(),
                    line: 0,
                    level: 3,
                    fields: vec![("dropped".into(), dropped as f64)],
                    labels: vec![],
                });
            }
        }
        if let (Some(trace), Some(diagnostics)) = (self.inner.input_trace.get(), self.inner.diagnostics.get()) {
            trace.drain(&diagnostics.recorder);
        }
    }

    fn main_command(&self, command: MainCommand) {
        match command {
            MainCommand::Wake => {}
            MainCommand::InputStatusChanged => {
                self.sync_notifications();
                self.consume_local_emergency();
                #[cfg(target_os = "macos")]
                self.inner.file_provider_safety_wake.notify_one();
                self.publish_local_options();
                self.emit_group();
                self.emit(ApiEvent::Pause(self.pause_status()));
            }
            MainCommand::Dnd {
                command,
                completion: _completion,
                report,
                safety,
            } => {
                if matches!(
                    command,
                    oink_routing::DndCmd::Begin { .. }
                        | oink_routing::DndCmd::HoldSource { .. }
                        | oink_routing::DndCmd::ResumeSource { .. }
                        | oink_routing::DndCmd::PrepareDrop { .. }
                        | oink_routing::DndCmd::Probe { .. }
                ) && (!self.input_backend().input_safety().accepts(safety)
                    || _completion
                        .work
                        .as_ref()
                        .is_some_and(|work| !self.accept_ordinary_input(work)))
                {
                    return;
                }
                let Some(dnd) = self.inner.dnd.get().cloned() else {
                    return;
                };
                let Ok(input) = self.input_thread() else { return };
                // The backend dispatches only window work to its native event loop.
                // Release/Begin/Cancel wait for that loop's callbacks, so running the
                // whole operation on AppKit's main queue starves its own completion.
                let _runtime = self.inner.rt.enter();
                let trace = self.inner.input_trace.get();
                let record = trace.and_then(|trace| trace.dnd_start_command(&command));
                let started = std::time::Instant::now();
                let begin_stamp = matches!(&command, oink_routing::DndCmd::Begin { .. })
                    .then(|| self.inner.reports.ingress.stamp())
                    .flatten();
                let offer = match &command {
                    oink_routing::DndCmd::Begin { id, .. }
                    | oink_routing::DndCmd::PrepareDrop { id, .. }
                    | oink_routing::DndCmd::HoldSource { id, .. }
                    | oink_routing::DndCmd::FinishSource { id, .. }
                    | oink_routing::DndCmd::ResumeSource { id, .. }
                    | oink_routing::DndCmd::ReleaseSourceButton { id, .. }
                    | oink_routing::DndCmd::ForgetSource { id }
                    | oink_routing::DndCmd::ReleaseData { id } => Some(id.seq),
                    _ => None,
                };
                let owner = offer.map(|offer| {
                    oink_foundation::drag_trace::Trace::new(
                        54,
                        oink_foundation::drag_trace::Context {
                            offer,
                            ..oink_foundation::drag_trace::Context::current()
                        },
                    )
                });
                let result = match &owner {
                    Some(trace) => trace.scope(|| dnd.execute_reported(command, report)),
                    None => dnd.execute_reported(command, report),
                };
                if let Some(trace) = owner {
                    trace.result(0);
                }

                if let Some(stamp) = begin_stamp {
                    self.inner.reports.ingress.interval(
                        stamp,
                        oink_telemetry::Metric::DragBeginWorker,
                        std::time::Instant::now(),
                    );
                }
                if let (Some(trace), Some(record)) = (trace, record) {
                    let success = result.iter().find_map(|event| match event {
                        oink_routing::DndEvent::HostLeft { ok, .. } => Some(*ok),
                        oink_routing::DndEvent::Ended { dropped, .. } => Some(*dropped),
                        _ => None,
                    });
                    trace.dnd_finish_command(record, started.elapsed().as_micros() as u64, success);
                }
                for event in result {
                    input.send(InputCommand::DndEvent(event));
                }
            }
            MainCommand::DndArtwork(command) => {
                if let Some(dnd) = self.inner.dnd.get() {
                    let _ = dnd.execute_reported(command, None);
                }
            }
            MainCommand::DndPreview { peer, stream, message } => {
                if let Some(dnd) = self.inner.dnd.get() {
                    dnd.preview(peer, stream, message);
                }
            }
            MainCommand::Low { peer, stream, message } => {
                if let Some(dnd) = self.inner.dnd.get() {
                    dnd.low(peer, stream, message);
                }
            }
            MainCommand::Attachment { peer, stream, message } => {
                if let Some(dnd) = self.inner.dnd.get() {
                    dnd.attachment(peer, stream, message);
                }
            }
            MainCommand::Decoration(output) => {
                let Some(trail) = self.inner.cursor_trail.get() else {
                    return;
                };
                match output {
                    oink_routing::Output::CursorTrail { active } => trail.set_active(active),
                    oink_routing::Output::CursorTrailResume(state) => trail.resume(state),
                    oink_routing::Output::CursorTrailResumeClipped { state, clip } => {
                        trail.resume_with_clip(state, Some(clip))
                    }
                    oink_routing::Output::CursorTrailDepart { exit } => trail.depart(exit),
                    _ => tracing::warn!("input owner sent an unexpected native decoration"),
                }
            }
            MainCommand::TrailDuration(ms) => {
                let secs = f64::from(ms) / 1000.;
                if let Some(trail) = self.inner.cursor_trail.get() {
                    trail.set_trail_duration(secs);
                }
                crate::aim::set_follow_trail_duration(self, secs);
            }
            MainCommand::TrailSuppressed(hidden) => {
                if let Some(trail) = self.inner.cursor_trail.get() {
                    trail.set_suppressed(hidden);
                }
            }
            MainCommand::TrailPointer { head, visible, clip } => {
                if let Some(trail) = self.inner.cursor_trail.get() {
                    trail.pointer_on_display(head, visible, clip);
                }
            }
            MainCommand::Activity { from } => {
                if let Ok(links) = self.links() {
                    links.user_activity(if from.is_some() {
                        oink_network::UserActivitySource::Group
                    } else {
                        oink_network::UserActivitySource::Local
                    });
                }
                let _ = crate::threads::declare_activity(self.input_backend().as_ref(), from, self.activity_allowed());
            }
            MainCommand::IdentifyScreens(id) => crate::display_overlay::identify_local(self, id),
            MainCommand::DismissOverlay(id) => crate::display_overlay::dismiss(self, id),
            MainCommand::ResetUserIdle => {
                if self.activity_allowed() {
                    let _ = self.input_backend().reset_user_idle();
                }
            }
            MainCommand::ExitVolumeKey {
                work,
                source,
                route,
                press_id,
                code,
                pressed,
                repeat,
                modifiers,
            } => {
                if !self.accept_ordinary_input(&work) {
                    return;
                }
                let Some(action) = oink_routing::input::VolumeKey::from_code(code) else {
                    return;
                };
                let Some(native_action) = oink_audio::volume::KeyAction::from_code(code) else {
                    return;
                };
                let receipt = self.inner.audio.get().map(|audio| {
                    audio.exit_volume_key(oink_audio::volume::KeyEvent {
                        source: oink_network::wire_identity::boot(source),
                        session: (*route.session).clone(),
                        press_id,
                        action: native_action,
                        pressed,
                        repeat,
                        modifiers,
                    })
                });
                let recorder = self
                    .inner
                    .diagnostics
                    .get()
                    .map(|diagnostics| diagnostics.recorder.clone());
                self.inner.rt.spawn(async move {
                    use oink_protocol::v1::dto::ControlFailure;
                    let result = match receipt {
                        Some(receipt) => receipt.await.unwrap_or(Err(ControlFailure::ApplyFailed)),
                        None => Err(ControlFailure::OutputUnavailable),
                    };
                    if let Err(reason) = result
                        && let Some(recorder) = recorder
                    {
                        recorder.volume_refused(source.device_id, route.target.device_id, press_id, action, reason);
                    }
                });
            }
            MainCommand::Media { event, work } => {
                if !self.accept_ordinary_input(&work) {
                    return;
                }
                // Media control is ordinary work; native routed input stays in InjectHandle.
                let result = self.input_backend().inject(&event);
                if let Some(trace) = self.inner.input_trace.get() {
                    trace.injected(&event, None, result.is_ok());
                }
                if let Err(error) = result {
                    tracing::warn!(%error, "media control failed");
                }
            }
            MainCommand::RoutingStatus { status, work } => {
                if work.as_ref().is_none_or(|work| self.accept_ordinary_input(work)) {
                    self.routing_status(status);
                }
            }
            MainCommand::ResumeSharing { epoch } => {
                let safety = self.input_backend().input_safety();
                if safety.paused
                    && safety.epoch == epoch
                    && let Err(error) = crate::api::AgentApi::set_local_paused(
                        self,
                        crate::device_controls::runtime::fresh_request_id(),
                        false,
                    )
                {
                    tracing::warn!(?error, "resume hotkey could not resume sharing");
                }
            }
            MainCommand::Realtime(record) => {
                let Some(diagnostics) = self.inner.diagnostics.get() else {
                    return;
                };
                let names = match record.code {
                    crate::threads::SCHEDULER_TOTALS => {
                        ["late_rounds", "late_max_us", "over_budget", "over_budget_max_us"]
                    }
                    crate::threads::SCHEDULER_POLICY => ["readback_present", "native_priority", "tier", "demotions"],
                    _ => return,
                };
                let mut fields: Vec<_> = names
                    .into_iter()
                    .zip(record.values)
                    .map(|(name, value)| (name.into(), value))
                    .collect();
                fields.push(("at_us".into(), record.at_us as f64));
                diagnostics.recorder.record(crate::diagnostics::Event::Log {
                    module: "oink_scheduler".into(),
                    line: 0,
                    level: 2,
                    fields,
                    labels: vec![("source".into(), format!("{:?}", record.source))],
                });
            }
        }
    }

    fn activity_allowed(&self) -> bool {
        self.inner.group.get().is_some_and(|group| !group.applied().paused)
            && self.settings().routing.wake_displays_on_input
    }

    fn routing_status(&self, status: StatusEvent) {
        self.notification_status(&status);
        match status {
            StatusEvent::FocusLock { .. } => {}
            StatusEvent::TouchpadContacts {
                mode,
                frames,
                gestures,
                cancelled,
                ended,
            } => {
                tracing::info!(
                    ?mode,
                    frames,
                    gestures,
                    cancelled,
                    ended,
                    "touchpad contact mode/gesture summary"
                );
            }
            StatusEvent::GameMode(status) => self.emit(ApiEvent::GameMode(status.into())),
            StatusEvent::Focus(focus) => {
                #[cfg(target_os = "macos")]
                self.inner.file_provider_safety_wake.notify_one();
                self.emit(ApiEvent::Focus(self.focus_view(&focus)));
            }
            StatusEvent::Hotkey { action } => self.on_hotkey(&action),
            StatusEvent::AudioVolumeKey { code } => {
                if let Some(audio) = self.inner.audio.get() {
                    audio.volume_key(code);
                }
            }
            StatusEvent::LayoutChanged | StatusEvent::PeerDisplaysChanged(_) => {
                self.emit_layout();
                self.emit_group();
            }
            StatusEvent::PeerTimedOut(_) | StatusEvent::PeerRecovered(_) => self.emit_group(),
            StatusEvent::ScreenLocalInput(_) | StatusEvent::KeyboardDisabled(_) | StatusEvent::MouseDisabled(_) => {
                self.publish_local_options();
                self.emit_group();
            }
            StatusEvent::KeyboardReleased => self.persist_keyboard_safety_release(),
            StatusEvent::LocalFactsChanged => self.publish_local_options(),
            StatusEvent::EmergencyStop { .. } => {
                // InputStatusChanged consumes the local trigger sequence exactly once.
                // Received emergency epochs never create another group event.
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_clipboard::{
        ClipboardError,
        dnd::{DndBackend, DndSink, DragSpec, Injector},
    };
    use oink_routing::DndCmd;

    #[cfg(windows)]
    #[test]
    fn permissions_projection_never_queries_native_policy_on_the_command_owner() {
        crate::test_watchdog::run(
            module_path!(),
            "permissions_projection_never_queries_native_policy_on_the_command_owner",
            Some(2),
            async {
                let root = tempfile::tempdir().unwrap();
                // Assemble a production-services node without starting native
                // owners. A UI projection must only read their last observation.
                let node = Node::prepare_with_services(
                    NodeOptions {
                        net: oink_test_support::network_config(root.path().into(), "permission-projection", 0),
                        input: Arc::new(oink_test_support::input::FakeInput::new()),
                        displays: vec![],
                        display_backend: None,
                        clipboard: None,
                        mode_switch_in_callback: true,
                        capture_reports_key_repeat: false,
                        cursor_trail: false,
                    },
                    true,
                )
                .unwrap();
                let before = crate::windows_permissions::test_observations();
                let items = crate::agent_api::permission_items(&node);
                assert_eq!(
                    crate::windows_permissions::test_observations(),
                    before,
                    "permission projection blocked the drag command owner on COM/SCM queries"
                );
                assert!(items.iter().all(|item| item.state == PermissionState::NotDetermined));
                node.inner
                    .windows_access
                    .publish(crate::windows_permissions::Observation {
                        firewall: PermissionState::Denied,
                        system_service: PermissionState::Granted,
                    });
                let updated = crate::agent_api::permission_items(&node);
                assert_eq!(updated[0].state, PermissionState::Denied);
                assert_eq!(updated[1].state, PermissionState::Granted);
                assert_eq!(crate::windows_permissions::test_observations(), before);
            },
        );
    }

    struct RecordingDrag(std::sync::mpsc::Sender<(&'static str, String)>);
    impl RecordingDrag {
        fn record(&self, command: &'static str) {
            let _ = self
                .0
                .send((command, std::thread::current().name().unwrap_or("").to_owned()));
        }
    }
    impl DndBackend for RecordingDrag {
        fn start(&self, _: DndSink, _: Injector) -> Result<(), ClipboardError> {
            Ok(())
        }
        fn probe(&self, _: oink_foundation::Point) {
            self.record("probe");
        }
        fn unprobe(&self) {
            self.record("unprobe");
        }
        fn release(&self) -> bool {
            true
        }
        fn hold_source(
            &self,
            _: oink_clipboard::dnd::DragOperation,
        ) -> Result<oink_foundation::dnd::DragAnchor, ClipboardError> {
            self.record("hold");
            Err(ClipboardError::Unsupported)
        }
        fn begin(&self, _: DragSpec) -> Result<(), ClipboardError> {
            Ok(())
        }
        fn cancel(&self) -> bool {
            self.record("cancel");
            true
        }
    }

    // No native application loop is started. Platform methods must run on the
    // ordinary dispatcher, because they dispatch window work and wait for native
    // callbacks themselves. Dispatching the whole method to AppKit deadlocks it.
    #[test]
    fn blocking_drag_methods_run_on_the_ordinary_command_worker() {
        crate::test_watchdog::run(
            module_path!(),
            "blocking_drag_methods_run_on_the_ordinary_command_worker",
            Some(2),
            blocking_drag_methods_run_on_the_ordinary_command_worker_scenario(),
        );
    }

    async fn blocking_drag_methods_run_on_the_ordinary_command_worker_scenario() {
        let root = tempfile::tempdir().unwrap();
        let native = Arc::new(oink_test_support::input::FakeInput::new());
        let node = Node::start(NodeOptions {
            net: oink_test_support::network_config(root.path().into(), "drag-dispatch", 0),
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
        let (sent, received) = std::sync::mpsc::channel();
        node.start_dnd_backend(Box::new(RecordingDrag(sent))).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !node.input_thread().unwrap().opening_state(true).iter().any(|message| {
                matches!(
                    message,
                    oink_routing::Packet::Dnd(oink_routing::dnd::DndMsg::Caps { enabled: true })
                )
            }) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let injector = crate::threads::inject::start_with_backend(native, Box::new(|_| {})).unwrap();
        for command in [
            DndCmd::Probe {
                at: oink_foundation::Point::new(1., 2.),
            },
            DndCmd::LeaveHost {
                physical: false,
                id: oink_foundation::content::ClipId {
                    origin: node.id(),
                    seq: 1,
                },
                epoch: 1,
            },
            DndCmd::Cancel {
                id: oink_foundation::content::ClipId {
                    origin: node.id(),
                    seq: 1,
                },
                epoch: 1,
                physical: false,
            },
            // The keyed leave/cancel above have no active native host and
            // must not reach the backend. Probe operations still dispatch.
            DndCmd::Unprobe,
            DndCmd::Probe {
                at: oink_foundation::Point::new(2., 3.),
            },
        ] {
            injector.push(crate::threads::inject::InjectItem::Dnd {
                command,
                main: node.inner.main_commands.get().unwrap().clone(),
            });
        }
        let result = tokio::task::spawn_blocking(move || {
            (0..3)
                .map(|_| received.recv_timeout(Duration::from_secs(2)))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .unwrap();
        // Close consumers before asserting so a failed dispatch cannot hang cleanup.
        node.shutdown().await;
        drop(injector);
        assert_eq!(
            result.unwrap(),
            ["probe", "unprobe", "probe"].map(|command| (command, "oink-main-commands".into()))
        );
    }
}
