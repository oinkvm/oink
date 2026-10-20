//! Ordinary-thread platform observations and API notifications.
use super::*;
use crate::threads::input::InputCommand;

impl Node {
    pub(crate) fn refresh_permissions(&self) {
        self.inner.permission_refresh.store(true, Ordering::Release);
        for worker in lock(&self.inner.ordinary_threads).iter() {
            if worker.thread().name() == Some("oink-platform-observer") {
                worker.thread().unpark();
            }
        }
        if let Some(audio) = self.inner.audio.get() {
            audio.refresh_permission();
        }
        #[cfg(target_os = "macos")]
        self.inner.file_provider_wake.notify_one();
    }

    pub(super) fn current_displays(&self) -> anyhow::Result<Vec<DisplayInfo>> {
        match &self.inner.display_backend {
            Some(backend) => Ok(backend.displays()?),
            None => Ok(self.inner.initial_displays.clone()),
        }
    }

    // Release-profile unit tests need the same sensor/input command ordering.
    #[cfg(test)]
    pub(crate) async fn hold_platform_sensor_clock(&self) {
        let (parked, ready) = tokio::sync::oneshot::channel();
        *lock(&self.inner.platform_hold) = Some(parked);
        for worker in lock(&self.inner.ordinary_threads).iter() {
            if worker.thread().name() == Some("oink-platform-observer") {
                worker.thread().unpark();
            }
        }
        ready.await.unwrap();
    }

