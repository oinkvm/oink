use super::*;
impl super::link::Handle {
    /// Called after the native backend's main loop is ready. Native commands continue
    /// to run on the main dispatcher; only low-priority I/O runs in the module task.
    pub(crate) fn start_backend(
        &self,
        node: Node,
        backend: Box<dyn DndBackend>,
        input_backend: Arc<dyn oink_input::InputBackend>,
    ) -> anyhow::Result<()> {
        let input = node.input_thread()?;
        let backend: Arc<dyn DndBackend> = Arc::from(backend);
        oink_clipboard::dnd::set_source_observer(super::timing::observed);
        oink_routing::dnd::set_source_learned_observer(super::timing::learned);
        oink_clipboard::dnd::set_preparation_observer(crate::telemetry::projections::drag_preparation);
        let files = Arc::new(DndFiles::new(node.link_config()?.source_grants()));
        *files.input.lock().unwrap() = Some(input.clone());
        backend.set_content_store(node.cache_dir());
        files.set_paused(node.link_config()?.applied().paused);
        let glue = Arc::new(DndGlue {
            me: node.id(),
            data_dir: node.cache_dir(),
            runtime: tokio::runtime::Handle::current(),
            native_targets: Arc::default(),
            target_images: Mutex::default(),
            backend: backend.clone(),
            input_node: Arc::downgrade(&node.inner),
            files: files.clone(),
            link_files: Some(self.files.clone()),
            input_backend: Some(input_backend.clone()),
            local: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(dnd_seq_base()),
            active_operation: Mutex::new(None),
            active_target: Mutex::new(None),
            update_targets: Mutex::new(HashMap::new()),
            #[cfg(target_os = "macos")]
            preparations: Mutex::new(HashMap::new()),
            pending_begins: Mutex::new(HashMap::new()),
            active_source: Mutex::new(None),
            export_epoch: Mutex::new(None),
            source_epoch: AtomicU64::new(0),
            source_physical: std::sync::atomic::AtomicBool::new(false),
            source_origin: Mutex::new(None),
            source_releases: Mutex::new(None),
            hold_observations: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&glue);
        let sink_input = input.clone();
        let sink_trace = node.inner.input_trace.get().cloned();
        // Both source callbacks run on ordinary workers. Keep cache discovery
        // and queue submission ordered so artwork cannot overtake Detected
        // after its source-image snapshot but before its event is submitted.
        let source_events = Mutex::new(());
        let sink: oink_clipboard::dnd::DndSink = Arc::new(move |event| {
            let _source_order = matches!(
                event,
                PlatformDndEvent::Detected { .. } | PlatformDndEvent::SourceImage { .. }
            )
            .then(|| source_events.lock().unwrap());
            let Some(glue) = weak.upgrade() else { return };
            let event = match event {
                PlatformDndEvent::NativeHoldSummary { operation, summary } => {
                    if let Some((id, physical)) = glue.hold_observations.lock().unwrap().remove(&operation)
                        && let Some(inner) = glue.input_node.upgrade()
                        && let Some(diagnostics) = inner.diagnostics.get()
                    {
                        diagnostics.recorder.dnd_native_hold(id, physical, *summary);
                    }
                    return;
                }
                PlatformDndEvent::SourceExported { operation, result } => {
                    let Some(event) = glue.exported(operation, result) else {
                        return;
                    };
                    event
                }
                PlatformDndEvent::SourceCancelRetired { operation } => {
                    let active = *glue.active_source.lock().unwrap();
                    let Some((id, _)) = active.filter(|(_, current)| *current == operation) else {
                        return;
                    };
                    DndEvent::SourceCancelRetired { id }
                }
                PlatformDndEvent::SourceCatcherLost { operation } => {
                    let active = *glue.active_source.lock().unwrap();
                    let Some((id, _)) = active.filter(|(_, current)| *current == operation) else {
                        return;
                    };
                    DndEvent::SourceCatcherLost { id }
                }
                PlatformDndEvent::SourceEnded { operation, accepted } => {
                    let active = *glue.active_source.lock().unwrap();
                    let Some((id, _)) = active.filter(|(_, current)| *current == operation) else {
                        return;
                    };
                    DndEvent::SourceEnded { id, accepted }
                }
                PlatformDndEvent::SourceImage { operation, image } => {
                    let local = glue.local.lock().unwrap();
                    let Some(id) = local
                        .iter()
                        .find_map(|(id, drag)| (drag.operation == operation).then_some(*id))
                    else {
                        return;
                    };
                    DndEvent::SourceImage { id, image }
                }
                PlatformDndEvent::Detected { operation, items, ops } => {
                    if oink_foundation::maintenance::process().closed() {
                        return;
                    }
                    glue.detected(operation, items, ops)
                }
                PlatformDndEvent::Ended { operation, dropped } => {
                    let mut active = glue.active_operation.lock().unwrap();
                    let Some((current, id, epoch)) = *active else {
                        return;
                    };
                    if current != operation {
                        return;
                    }
                    *active = None;
                    glue.target_ended(id, epoch, dropped);
                    if !dropped {
                        let grant = glue.active_target.lock().unwrap().as_ref().map(|(_, g)| g.clone());
                        if let Some(grant) = grant {
                            glue.release_target(&grant, grant.input == glue.me && !grant.released());
                        }
                    }
                    DndEvent::Ended { id, epoch, dropped }
                }
            };
            if let Some(trace) = &sink_trace {
                trace.dnd_observed(&event, "PlatformEvent");
            }
            sink_input.send(crate::threads::input::InputCommand::DndEvent(event));
        });
        let press = input_backend.clone();
        let trace = node.inner.input_trace.get().cloned();
        let press_trace = trace.clone();
        backend.set_source_start(input.source_start());
        let nudge_input = input_backend.clone();
        backend.set_source_image_nudge(Arc::new(move |operation, button, posted| {
            nudge_input
                .nudge_held_drag(operation.sequence, button, posted)
                .map_err(|error| error.to_string())
        }));
        let freeze_input = input_backend.clone();
        let relocate_input = input_backend.clone();
        let return_input = input_backend.clone();
        backend.set_source_hold_input(oink_clipboard::dnd::SourceHoldInput {
            freeze: Arc::new(move |operation| {
                freeze_input
                    .freeze_drag_anchor(operation.sequence)
                    .map_err(|error| error.to_string())
            }),
            relocate: Arc::new(move |operation, at| {
                relocate_input
                    .relocate_drag_anchor(operation.sequence, at)
                    .map_err(|error| error.to_string())
            }),
            return_to: Arc::new(move |operation, at, posted| {
                return_input
                    .move_held_drag_observed(operation.sequence, at, posted)
                    .map_err(|error| error.to_string())
            }),
        });
        let press_glue = Arc::downgrade(&glue);
        backend.set_source_press(Arc::new(move |operation, point| {
            let Some(glue) = press_glue.upgrade() else {
                return Err("service stopped".into());
            };
            let grant = glue
                .active_target
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(op, _)| *op == operation)
                .map(|(_, g)| g.clone());
            let Some(grant) = grant.filter(|g| g.claim_down()) else {
                return Err("drag grant retired".into());
            };
            let result = press.press_drag_at(point).map_err(|error| error.to_string());
            grant.down_submitted(result.is_ok());
            if let Some(trace) = &press_trace {
                trace.dnd_native("SourcePress", Some(point), Some(true), result.is_ok());
            }
            result
        }));
        let inject_glue = Arc::downgrade(&glue);
        let inject: oink_clipboard::dnd::Injector = Arc::new(move |event| {
            if matches!(
                event,
                oink_foundation::input::InjectEvent::Button {
                    button: oink_foundation::input::MouseButton::Left,
                    pressed: false
                }
            ) && let Some(glue) = inject_glue.upgrade()
            {
                let grant = glue.active_target.lock().unwrap().as_ref().map(|(_, g)| g.clone());
                if let Some(grant) = grant
                    && !grant.claim_up()
                {
                    return Ok(());
                }
            }
            let result = input_backend.inject(event).map_err(|error| error.to_string());
            if let Some(trace) = &trace {
                trace.dnd_injected(event, result.is_ok());
            }
            result
        });
        let destination = self.glue.clone();
        tokio::spawn(async move {
            let mut stop = node.stop_receiver();
            for _ in 0..30 {
                if *stop.borrow() {
                    return;
                }
                let backend = backend.clone();
                let sink = sink.clone();
                let inject = inject.clone();
                if matches!(
                    tokio::task::spawn_blocking(move || backend.start(sink, inject)).await,
                    Ok(Ok(()))
                ) {
                    if let Some(clip) = node.clipboard() {
                        clip.set_dnd_files(files);
                    }
                    *destination.lock().unwrap() = Some(glue);
                    input.send(crate::threads::input::InputCommand::DndAvailable(true));
                    return;
                }
                tokio::select! { _ = stop.changed() => return, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
            }
            tracing::info!("cross-device drag and drop unavailable on this computer");
        });
        Ok(())
    }
}