    pub(super) fn start_platform_observer(&self) -> std::io::Result<()> {
        let dirty = Arc::new(AtomicBool::new(true));
        let observer = Arc::new(std::sync::OnceLock::<std::thread::Thread>::new());
        if let Some(display) = &self.inner.display_backend {
            let changed = dirty.clone();
            let observer = observer.clone();
            if let Err(error) = display.watch(Arc::new(move || {
                display_changed(&changed, &observer);
            })) {
                tracing::warn!(%error, "display observation unavailable");
            }
        }
        let weak = Arc::downgrade(&self.inner);
        let worker = std::thread::Builder::new()
            .name("oink-platform-observer".into())
            .spawn(move || {
                let _ = observer.set(std::thread::current());
                let Some(inner) = weak.upgrade() else { return };
                let power_thread = std::thread::current();
                let _power_wake = inner
                    .native_services
                    .then(|| oink_audio::device_notifications::subscribe(Arc::new(move || power_thread.unpark())));
                let mut power = inner.input_backend.display_power_observation().ok();
                let system_power = inner
                    .native_services
                    .then(oink_audio::power::Monitor::new)
                    .and_then(Result::ok);
                let mut previous_power = oink_audio::power::State::default();
                let mut pending_power = None;
                #[cfg(windows)]
                let mut windows_access = inner.native_services.then(crate::windows_permissions::Observer::new);
                drop(inner);
                let mut last_preferences = None;
                let mut last_permissions = None;
                let mut last_caps = None;
                let mut last_feel = None;
                let mut last_edges = None;
                let mut last_capture = None;
                let mut volume_probe_attached = false;
                let mut last_pointer = None;
                let mut last_session = None;
                let mut display_revision = 0;
                let mut enumerated = None;
                loop {
                    let Some(inner) = weak.upgrade() else { break };
                    if *inner.stop.borrow() {
                        break;
                    }
                    let node = Node { inner };
                    let (Ok(input), Ok(group)) = (node.input_thread(), node.link_config()) else {
                        break;
                    };
                    #[cfg(windows)]
                    if let Some(observed) = windows_access.as_mut().and_then(|access| access.sample(Instant::now())) {
                        // COM/SCM queries belong only to this ordinary observer.
                        // Command, focus and group paths read completed snapshots.
                        node.inner.windows_access.publish(observed);
                        node.emit(ApiEvent::Permissions(crate::agent_api::permission_items(&node)));
                    }
                    let backend = node.input_backend();
                    let session = backend.session_locked();
                    if last_session != Some(session) {
                        input.send(InputCommand::SessionLocked(session));
                        last_session = Some(session);
                    }
                    if dirty.swap(false, Ordering::AcqRel) {
                        match node.current_displays() {
                            Ok(displays) => {
                                if let Err(reason) = node.seed_live_screen_preferences(&displays) {
                                    tracing::warn!(
                                        ?reason,
                                        "display preference seed failed; retaining previous geometry"
                                    );
                                    continue;
                                }
                                if let Some(diag) = node.inner.diagnostics.get().filter(|d| d.recorder.enabled()) {
                                    if enumerated.as_ref() != Some(&displays) {
                                        display_revision += 1;
                                        diag.recorder.display_enumerated(&displays, display_revision);
                                        enumerated = Some(displays.clone());
                                    }
                                } else {
                                    enumerated = None;
                                }
                                group.local_displays_changed(displays.clone());
                                input.send(InputCommand::LocalDisplays(displays.clone()));
                                if let Some(trail) = node.inner.cursor_trail.get() {
                                    trail.set_displays(displays);
                                }
                            }
                            Err(error) => tracing::warn!(%error, "display refresh failed"),
                        }
                    }
                    let mut environment = *node.inner.input_environment.load();
                    if let Some(monitor) = &system_power {
                        let state = monitor.state();
                        if state != previous_power {
                            previous_power = state;
                            pending_power = Some(state);
                            environment.system_sleeping = state.sleeping;
                            if state.sleeping {
                                group.sleep();
                            } else {
                                group.wake();
                            }
                        }
                        if let Some(state) = pending_power
                            && (!state.sleeping
                                || (backend.capture_active() == Some(false)
                                    && node.links().is_ok_and(|l| l.links().links.is_empty())))
                        {
                            monitor.acknowledge(state.epoch);
                            pending_power = None;
                        }
                    }
                    if let Some(asleep) = power.as_mut().and_then(|power| power.poll()) {
                        environment.displays_asleep = asleep;
                    }
                    if node.inner.permission_refresh.swap(false, Ordering::AcqRel)
                        || last_preferences.is_none_or(|at: Instant| at.elapsed() >= Duration::from_secs(2))
                    {
                        last_preferences = Some(Instant::now());
                        let caps = backend.caps_lock_switches_input_source();
                        if last_caps != Some(caps) {
                            last_caps = Some(caps);
                            node.emit(ApiEvent::InputSettings(crate::agent_api::input_settings(&node)));
                        }
                        let permissions = backend.permissions();
                        environment.secure_input = permissions.secure_input_active;
                        if last_permissions.as_ref() != Some(&permissions) {
                            input.send(InputCommand::Controllable(!inject_blocked(&permissions)));
                            node.publish_local_permissions();

                            node.emit_group();
                            last_permissions = Some(permissions);
                        }
                        // Firewall/service configuration, network evidence and audio grants
                        // can change independently of input permissions.
                        node.emit(ApiEvent::Permissions(crate::agent_api::permission_items(&node)));
                        let feel = backend.feel_profile();
                        if last_feel.as_ref() != Some(&feel) {
                            input.send(InputCommand::LocalFeel(feel.clone()));
                            last_feel = Some(feel);
                        }
                        let displays = group.local_displays().displays.clone();
                        let edges = backend.edge_resistance_regions(&displays);
                        if group.local_displays().displays == displays && last_edges.as_ref() != Some(&edges) {
                            input.send(InputCommand::EdgeResistance(edges.clone()));
                            last_edges = Some(edges);
                        }
                    }
                    if *node.inner.input_environment.load() != environment {
                        node.inner.input_environment.publish(environment);
                        input.wake()();
                    }
                    if !volume_probe_attached && let Some(audio) = node.inner.audio.get() {
                        input.send(InputCommand::VolumeExit(
                            audio.volume_context_snapshot(),
                            audio.keep_local_snapshot(),
                        ));
                        volume_probe_attached = true;
                    }
                    let status = input.status();
                    let raw = backend.pointer_capture_sample();
                    let eligible = status.pointer_focus
                        && !status.paused
                        && status
                            .game_status
                            .as_ref()
                            .is_some_and(|game| game.mode == oink_routing::GameMode::Auto);
                    let sample = raw
                        .and_then(|sample| backend.refresh_pointer_capture_sample(sample, eligible, status.game_mode));
                    let hidden = raw.is_some_and(|sample| sample.hidden);
                    let mut comparable = sample;
                    if let Some(windows) = comparable.as_mut().and_then(|s| s.windows.as_mut()) {
                        windows.sampled_at = 0;
                    }
                    let observation = (comparable, hidden, eligible, status.game_mode);
                    let detecting = eligible && sample.is_some_and(|s| s.hidden && s.application_hidden != Some(false));
                    if last_pointer != Some(observation) || detecting || status.game_mode {
                        input.send(InputCommand::PointerSample { sample, hidden });
                        last_pointer = Some(observation);
                    }
                    let capture = (backend.capture_active(), status.locked_device);
                    if last_capture != Some(capture) {
                        last_capture = Some(capture);
                        node.publish_local_options();
                        node.emit_group();
                        node.emit(ApiEvent::Pause(node.pause_status()));
                    }
                    #[cfg(all(test, debug_assertions))]
                    node.inner.platform_ready.store(true, Ordering::Release);
                    #[cfg(test)]
                    if let Some(parked) = lock(&node.inner.platform_hold).take() {
                        node.inner.platform_clock_held.store(true, Ordering::Release);
                        let _ = parked.send(());
                    }
                    #[cfg(test)]
                    let clock_held = node.inner.platform_clock_held.load(Ordering::Acquire);
                    // Drop the strong Node before sleeping; shutdown owns the thread join.
                    drop(node);
                    // Only unobservable platform sensors use this sampling period.
                    // Display callbacks unpark the owner immediately.
                    #[cfg(test)]
                    if clock_held {
                        std::thread::park();
                        continue;
                    }
                    std::thread::park_timeout(Duration::from_millis(50));
                }
            })?;
        lock(&self.inner.ordinary_threads).push(worker);
        Ok(())
    }
}

impl Node {
    pub(super) fn start_observers(&self) -> anyhow::Result<()> {
        self.start_platform_observer()?;
        #[cfg(target_os = "macos")]
        if self.native_services_enabled() {
            lock(&self.inner.tasks).push(crate::file_provider::spawn(self));
        }
        let mut diagnostic_status = self.inner.diagnostics.get().unwrap().recorder.status_changes();
        let mut diagnostic_revision = *diagnostic_status.borrow_and_update();
        let mut changes = self.link_config()?.subscribe();
        let mut stop = self.stop_receiver();
        let node = self.clone();
        let task = tokio::spawn(async move {
            // Establish the shared clock before the first scheduled deadline.
            crate::connection_quality::now_ms_at(Instant::now());
            let mut sample = statistics_interval();
            let mut previous_input_sample = Instant::now();
            let mut previous_pause = node.pause_status();
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    result = diagnostic_status.changed() => {
                        if result.is_err() { break; }
                        let revision = *diagnostic_status.borrow_and_update();
                        let status = node.inner.diagnostics.get().unwrap().recorder.status();
                        if revision.0 != diagnostic_revision.0 {
                            node.emit(ApiEvent::DiagnosticsStatus(status.clone()));
                        }
                        if revision.1 != diagnostic_revision.1 {
                            node.emit(ApiEvent::DiagnosticsStats((&status).into()));
                        }
                        diagnostic_revision = revision;
                    },
                    change = changes.next() => match change {
                        Some(crate::config_module::ConfigChange::DeviceControl{peer,message})=>node.receive_device_control(peer.into(),message),
                        Some(crate::config_module::ConfigChange::PairingEvent(event))=>node.emit((*event).into()),
                        Some(crate::config_module::ConfigChange::BlockedDevices)=>{
                            if let Ok(snapshot)=node.link_config().and_then(|group|group.blocked_devices()){node.emit(ApiEvent::BlockedDevices{snapshot});}
                        },
                        Some(_) => { node.emit_layout(); node.emit_group(); },
                        None => break,
                    },
                    deadline = sample.tick() => {
                        node.inner.reports.ingress.drain_realtime();
                        node.sample_rtt(statistics_sample_time(deadline.into_std(), Instant::now()));
                        node.reconcile_controls();
                        node.refresh_device_cards();
                        if let Ok(input) = node.input_thread() {
                            let now = Instant::now();
                            let mut timings = input.take_injection_timing(now);
                            timings.window_us = now.saturating_duration_since(previous_input_sample).as_micros() as u64;
                            previous_input_sample = now;
                            oink_input::timing::take_capture(&mut timings);
                            crate::telemetry::projections::input_window(&node.inner.reports.ingress, &timings);
                            node.inner.diagnostics.get().unwrap().recorder.record(crate::diagnostics::Event::InputTiming(timings.clone()));
                            if node.has_stats_subscribers() { node.emit(ApiEvent::InputStats(timings)); }
                        }
                        continue;
                    },
                }
                node.reconcile_controls();
                let pause = node.pause_status();
                if pause != previous_pause {
                    node.emit(ApiEvent::Pause(pause.clone()));
                    previous_pause = pause;
                }
            }
        });
        lock(&self.inner.tasks).push(task);
        let weak = Arc::downgrade(&self.inner);
        *lock(&self.inner.log_listener) = Some(crate::logs::listen(Arc::new(move |line| {
            if let Some(inner) = weak.upgrade() {
                Node { inner }.emit(ApiEvent::Log(line.clone()));
            }
        })));
        Ok(())
    }
    fn sample_rtt(&self, sampled_at: Instant) {
        if let Some(links) = self.inner.links.get() {
            let table = links.links();
            // The existing ordinary-thread sample timer records the published
            // owner snapshots even with no UI subscriber or diagnostic export.
            // Otherwise the recorder retains only join-time RTT/scheduler values.
            if let Some(diagnostics) = self.inner.diagnostics.get() {
                diagnostics.recorder.sample_links(&table);
                diagnostics.recorder.sample_scheduler(&links.scheduler());
            }
            let jitter = self.input_thread().ok().map(|input| input.quality_jitter());
            let now = crate::connection_quality::now_ms_at(sampled_at);
            let quality = {
                let mut history = lock(&self.inner.quality);
                history.record(
                    now,
                    table.links.iter().filter(|l| table.ready(l.peer)).map(|link| {
                        (
                            link.peer,
                            self.device_name(link.peer),
                            match link.path {
                                oink_network::link::LinkPath::Direct => crate::api::LinkKind::Direct,
                                oink_network::link::LinkPath::Relayed { via } => crate::api::LinkKind::Relay {
                                    via: via.into(),
                                    via_name: self.device_name(via),
                                },
                            },
                            link.rtt.map(|rtt| rtt.as_secs_f64() * 1000.),
                            jitter.as_ref().and_then(|j| j.get(&link.peer)).cloned(),
                        )
                    }),
                );
                history.view(now)
            };
            self.emit(ApiEvent::ConnectionQuality(quality));
            for link in &table.links {
                if let Some(rtt) = link.rtt
                    && let Some(generation) = self.inner.reports.ingress.generation()
                {
                    crate::telemetry::projections::distribution_value(
                        &self.inner.reports.ingress,
                        generation,
                        oink_telemetry::Metric::NetworkRtt,
                        rtt.as_micros().min(u64::MAX as u128) as u64,
                        None,
                    );
                }
            }
        }
        if self.has_stats_subscribers() {
            self.emit(ApiEvent::Stats(crate::agent_api::peer_statuses(self)));
        }
    }
}

fn display_changed(dirty: &AtomicBool, observer: &std::sync::OnceLock<std::thread::Thread>) {
    dirty.store(true, Ordering::Release);
    if let Some(thread) = observer.get() {
        thread.unpark();
    }
}

#[cfg(all(test, debug_assertions))]
mod notification_tests {
    use super::*;
    #[cfg(debug_assertions)]
    #[test]
    fn display_callback_wakes_an_owner_parked_past_the_sensor_deadline() {
        let _watchdog = crate::test_watchdog::Scenario::new(
            module_path!(),
            "display_callback_wakes_an_owner_parked_past_the_sensor_deadline",
        );
        let dirty = Arc::new(AtomicBool::new(false));
        let observer = Arc::new(std::sync::OnceLock::new());
        let (ready, started) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let flag = dirty.clone();
        let thread = observer.clone();
        let worker = std::thread::spawn(move || {
            thread.set(std::thread::current()).unwrap();
            ready.send(()).unwrap();
            std::thread::park();
            done.send(flag.load(Ordering::Acquire)).unwrap();
        });
        started.recv().unwrap();
        display_changed(&dirty, &observer);
        let result = finished.recv();
        observer.get().unwrap().unpark();
        worker.join().unwrap();
        assert_eq!(result, Ok(true), "display callback waited for an observation tick");
    }
}

// A stalled executor produces one current sample, never a burst of old ticks.
// Bucket by the fixed sampling schedule, not the executor wake-up time: a
// subsecond early/late wake otherwise becomes duplicate seconds and false gaps.
// After a stall, record only the current bucket; skipped seconds remain missing.
fn statistics_sample_time(deadline: Instant, observed: Instant) -> Instant {
    deadline + Duration::from_secs(observed.saturating_duration_since(deadline).as_secs())
}

pub(crate) fn statistics_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

#[cfg(debug_assertions)]
#[tokio::test(start_paused = true)]
async fn link_statistics_are_source_clocked_without_catch_up_bursts() {
    let mut sample = statistics_interval();
    sample.tick().await; // subscription snapshot/startup
    for _ in 0..60 {
        tokio::time::advance(Duration::from_secs(1)).await;
        sample.tick().await;
        assert!(poll_statistics(&mut sample).is_pending());
    }
    tokio::time::advance(Duration::from_secs(10)).await;
    sample.tick().await;
    assert!(
        poll_statistics(&mut sample).is_pending(),
        "stall emitted catch-up samples"
    );
}

#[cfg(all(test, debug_assertions))]
fn poll_statistics(sample: &mut tokio::time::Interval) -> std::task::Poll<tokio::time::Instant> {
    sample.poll_tick(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

#[cfg(all(test, debug_assertions))]
#[test]
fn statistics_timestamps_follow_deadlines_not_subsecond_wakeup_jitter() {
    let start = Instant::now();
    let mut times = Vec::new();
    for (second, early) in [(1, false), (2, true), (3, false), (4, true)] {
        let deadline = start + Duration::from_secs(second);
        let wake = if early {
            deadline - Duration::from_micros(800)
        } else {
            deadline + Duration::from_micros(264)
        };
        let sample = statistics_sample_time(deadline, wake);
        times.push(sample.saturating_duration_since(start).as_millis());
    }
    assert_eq!(times, [1000, 2000, 3000, 4000]);
}

#[cfg(all(test, debug_assertions))]
#[tokio::test(start_paused = true)]
async fn statistics_deadlines_keep_phase_after_a_late_executor_turn() {
    let start = tokio::time::Instant::now();
    let mut interval = statistics_interval();
    interval.tick().await;
    tokio::time::advance(Duration::from_millis(1009)).await;
    assert_eq!(interval.tick().await, start + Duration::from_secs(1));
    tokio::time::advance(Duration::from_millis(991)).await;
    assert!(
        poll_statistics(&mut interval).is_ready(),
        "timer phase drifted after a late wakeup"
    );
}

#[cfg(all(test, debug_assertions))]
#[test]
fn statistics_stalls_record_only_the_current_bucket_without_backfill() {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(1);
    let wake = start + Duration::from_millis(4200);
    let sample = statistics_sample_time(deadline, wake);
    assert_eq!(sample.duration_since(start), Duration::from_secs(4));
    let peer = oink_foundation::DeviceId([2; 32]);
    let mut history = crate::connection_quality::History::default();
    for at_ms in [1000, sample.duration_since(start).as_millis() as u64] {
        history.record(
            at_ms,
            [(peer, "Peer".into(), crate::api::LinkKind::Direct, Some(4.), None)],
        );
    }
    let view = history.view(4200);
    assert_eq!(
        view.peers[0].samples.iter().map(|s| s.at_ms).collect::<Vec<_>>(),
        [1000, 4000]
    );
}
