use super::*;
use crate::streams::lifecycle_tests::Fixture;
use oink_protocol::v1::schema::{FixedBytes, WireValue};

#[derive(Default)]
struct State {
    encoder: Option<oink_audio::source::Encoder>,
    omitted_silent_frames: u64,
    volume: Option<oink_audio::volume::Reading>,
    volume_writes: usize,
    volume_revision: u64,
    key_failed_after_write: bool,
    volume_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    opened_outputs: Vec<String>,
    events: Vec<&'static str>,
    muted: Vec<bool>,
    permission: Option<CapturePreflight>,
    capture_init_error: Option<String>,
    capture_error: Option<String>,
    local_controls_changed: bool,
    default_output_changed: bool,
    capture_restart: Option<String>,
    capture_restart_hook: Option<Box<dyn FnOnce() + Send>>,
    capture_finish_hook: Option<Box<dyn FnOnce() + Send>>,
    cleanup_error: Option<String>,
    follows_default: bool,
    default_output: Option<String>,
    cleanup_failed: bool,
    playback_error: Option<String>,
    playback_init_error: Option<String>,
    playback_start_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    playback_entered: Option<Arc<tokio::sync::Notify>>,
    realtime_playback: bool,
    playback_core: Option<Arc<Mutex<oink_audio::sink::Simulator>>>,
    playback_invalidated: bool,
    service_generation: Option<u64>,
    device_revision: Option<u64>,
    queries: usize,
    configuration_observer: Option<Arc<dyn Fn() + Send + Sync>>,
}
#[derive(Default)]
struct Platform(Arc<Mutex<State>>);
struct Capture(Arc<Mutex<State>>);

impl CaptureStream for Capture {
    fn omitted_silent_frames(&self) -> u64 {
        lock(&self.0).omitted_silent_frames
    }
    fn finish(&mut self) -> Option<String> {
        let hook = lock(&self.0).capture_finish_hook.take();
        if let Some(hook) = hook {
            hook();
        }
        lock(&self.0).cleanup_error.clone()
    }
    fn default_output_changed(&self) -> bool {
        lock(&self.0).default_output_changed
    }
    fn restart_reason(&self) -> Option<String> {
        let hook = lock(&self.0).capture_restart_hook.take();
        if let Some(hook) = hook {
            hook();
        }
        lock(&self.0).capture_restart.take()
    }
    fn local_controls_changed(&self) -> bool {
        lock(&self.0).local_controls_changed
    }
    fn error(&self) -> Option<String> {
        let observer = lock(&self.0).configuration_observer.clone();
        if let Some(observer) = observer {
            observer();
        }
        lock(&self.0).capture_error.clone()
    }
    fn source_muted(&self) -> bool {
        lock(&self.0).muted.last().copied().unwrap_or(false)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let mut state = lock(&self.0);
        state.encoder = None;
        state.events.push("restore");
    }
}
impl AudioPlatform for Platform {
    fn volume_endpoint(
        &self,
        output: &str,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn oink_audio::volume::Endpoint>, oink_audio::volume::Fault> {
        let mut state = lock(&self.0);
        state.opened_outputs.push(output.into());
        state.volume_wake = Some(wake);
        state.volume.get_or_insert(oink_audio::volume::Reading {
            percent: 55,
            muted: false,
        });
        Ok(Box::new(Endpoint(self.0.clone(), std::cell::Cell::new(0))))
    }
    fn capture_encoded(
        &self,
        encoder: oink_audio::source::Encoder,
        muted: bool,
        _: CaptureScope,
    ) -> Result<Box<dyn CaptureStream>, String> {
        let mut state = lock(&self.0);
        state.muted.push(muted);
        if let Some(error) = &state.capture_init_error {
            return Err(error.clone());
        }
        state.encoder = Some(encoder);
        state.events.push("capture");
        Ok(Box::new(Capture(self.0.clone())))
    }
    fn forwarding_pause(&self) -> oink_audio::media_pause::ForwardingPause {
        let state = self.0.clone();
        oink_audio::media_pause::ForwardingPause::with_backend(move || lock(&state).events.push("pause"))
    }
    fn outputs(&self) -> Result<Vec<OutputDevice>, String> {
        lock(&self.0).queries += 1;
        Ok(vec![OutputDevice {
            id: "synthetic-output".into(),
            name: "Synthetic".into(),
            is_default: true,
        }])
    }
    fn capture_capability(&self) -> String {
        "synthetic".into()
    }
    fn playback(&self, output: &str) -> Result<Box<dyn PlaybackStream>, String> {
        let gate = {
            let mut state = lock(&self.0);
            state.events.push("playback");
            state.opened_outputs.push(output.into());
            if let Some(entered) = &state.playback_entered {
                entered.notify_one();
            }
            state.playback_start_gate.clone()
        };
        if let Some(gate) = gate {
            let (mutex, ready) = &*gate;
            let mut opened = lock(mutex);
            while !*opened {
                opened = ready.wait(opened).unwrap();
            }
        }
        let mut state = lock(&self.0);
        if let Some(error) = &state.playback_init_error {
            return Err(error.clone());
        }
        if state.realtime_playback {
            state.playback_core = Some(Arc::new(Mutex::new(oink_audio::sink::Simulator::new(false))));
        }
        Ok(Box::new(TestPlayback(self.0.clone())))
    }
    fn capture_permission_preflight(&self) -> CapturePreflight {
        lock(&self.0)
            .permission
            .clone()
            .unwrap_or(CapturePreflight::NotDetermined)
    }
    fn permission_requires_signal(&self) -> bool {
        lock(&self.0).permission.is_some()
    }
    fn capture_cleanup_failed(&self) -> bool {
        lock(&self.0).cleanup_failed
    }
    fn service_generation(&self) -> Option<u64> {
        lock(&self.0).service_generation
    }
    fn device_revision(&self) -> Option<u64> {
        lock(&self.0).device_revision
    }
    fn supports_default_following(&self) -> bool {
        lock(&self.0).follows_default
    }
    fn default_output_id(&self) -> Option<String> {
        lock(&self.0).default_output.clone()
    }
}
struct TestPlayback(Arc<Mutex<State>>);
impl PlaybackStream for TestPlayback {
    fn realtime_source(&mut self, config: dsp::Config) -> Result<Option<oink_audio::sink::Source>, String> {
        lock(&self.0)
            .playback_core
            .as_ref()
            .map(|core| lock(core).source(config))
            .transpose()
    }
    fn error(&self) -> Option<String> {
        lock(&self.0).playback_error.clone()
    }
    fn device_invalidated(&self) -> bool {
        lock(&self.0).playback_invalidated
    }
}
impl Drop for TestPlayback {
    fn drop(&mut self) {
        lock(&self.0).events.push("close-output");
    }
}

struct Endpoint(Arc<Mutex<State>>, std::cell::Cell<u64>);
impl oink_audio::volume::Endpoint for Endpoint {
    fn key(&mut self, action: oink_audio::volume::KeyAction) -> Result<(), oink_audio::volume::Fault> {
        let mut state = lock(&self.0);
        let mut reading = state.volume.unwrap();
        match action {
            oink_audio::volume::KeyAction::Up => reading.percent = reading.percent.saturating_add(5).min(100),
            oink_audio::volume::KeyAction::Down => reading.percent = reading.percent.saturating_sub(5),
            oink_audio::volume::KeyAction::Mute => reading.muted = !reading.muted,
        }
        state.volume = Some(reading);
        state.volume_writes += 1;
        if state.key_failed_after_write {
            Err(oink_audio::volume::Fault {
                status: dto::ExitVolumeStatus::BackendFailed,
                native_code: Some(1),
            })
        } else {
            Ok(())
        }
    }

    fn changed(&self) -> bool {
        self.1.get() != lock(&self.0).volume_revision
    }
    fn read(&mut self) -> Result<oink_audio::volume::Reading, oink_audio::volume::Fault> {
        let state = lock(&self.0);
        self.1.set(state.volume_revision);
        state.volume.ok_or_else(oink_audio::volume::Fault::unavailable)
    }
    fn write(&mut self, reading: oink_audio::volume::Reading) -> Result<(), oink_audio::volume::Fault> {
        let mut state = lock(&self.0);
        state.volume = Some(reading);
        state.volume_writes += 1;
        Ok(())
    }
}
struct Running {
    fixture: Fixture<AudioStreams>,
    owners: Vec<Owner>,
    platforms: Vec<Arc<Platform>>,
    directories: Vec<std::path::PathBuf>,
    held_prepared_acks: Option<Vec<(usize, DeviceId, Message)>>,
}
impl Running {
    async fn new() -> Self {
        let mut this = Self::before_reconciliation(true).await;
        this.until(|r| r.owners.iter().all(|o| matches!(o.resolution, Resolution::None { .. })))
            .await;
        this
    }
    async fn before_reconciliation(forwarding_enabled: bool) -> Self {
        let fixture = Fixture::<AudioStreams>::start().await;
        let mut owners = Vec::new();
        let mut platforms = Vec::new();
        let mut directories = Vec::new();
        for index in 0..2 {
            let (links, group) = fixture.context(index);
            let (store, gate) = fixture.owner_store(index);
            let (settings, directory) = {
                let mut config = lock(&store);
                config
                    .update(|settings| {
                        settings.audio.forwarding_enabled = forwarding_enabled;
                        settings.audio.preferred_format = StreamFormat::Pcm16_2p5ms;
                    })
                    .unwrap();
                (
                    config.settings.audio.clone(),
                    config.settings_path().parent().unwrap().to_path_buf(),
                )
            };
            let platform = Arc::new(Platform::default());
            let mut owner = Owner::with_platform(
                OwnerContext {
                    links: links.clone(),
                    group,
                    store,
                    gate,
                },
                settings,
                Arc::new(Mutex::new(Status::default())),
                Arc::default(),
                platform.clone(),
            );
            owner.publish_preferences().unwrap();
            for link in &links.links().links {
                owner.streams.link_ready(link.link, link.peer);
                owner.streams.enable_state_reports(link.peer);
            }
            let devices = owner.discovery.wait().unwrap();
            owner.accept_devices(devices);
            owners.push(owner);
            platforms.push(platform);
            directories.push(directory);
        }
        Self {
            fixture,
            owners,
            platforms,
            directories,
            held_prepared_acks: None,
        }
    }
    async fn start(forward: bool) -> Self {
        let mut this = Self::new().await;
        this.claim(1, true);
        this.until(Self::prepared_route_ready).await;
        if forward {
            this.supply(0, 0.1);
            assert!(this.owners[0].sent.load(Ordering::Relaxed) > 0);
        }
        this
    }
    fn prepared_route_ready(&self) -> bool {
        let source = &self.owners[0];
        let receiver = &self.owners[1];
        // Sending PreparedAck admits capture at the source before the receiver
        // consumes it. Require both ends of this exact preparation, not an old
        // acknowledged stream or the native output (which may already be idle).
        source.capture.is_some()
            && source.source_session.prepared().is_some_and(|prepared| {
                receiver
                    .receive_sessions
                    .get(&source.links.id())
                    .is_some_and(|session| session.ready() && session.prepared() == prepared)
            })
    }
    fn claim(&mut self, index: usize, claimed: bool) {
        let expected = lock(&self.owners[index].store)
            .owner()
            .revision(dto::LocalOption::AudioExitClaim)
            .unwrap();
        let requester = boot(self.owners[index].links.boot());
        let group = self.owners[index].group.applied().group;
        let applied = self.owners[index]
            .set_claim(&requester, group, claimed, expected)
            .unwrap();
        assert_eq!(applied.outcome, dto::ControlOutcome::Applied);
        assert!(applied.persisted);
        assert!(applied.fact_sequence.is_some());
    }
    fn supply(&self, index: usize, level: f32) {
        let now = Instant::now();
        lock(&self.platforms[index].0).encoder.as_mut().unwrap().supply(
            &[level; 240],
            oink_audio::source::Stamp {
                first_sample_at: None,
                callback_at: Some(now),
                delivered_at: now,
                source_frame_position: None,
                discontinuity: false,
            },
        );
    }
    async fn pump(&mut self) {
        for index in 0..self.owners.len() {
            for _ in 0..32 {
                let Ok(Some(event)) =
                    tokio::time::timeout(Duration::from_millis(1), self.fixture.port(index).next()).await
                else {
                    break;
                };
                let owner = &mut self.owners[index];
                match event {
                    ModuleEvent::LinkReady { link, peer } => {
                        owner.streams.link_ready(link, peer);
                        owner.streams.enable_state_reports(peer);
                    }
                    ModuleEvent::LinkLost { link, peer, reason } => owner.link_lost(link, peer, reason),
                    ModuleEvent::Inbound(stream) => owner.streams.inbound(stream),
                    ModuleEvent::InboundLow(op) => owner.streams.inbound_low(*op),
                    ModuleEvent::StreamClosed { stream, reason } => owner.streams.stream_closed(stream, reason),
                    _ => {}
                }
            }
            let owner = &mut self.owners[index];
            owner.apply();
            for (peer, message) in owner.streams.poll(Instant::now()) {
                if matches!(message, Message::AudioPreparedAck(_))
                    && let Some(held) = &mut self.held_prepared_acks
                {
                    held.push((index, peer, message));
                } else {
                    owner.message(peer, message);
                }
            }
            owner.tick();
        }
    }
    async fn until(&mut self, test: impl Fn(&Self) -> bool) {
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                self.pump().await;
                if test(self) {
                    break;
                }
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "synthetic audio owners converge: {:?}",
            self.owners
                .iter()
                .map(|owner| (
                    owner.phase(),
                    &owner.error,
                    owner.playback_failed,
                    owner
                        .playback_retry_at
                        .map(|at| (owner.playback_retry_delay, at.saturating_duration_since(Instant::now()),)),
                    owner.starting_playback.is_some(),
                    owner.playback.is_some(),
                    owner.playback_idle,
                    owner.capture.is_some(),
                    owner.source_session.prepared().is_some(),
                    owner.prepared.len(),
                    owner.receive_sessions.len(),
                ))
                .collect::<Vec<_>>()
        );
    }
    async fn stop(mut self) {
        self.owners.clear();
        self.fixture.stop().await;
    }
}
fn pauses(platform: &Platform) -> usize {
    lock(&platform.0)
        .events
        .iter()
        .filter(|event| **event == "pause")
        .count()
}
fn volume_request(owner: &Owner, id: u8, percent: u8) -> dto::SetExitVolume {
    dto::SetExitVolume::from_raw(
        [
            (1, FixedBytes([id; 16]).write()),
            (2, owner.exit_volume.state().unwrap().version().unwrap().write()),
            (3, percent.write()),
            (4, false.write()),
        ]
        .into(),
    )
    .unwrap()
}

#[tokio::test]
async fn explicit_clear_pauses_once_but_offline_unknown_and_switch_timeout_do_not() {
    for outcome in 0..4 {
        let mut running = Running::start(true).await;
        let old = running.owners[0].resolution.stamp().unwrap();
        let next = match outcome {
            0 => Resolution::None { barrier: Some(old) },
            1 => Resolution::Unknown,
            2 => Resolution::Exit {
                stamp: old,
                available: false,
            },
            _ => Resolution::Exit {
                stamp: Stamp {
                    counter: old.counter + 1,
                    owner: old.owner,
                },
                available: true,
            },
        };
        running.owners[0].transition(next);
        if outcome == 3 {
            assert!(running.owners[0].capture.is_some());
            running.owners[0].exit_switch_deadline = Some(Instant::now());
            running.owners[0].tick_source();
        }
        assert!(running.owners[0].capture.is_none());
        assert_eq!(pauses(&running.platforms[0]), usize::from(outcome == 0));
        running.owners[0].stop_source();
        assert_eq!(pauses(&running.platforms[0]), usize::from(outcome == 0));
        running.stop().await;
    }
}

#[tokio::test]
async fn source_becoming_exit_releases_capture_without_capturing_own_mix_or_pausing() {
    let mut running = Running::start(true).await;
    running.claim(0, true);
    let me = running.owners[0].links.id();
    running
        .until(|r| r.owners.iter().all(|o| o.resolution.owner() == Some(me)))
        .await;
    assert!(running.owners[0].capture.is_none());
    assert!(running.owners[0].exit_volume.state().is_some());
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn muted_forwarding_persists_and_reenable_resets_mute_before_reply() {
    let mut running = Running::start(true).await;
    let owner = &mut running.owners[0];
    let mut settings = owner.settings.clone();
    settings.forwarding_muted = true;
    owner.install_settings(settings).unwrap();
    let enabled = dto::LocalOption::ForwardingEnabled;
    let revision = lock(&owner.store).owner().revision(enabled).unwrap();
    owner
        .set_option(
            &boot(owner.links.boot()),
            owner.group.applied().group,
            enabled,
            revision,
            false.write(),
        )
        .unwrap();
    let revision = lock(&owner.store).owner().revision(enabled).unwrap();
    owner
        .set_option(
            &boot(owner.links.boot()),
            owner.group.applied().group,
            enabled,
            revision,
            true.write(),
        )
        .unwrap();
    assert!(!owner.settings.forwarding_muted);
    assert_eq!(
        owner
            .set_option(
                &boot(owner.links.boot()),
                owner.group.applied().group,
                enabled,
                revision,
                false.write()
            )
            .unwrap_err(),
        dto::ControlFailure::StaleRevision
    );
    let saved = AgentConfig::load_from(running.directories[0].as_path()).unwrap();
    assert_eq!(saved.settings.audio, owner.settings);
    assert!(!running.directories[0].as_path().join("audio/settings.json").exists());
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn gain_keys_cover_both_hid_pages_and_commit_before_publication() {
    let mut running = Running::start(true).await;
    for page in [7, 12] {
        for _ in 0..20 {
            let code = if page == 7 { 0x81 } else { 0xea };
            running.owners[0].volume_key(oink_foundation::input::KeyCode::new(page, code));
        }
        assert_eq!(running.owners[0].settings.forwarding_volume_percent, 0);
        for _ in 0..20 {
            let code = if page == 7 { 0x80 } else { 0xe9 };
            running.owners[0].volume_key(oink_foundation::input::KeyCode::new(page, code));
        }
        assert_eq!(running.owners[0].settings.forwarding_volume_percent, 200);
        let saved = AgentConfig::load_from(running.directories[0].as_path()).unwrap();
        assert_eq!(saved.settings.audio, running.owners[0].settings);
    }
    running.stop().await;
}

#[tokio::test]
async fn local_keyboard_volume_keys_follow_focus_forwarding_mode() {
    volume_keys_follow_focus_forwarding_mode(true).await;
}

#[tokio::test]
async fn remote_keyboard_volume_keys_follow_focus_forwarding_mode() {
    volume_keys_follow_focus_forwarding_mode(false).await;
}

async fn volume_keys_follow_focus_forwarding_mode(local_keyboard: bool) {
    use oink_foundation::input::{CapturedEvent, InjectEvent, InputEvent, KeyCode, SourceKind};
    use oink_input::Disposition;
    use oink_routing::{Os, Output, Packet, Router, RouterConfig, SimpleLayout, input::InputMsg};

    let mut running = Running::start(true).await;
    let focus = running.owners[0].links.boot();
    let exit = running.owners[1].links.boot();
    let mut config = RouterConfig::new(focus, "focus", Os::current());
    config.capture_reports_key_repeat = true;
    let mut router = Router::new(config, Arc::new(SimpleLayout::default()));
    let keyboard = Router::new(
        RouterConfig::new(exit, "keyboard", Os::Other),
        Arc::new(SimpleLayout::default()),
    );
    assert!(router.attach_input(exit, keyboard.routing_session(), true, true, Default::default(), 0));
    for message in keyboard.input_opening(true) {
        assert!(router.input_message(exit.device_id, message, 0));
    }
    let volume = running.owners[0].volume.clone();
    router.set_volume_exit_probe(Arc::new(move || {
        volume
            .load()
            .as_ref()
            .as_ref()
            .cloned()
            .and_then(oink_routing::input::VolumeRoute::from_session)
    }));
    let event = |code, pressed| CapturedEvent {
        event: InputEvent::Key { code, pressed },
        physical: true,
        source: SourceKind::Keyboard,
        time_us: 1,
        cursor: None,
    };
    // Establish local focus through the normal first physical input path.
    router.on_local_event(&event(KeyCode::keyboard(4), true), 1);
    router.on_local_event(&event(KeyCode::keyboard(4), false), 2);
    assert_eq!(router.focus().holder, focus.device_id);
    router.drain_outputs();

    // The remote physical keyboard belongs to the exit. Its forwarding setting
    // must not decide what happens to keys delivered to the focus source.
    let mut settings = running.owners[1].settings.clone();
    settings.forwarding_enabled = false;
    running.owners[1].install_settings(settings).unwrap();
    for keep_local in [false, true] {
        for forwarding in [true, false, true] {
            let mut settings = running.owners[0].settings.clone();
            settings.forwarding_enabled = forwarding;
            settings.keep_local_playback = keep_local;
            running.owners[0].install_settings(settings).unwrap();
            for code in [
                KeyCode::new(12, 0xe9),
                KeyCode::new(12, 0xea),
                KeyCode::new(12, 0xe2),
                KeyCode::keyboard(0x80),
                KeyCode::keyboard(0x81),
                KeyCode::keyboard(0x7f),
            ] {
                let before = lock(&running.platforms[1].0).volume_writes;
                for pressed in [true, false] {
                    if local_keyboard {
                        assert_eq!(
                            router.on_local_event(&event(code, pressed), 3),
                            if forwarding {
                                Disposition::Swallow
                            } else {
                                Disposition::Pass
                            },
                            "forwarding={forwarding}, keep_local={keep_local}, code={code:?}"
                        );
                    } else {
                        assert!(router.input_message(
                            exit.device_id,
                            Packet::Input(InputMsg::Key { code, pressed }),
                            3
                        ));
                    }
                    let mut injected = Vec::new();
                    let mut routed = 0;
                    for output in router.drain_outputs() {
                        match output {
                            Output::Send {
                                to,
                                msg:
                                    Packet::Input(InputMsg::Volume {
                                        route,
                                        press_id,
                                        code: sent,
                                        pressed: down,
                                        repeat,
                                        modifiers,
                                    }),
                                ..
                            } => {
                                assert_eq!(to, exit.device_id);
                                assert_eq!((sent, down), (code, pressed));
                                running.owners[1]
                                    .exit_volume_key(oink_audio::volume::KeyEvent {
                                        source: boot(focus),
                                        session: (*route.session).clone(),
                                        press_id,
                                        action: oink_audio::volume::KeyAction::from_code(sent).unwrap(),
                                        pressed: down,
                                        repeat,
                                        modifiers,
                                    })
                                    .unwrap();
                                routed += 1;
                            }
                            Output::Inject(InjectEvent::Key { code, pressed }) => injected.push((code, pressed)),
                            Output::Status(oink_routing::StatusEvent::AudioVolumeKey { .. }) => {
                                panic!("plain volume must not change source gain")
                            }
                            _ => {}
                        }
                    }
                    assert_eq!(routed, usize::from(forwarding));
                    assert_eq!(
                        injected,
                        if !local_keyboard && !forwarding {
                            vec![(code, pressed)]
                        } else {
                            vec![]
                        }
                    );
                }
                assert_eq!(
                    lock(&running.platforms[1].0).volume_writes - before,
                    usize::from(forwarding)
                );
            }
        }
    }
    running.stop().await;
}

#[tokio::test]
async fn live_volume_works_without_capture_and_publishes_fact_before_success() {
    let mut running = Running::new().await;
    for owner in &mut running.owners {
        let mut settings = owner.settings.clone();
        settings.forwarding_enabled = false;
        owner.install_settings(settings).unwrap();
    }
    running.claim(1, true);
    running.until(|r| r.owners[0].resolved_volume().is_some()).await;
    assert!(running.owners.iter().all(|o| o.capture.is_none()));
    assert!(running.owners.iter().all(|o| o.volume.load().is_none()));
    let requester = boot(running.owners[0].links.boot());
    let request = volume_request(&running.owners[1], 1, 32);
    let group = running.owners[1].group.applied().group;
    let result = running.owners[1]
        .set_exit_volume(&requester, group, request.clone())
        .unwrap();
    assert_eq!(result.outcome().unwrap(), dto::ControlOutcome::Applied);
    let fact = running.owners[1].group.local_facts().exit_volume().unwrap().unwrap();
    assert_eq!(fact, running.owners[1].exit_volume.state().unwrap().clone());
    assert_eq!(fact.volume_percent().unwrap(), Some(32));
    assert_eq!(
        running.owners[1].set_exit_volume(&requester, group, request).unwrap(),
        result
    );
    assert_eq!(lock(&running.platforms[1].0).volume_writes, 1);
    let generation = fact.version().unwrap().output_generation().unwrap();
    running.owners[1].paused = true;
    running.owners[1].refresh_exit_binding(false).unwrap();
    assert_eq!(
        running.owners[1]
            .exit_volume
            .state()
            .unwrap()
            .version()
            .unwrap()
            .output_generation()
            .unwrap(),
        generation
    );
    assert!(running.owners[1].resolved_volume().is_some());
    running.stop().await;
}

#[tokio::test]
async fn output_rebinding_rejects_old_volume_version_and_uses_exact_local_output() {
    let mut running = Running::start(false).await;
    let requester = boot(running.owners[0].links.boot());
    let old = volume_request(&running.owners[1], 1, 32);
    let group = running.owners[1].group.applied().group;
    let generation = running.owners[1]
        .exit_volume
        .state()
        .unwrap()
        .version()
        .unwrap()
        .output_generation()
        .unwrap();
    running.owners[1].refresh_exit_binding(true).unwrap();
    assert!(
        running.owners[1]
            .exit_volume
            .state()
            .unwrap()
            .version()
            .unwrap()
            .output_generation()
            .unwrap()
            > generation
    );
    let result = running.owners[1].set_exit_volume(&requester, group, old).unwrap();
    assert_ne!(result.outcome().unwrap(), dto::ControlOutcome::Applied);
    assert_eq!(lock(&running.platforms[1].0).volume_writes, 0);
    let mut settings = running.owners[1].settings.clone();
    settings.receive_output = Some("missing-output".into());
    running.owners[1].install_settings(settings).unwrap();
    assert!(running.owners[1].bound_output.is_none());
    assert_eq!(running.owners[1].phase(), AudioPhase::Failed);
    assert_eq!(
        running.owners[1].problem().unwrap().code,
        dto::UserFacingCode::DeviceUnavailable
    );
    assert!(
        lock(&running.platforms[1].0)
            .opened_outputs
            .iter()
            .all(|output| output == "synthetic-output")
    );
    running.stop().await;
}

#[tokio::test]
async fn capture_denial_and_unconfirmed_cleanup_are_typed_and_never_pause_players() {
    for denied in [true, false] {
        let mut running = Running::start(true).await;
        if denied {
            running.owners[0].release_capture();
            running.owners[0].needs_generation = false;
            lock(&running.platforms[0].0).permission = Some(CapturePreflight::Denied);
            running.owners[0].tick_source();
            assert_eq!(
                running.owners[0].problem().unwrap().code,
                dto::UserFacingCode::PermissionDenied
            );
        } else {
            lock(&running.platforms[0].0).cleanup_error = Some("synthetic restore failure".into());
            running.owners[0].stop_source();
            assert_eq!(
                running.owners[0].problem().unwrap().code,
                dto::UserFacingCode::CleanupFailed
            );
        }
        assert_eq!(pauses(&running.platforms[0]), 0);
        running.stop().await;
    }
}

#[tokio::test]
async fn external_endpoint_takeover_saves_disabled_reason_before_observation() {
    let mut running = Running::start(true).await;
    {
        let mut state = lock(&running.platforms[0].0);
        state.local_controls_changed = true;
        state.default_output_changed = true;
    }
    running.owners[0].tick();
    assert!(!running.owners[0].settings.forwarding_enabled);
    assert_eq!(
        running.owners[0].settings.forwarding_off_reason,
        Some(oink_audio::ForwardingOffReason::LocalVolumeChanged)
    );
    assert_eq!(
        AgentConfig::load_from(running.directories[0].as_path())
            .unwrap()
            .settings
            .audio,
        running.owners[0].settings
    );
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[test]
fn repeated_no_frame_capture_probes_back_off_and_valid_frames_reset_the_deadline() {
    let mut watchdog = CaptureWatchdog::default();
    let mut now = Instant::now();
    for _ in 0..8 {
        watchdog.opened(now);
        let deadline = now + watchdog.timeout;
        assert!(!watchdog.stalled(deadline - Duration::from_nanos(1), 0));
        assert!(watchdog.stalled(deadline, 0));
        assert!(watchdog.timeout <= Duration::from_secs(10));
        now = deadline;
    }
    watchdog.opened(now);
    assert!(!watchdog.stalled(now, 240)); // includes all-zero native PCM
    assert_eq!(watchdog.timeout, Duration::from_millis(500));
}

#[tokio::test]
async fn native_silent_frame_omission_keeps_capture_dormant_without_reopening() {
    let mut running = Running::start(false).await;
    running.supply(0, 0.0);
    let now = Instant::now();
    running.owners[0].tick_source_at(now);
    let opens = lock(&running.platforms[0].0).muted.len();
    lock(&running.platforms[0].0)
        .encoder
        .as_mut()
        .unwrap()
        .tick(now + oink_audio::source::SILENCE_TIMEOUT);
    assert!(running.owners[0].encoded.as_ref().unwrap().sleeping());
    let sent = running.owners[0].sent.load(Ordering::Relaxed);
    // Match the native tap's optimisation: callbacks continue, but exact-zero
    // frames after the five-second tail do not enter the PCM ring/encoder.
    for second in 5..=65 {
        // Keep real QUIC/control queues moving while only the capture-health
        // clock advances; otherwise a full control queue is a different fault.
        running.pump().await;
        lock(&running.platforms[0].0).omitted_silent_frames += 48_000;
        running.owners[0].tick_source_at(now + Duration::from_secs(second));
        assert!(
            running.owners[0].capture.is_some(),
            "healthy silent tap was reopened at {second}s: {:?}",
            running.owners[0].restart_reason
        );
        assert_eq!(running.owners[0].phase(), AudioPhase::SoundSleep);
        assert_eq!(running.owners[0].sent.load(Ordering::Relaxed), sent);
        assert_eq!(lock(&running.platforms[0].0).muted.len(), opens);
    }
    // Onset bypasses the sleeping control owner; no reopen or owner tick is needed.
    let wake = now + Duration::from_secs(66);
    lock(&running.platforms[0].0).encoder.as_mut().unwrap().supply(
        &[0.2; 240],
        oink_audio::source::Stamp {
            first_sample_at: None,
            callback_at: Some(wake),
            delivered_at: wake,
            source_frame_position: None,
            discontinuity: false,
        },
    );
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
    assert!(!running.owners[0].encoded.as_ref().unwrap().sleeping());
    assert_eq!(lock(&running.platforms[0].0).muted.len(), opens);
    running.owners[0].tick_source_at(wake);
    // Silence is not an exemption for a genuinely stalled native producer.
    lock(&running.platforms[0].0)
        .encoder
        .as_mut()
        .unwrap()
        .tick(wake + oink_audio::source::SILENCE_TIMEOUT);
    running.owners[0].tick_source_at(wake + oink_audio::source::SILENCE_TIMEOUT);
    assert!(running.owners[0].capture.is_none());
    running.until(Running::prepared_route_ready).await;
    let sent = running.owners[0].sent.load(Ordering::Relaxed);
    running.supply(0, 0.2);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
    running.stop().await;
}

#[tokio::test]
async fn no_frames_capture_reopens_but_silent_frames_are_progress() {
    let mut running = Running::start(true).await;
    let now = Instant::now();
    running.owners[0].tick_source_at(now);
    for step in 1..=3 {
        running.supply(0, 0.0);
        running.owners[0].tick_source_at(now + Duration::from_millis(step * 400));
        assert!(running.owners[0].capture.is_some(), "zero PCM is still frame progress");
    }
    running.owners[0].tick_source_at(now + Duration::from_millis(1800));
    assert!(
        running.owners[0].capture.is_none(),
        "no frame progress must retire capture"
    );
    assert!(running.owners[0].settings.forwarding_enabled);
    assert_ne!(running.owners[0].phase(), AudioPhase::Forwarding);
    running.until(Running::prepared_route_ready).await;
    let sent = running.owners[0].sent.load(Ordering::Relaxed);
    running.supply(0, 0.1);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
    running.stop().await;
}

#[tokio::test]
async fn media_health_does_not_blame_the_exit_for_an_explicit_source_failure() {
    let mut running = Running::start(true).await;
    let source = running.owners[0].links.id();
    running.owners[0].fail_source("capture permission denied".into());
    running
        .until(|r| {
            r.owners[1]
                .peer_states
                .get(&source)
                .is_some_and(|(state, _)| state.phase == AudioPhase::Failed)
        })
        .await;
    running.owners[1].tick_exit_at(Instant::now() + Duration::from_secs(9));
    assert!(!running.owners[1].playback_failed);
    assert!(running.owners[0].source_failed);
    running.stop().await;
}

#[tokio::test]
async fn media_stall_faults_the_current_receive_generation() {
    let mut running = Running::start(true).await;
    let source = running.owners[0].links.id();
    running.until(|r| r.owners[1].receivers.contains_key(&source)).await;
    running.owners[1].tick_exit_at(Instant::now() + Duration::from_secs(9));
    assert!(
        running.owners[1].playback_failed,
        "missing media without Sleep is not healthy playback"
    );
    assert_eq!(running.owners[1].phase(), AudioPhase::Failed);
    assert!(running.owners[1].problem().is_some());
    let old_stream = running.owners[0].selected.unwrap().generation;
    let retry = running.owners[1].playback_retry_at.unwrap();
    running.owners[1].tick_exit_at(retry);
    running
        .until(|r| r.prepared_route_ready() && r.owners[0].selected.unwrap().generation != old_stream)
        .await;
    let sent = running.owners[0].sent.load(Ordering::Relaxed);
    running.supply(0, 0.1);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
    running.stop().await;
}

#[tokio::test]
async fn terminal_capture_retirement_never_reuses_an_ended_send_ticket() {
    for (during_finish, reported_error) in [(false, true), (true, true), (true, false)] {
        let mut running = Running::start(true).await;
        let ticket = running.owners[0].forwarding_pause.clone();
        let changed = running.platforms[0].0.clone();
        let fail: Box<dyn FnOnce() + Send> = Box::new(move || {
            ticket.stop();
            lock(&changed).capture_error = reported_error.then(|| "native capture terminated".into());
        });
        if during_finish {
            lock(&running.platforms[0].0).capture_finish_hook = Some(fail);
        } else {
            fail();
        }
        // Both AudioPrepared replacement and an empty transport table retire
        // capture before tick_source's ordinary error check.
        running.owners[0].release_capture();
        assert!(
            !running.owners[0].forwarding_pause.is_ended(),
            "replacement needs a live ticket"
        );
        assert_eq!(
            running.owners[0].restart_reason.as_deref(),
            Some(if reported_error {
                "native capture terminated"
            } else {
                "capture send ticket ended"
            })
        );
        lock(&running.platforms[0].0).capture_error = None;
        running.until(Running::prepared_route_ready).await;
        let sent = running.owners[0].sent.load(Ordering::Relaxed);
        running.supply(0, 0.1);
        assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
        running.stop().await;
    }
}

#[tokio::test]
async fn default_change_during_capture_retirement_switches_to_local_before_reopen() {
    use crate::device_cards::{SpeakerActivity, SpeakerMode};
    for (during_finish, retired_before_tick) in [(false, false), (true, false), (true, true)] {
        let mut running = Running::start(true).await;
        let claim = running.owners[0].resolution;
        let ticket = running.owners[0].forwarding_pause.clone();
        let state = running.platforms[0].0.clone();
        let changed = state.clone();
        // The default notification lands after tick_running's first check,
        // either while reading restart_reason or while joining the old worker.
        // A muted WASAPI default change ends this exact logical ticket.
        let notify: Box<dyn FnOnce() + Send> = Box::new(move || {
            lock(&changed).default_output_changed = true;
            ticket.stop();
        });
        {
            let mut state = lock(&state);
            state.capture_restart = Some("default audio output changed".into());
            if during_finish {
                state.capture_finish_hook = Some(notify);
            } else {
                state.capture_restart_hook = Some(notify);
            }
        }
        if retired_before_tick {
            // A control message can retire capture before the gated tick. The
            // final native flags must survive that handle's destruction too.
            running.owners[0].release_capture();
            assert!(lock(&running.owners[0].store).settings.audio.forwarding_enabled);
        }
        running.owners[0].tick();
        let owner = &running.owners[0];
        assert!(
            !owner.settings.forwarding_enabled,
            "late default change must turn forwarding off"
        );
        assert_eq!(
            owner.settings.forwarding_off_reason,
            Some(oink_audio::ForwardingOffReason::DefaultOutputChanged)
        );
        assert_eq!(owner.phase(), AudioPhase::Idle);
        assert!(owner.capture.is_none());
        assert!(!lock(&owner.state).source_muted);
        assert_eq!(owner.resolution, claim);
        assert_eq!(
            AgentConfig::load_from(&running.directories[0]).unwrap().settings.audio,
            owner.settings
        );
        let source = owner.links.id();
        let nonce = owner.links.boot().nonce;
        running
            .until(|r| {
                r.owners.iter().all(|owner| {
                    let card = speaker_card(&owner.group, source, nonce, true);
                    card.forwarding_enabled == Some(false)
                        && card.resource.configured_mode == Some(SpeakerMode::Unused)
                        && card.activity == SpeakerActivity::Idle
                })
            })
            .await;
        assert_eq!(
            lock(&state).muted,
            [true],
            "never open/mute the replacement default automatically"
        );
        assert_eq!(pauses(&running.platforms[0]), 0);
        // Explicit re-enable is the recovery action. The replacement must have
        // a live ticket and actually submit media, not merely own a capture.
        {
            let mut state = lock(&state);
            state.default_output_changed = false;
            state.capture_restart = None;
        }
        let mut settings = running.owners[0].settings.clone();
        settings.forwarding_enabled = true;
        running.owners[0].install_settings(settings).unwrap();
        running.until(Running::prepared_route_ready).await;
        let sent = running.owners[0].sent.load(Ordering::Relaxed);
        running.supply(0, 0.1);
        assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
        assert_eq!(running.owners[0].settings.forwarding_off_reason, None);
        running.stop().await;
    }
}

#[tokio::test]
async fn typed_remote_fault_preserves_code_and_delayed_old_context_cannot_stop_new_session() {
    let mut running = Running::start(true).await;
    let session = running.owners[0].source_session.prepared().unwrap().session();
    let fault = dto::AudioRouteFault::from_raw(
        [
            (1, session.write()),
            (
                3,
                Problem::new(dto::UserFacingCode::DeviceUnavailable).to_wire().write(),
            ),
        ]
        .into(),
    )
    .unwrap();
    let peer = running.owners[1].links.id();
    running.owners[0].message(peer, Message::AudioRouteFault(fault.clone()));
    assert!(running.owners[0].capture.is_none());
    assert_eq!(
        running.owners[0].problem().unwrap().code,
        dto::UserFacingCode::DeviceUnavailable
    );
    assert_eq!(pauses(&running.platforms[0]), 0);
    assert!(!running.owners[0].source_failed);
    running.until(|r| r.owners[0].capture.is_some()).await;
    let current = running.owners[0].source_session.prepared().unwrap().session();
    assert_ne!(session, current);
    running.owners[0].message(peer, Message::AudioRouteFault(fault));
    assert!(running.owners[0].capture.is_some());
    running.stop().await;
}

#[test]
fn another_sources_busy_recovery_lock_is_not_our_unreleased_mute() {
    use oink_audio::recovery::{EndpointState, Journal};
    let dir = tempfile::tempdir().unwrap();
    let busy = || Some("another process owns endpoint mute/recovery".to_string());
    assert!(
        startup_recovery_error(dir.path(), busy()).is_none(),
        "no own mute record"
    );
    let mut journal = Journal::open(dir.path()).unwrap();
    journal
        .prepare(EndpointState {
            endpoint: "test-source".into(),
            muted: false,
            volume: 0.5,
        })
        .unwrap();
    drop(journal);
    assert!(
        startup_recovery_error(dir.path(), busy()).is_some(),
        "own pending record still needs recovery"
    );
    assert!(
        startup_recovery_error(dir.path(), None).is_none(),
        "a retained unavailable endpoint alone is not a cleanup error"
    );
    let mut journal = Journal::open(dir.path()).unwrap();
    journal.revoke().unwrap();
    drop(journal);
    assert!(
        startup_recovery_error(dir.path(), busy()).is_none(),
        "revoked ownership is not an unresolved mute"
    );
    std::fs::write(dir.path().join("audio-mute-recovery.json"), "corrupt").unwrap();
    assert!(
        startup_recovery_error(dir.path(), busy()).is_some(),
        "unreadable own state stays conservative"
    );
}

mod preparation;
mod statistics;

#[tokio::test]
async fn permission_preflight_blocks_automatic_prompt_then_grant_rearms_source() {
    let mut running = Running::start(false).await;
    running.owners[0].release_capture();
    running.owners[0].needs_generation = false;
    lock(&running.platforms[0].0).muted.clear();
    for preflight in [
        CapturePreflight::NotDetermined,
        CapturePreflight::Denied,
        CapturePreflight::Unavailable("missing SPI".into()),
    ] {
        lock(&running.platforms[0].0).permission = Some(preflight.clone());
        running.owners[0].accept_permission(preflight.clone());
        assert_eq!(
            running.owners[0].permission,
            if preflight == CapturePreflight::Denied {
                CapturePermission::Denied
            } else {
                CapturePermission::Unknown
            }
        );
        running.owners[0].source_failed = false;
        running.owners[0].tick_source();
        assert!(running.owners[0].source_failed);
        assert_eq!(
            running.owners[0].problem().unwrap().code,
            if preflight == CapturePreflight::Denied {
                dto::UserFacingCode::PermissionDenied
            } else {
                dto::UserFacingCode::PermissionRequired
            }
        );
        assert!(running.owners[0].capture.is_none());
        assert!(
            lock(&running.platforms[0].0).muted.is_empty(),
            "preflight must never open a tap"
        );
    }
    lock(&running.platforms[0].0).permission = Some(CapturePreflight::Authorized);
    running.owners[0].accept_permission(CapturePreflight::Authorized);
    assert!(!running.owners[0].source_failed);
    assert!(running.owners[0].error.is_none());
    running.owners[0].tick_source();
    assert_eq!(lock(&running.platforms[0].0).muted, [true]);
    running.supply(0, 0.1);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > 0);
    running.owners[0].fail_source("device disconnected".into());
    running.owners[0].accept_permission(CapturePreflight::Authorized);
    assert!(running.owners[0].source_failed, "grants must not reset device failures");
    running.stop().await;
}

#[tokio::test]
async fn keep_local_rebuilds_once_and_preserves_forwarding_claim() {
    let mut running = Running::start(true).await;
    let claim = running.owners[0].resolution;
    let mut settings = running.owners[0].settings.clone();
    settings.keep_local_playback = true;
    running.owners[0].install_settings(settings).unwrap();
    running.until(|r| r.owners[0].capture.is_some()).await;
    assert_eq!(running.owners[0].resolution, claim);
    assert_eq!(lock(&running.platforms[0].0).muted, [true, false]);
    let ticket = running.owners[0].forwarding_pause.clone();
    let changed = running.platforms[0].0.clone();
    {
        let mut state = lock(&running.platforms[0].0);
        state.capture_restart = Some("default audio output changed".into());
        state.capture_restart_hook = Some(Box::new(move || lock(&changed).default_output_changed = true));
    }
    running.until(Running::prepared_route_ready).await;
    assert!(running.owners[0].settings.forwarding_enabled);
    assert_eq!(running.owners[0].settings.forwarding_off_reason, None);
    assert!(
        !ticket.is_ended(),
        "keep-local default following preserves the logical ticket"
    );
    assert_eq!(lock(&running.platforms[0].0).muted, [true, false, false]);
    let sent = running.owners[0].sent.load(Ordering::Relaxed);
    running.supply(0, 0.1);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn audio_service_restart_retires_ready_volume_until_fresh_output_observation() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    owner.service_generation = Some(1);
    lock(&running.platforms[1].0).service_generation = Some(2);
    let old = owner
        .exit_volume
        .state()
        .unwrap()
        .version()
        .unwrap()
        .output_generation()
        .unwrap();
    owner.observe_service_restart();
    owner.refresh_exit_binding(false).unwrap();
    assert!(owner.exit_volume.state().is_none());
    assert!(owner.awaiting_output_snapshot);
    owner.accept_devices(Ok(DeviceSnapshot {
        service_generation: Some(2),
        outputs: vec![OutputDevice {
            id: "synthetic-output".into(),
            name: "Synthetic".into(),
            is_default: true,
        }],
        latencies: vec![],
        capture: "synthetic".into(),
    }));
    owner.refresh_exit_binding(false).unwrap();
    assert!(
        owner
            .exit_volume
            .state()
            .unwrap()
            .version()
            .unwrap()
            .output_generation()
            .unwrap()
            > old
    );
    running.stop().await;
}

fn post_media(
    sender: &dyn crate::streams::audio::transport::MediaSender,
    stream: u64,
    burst: u64,
    sequence: u64,
    sleep: bool,
    sample: i16,
) {
    let now = Instant::now();
    let mut payload = [0; dsp::MAX_PACKET_BYTES];
    for frame in payload[..480].as_chunks_mut::<2>().0 {
        frame.copy_from_slice(&sample.to_le_bytes());
    }
    let packet = oink_audio::source::Packet {
        ordinal: sequence,
        burst,
        sleep,
        payload,
        len: 480,
        silent: sample == 0,
        delivered_at: now,
        encode_start: now,
        encode_end: now,
        remaining_samples: 0,
    };
    let mut bytes = [0; oink_audio::media::FRAME_CAPACITY];
    let len = oink_audio::media::encode(&packet, stream, now, &mut bytes).unwrap();
    sender.post(&bytes[..len]).unwrap();
}

#[tokio::test]
async fn silence_closes_only_after_all_sources_and_wake_preserves_startup_backlog() {
    use crate::streams::audio::ForwardFrom;
    use crate::streams::audio::transport::testing::datagram_pair;
    use oink_network::link::{LinkId, QueueSpec};
    use oink_protocol::frame::{StreamKind, StreamTag};
    let mut running = Running::start(false).await;

    let table = running.owners[1].streams.forward_table();
    let mut writers = Vec::new();
    let mut from = Vec::new();
    for index in 0..2_u8 {
        let peer = DeviceId([30 + index; 32]);
        let (tx, rx) = datagram_pair(
            LinkId(100 + u64::from(index)),
            StreamTag {
                kind: StreamKind::Audio,
                ordinal: 1,
            },
            QueueSpec::datagram(StreamKind::Audio),
        );
        from.push(ForwardFrom {
            admission: Default::default(),
            peer,
            stream_id: 7,
            format: StreamFormat::Pcm16_2p5ms,
            gain: 1.0,
            playing: true,
            packets: rx,
        });
        writers.push((peer, tx));
    }
    table.publish(ForwardTable { to: vec![], from });
    running.owners[1].tick_exit();
    // Preparation may already have drained its silent output before the peer
    // receives its ACK. Establish a live output with real onset packets first.
    // The fake sink has no RT reader, so perform its read/consume explicitly.
    for (peer, tx) in &writers {
        post_media(tx.as_ref(), 7, 1, 1, false, 100);
        let receiver = running.owners[1].receivers.get_mut(peer).unwrap();
        let packet = receiver.reader.read().1.unwrap();
        receiver.backlog.push_back(packet);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.owners[1].tick_exit();
            if running.owners[1].playback.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("onset wakes the prepared output");
    for receiver in running.owners[1].receivers.values_mut() {
        receiver.backlog.clear();
    }
    let prepared = running.owners[1].prepared.clone();
    for (index, (peer, tx)) in writers.iter().enumerate() {
        post_media(tx.as_ref(), 7, 1, 20, true, 0);
        let reader = &running.owners[1].receivers[peer].reader;
        assert!(reader.read().1.is_none()); // the fake output has no native RT thread
        running.owners[1].tick_exit();
        assert!(running.owners[1].playback.is_some());
        assert_eq!(running.owners[1].draining, index == 1);
    }
    running.owners[1].tick_exit(); // fake device acknowledges its zero drain
    assert!(running.owners[1].playback.is_none());
    assert!(running.owners[1].playback_idle);
    assert_eq!(running.owners[1].prepared, prepared, "idle is not a disconnect");
    let starts = lock(&running.platforms[1].0)
        .events
        .iter()
        .filter(|event| **event == "playback")
        .count();
    for _ in 0..4 {
        running.owners[1].tick_exit();
    }
    assert_eq!(
        lock(&running.platforms[1].0)
            .events
            .iter()
            .filter(|event| **event == "playback")
            .count(),
        starts,
        "exit maintenance must not reopen a dormant output"
    );
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    lock(&running.platforms[1].0).playback_start_gate = Some(gate.clone());
    let (peer, tx) = &writers[0];
    let before = Instant::now();
    for sequence in 1..=12_u64 {
        post_media(tx.as_ref(), 7, 2, sequence, false, sequence as i16 * 100);
        running.owners[1].tick_exit();
    }
    let after = Instant::now();
    let receiver = &running.owners[1].receivers[peer];
    assert_eq!(receiver.backlog.len(), 12);
    assert!(
        receiver
            .backlog
            .iter()
            .all(|packet| packet.arrived >= before && packet.arrived <= after)
    );
    assert!(running.owners[1].starting_playback.is_some());
    *lock(&gate.0) = true;
    gate.1.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while running.owners[1].playback.is_none() {
            running.owners[1].tick_exit();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!running.owners[1].playback_idle);
    assert!(!running.owners[1].playback_failed);
    assert_eq!(running.owners[1].prepared, prepared);
    // The fake output has no real port, so inspect the complete pending handover.
    // sink::wake_tests runs these buffered packets through the real DSP without a device.
    for (index, packet) in running.owners[1].receivers[peer].backlog.iter().enumerate() {
        assert_eq!(packet.sequence, index as u64 + 1);
        assert_eq!(
            i16::from_le_bytes(packet.payload[..2].try_into().unwrap()),
            (index as i16 + 1) * 100
        );
    }
    running.stop().await;
}

fn render_test_exit(running: &Running, at: Instant) -> [f32; 256] {
    let core = lock(&running.platforms[1].0).playback_core.clone().unwrap();
    let mut core = lock(&core);
    let mut output = [0.; 256];
    crate::allocation_audit::assert_no_heap(|| core.fill(&mut output, 48_000, 2, 128, at, None));
    output
}

async fn prepared_opus_remote_exit() -> Running {
    let mut running = Running::new().await;
    lock(&running.platforms[1].0).realtime_playback = true;
    for owner in &mut running.owners {
        let mut settings = owner.settings.clone();
        settings.preferred_format = StreamFormat::Opus5ms;
        owner.install_settings(settings).unwrap();
    }
    running.claim(1, true);
    running.until(Running::prepared_route_ready).await;
    running
}

async fn sleeping_opus_remote_exit() -> Running {
    let mut running = prepared_opus_remote_exit().await;
    running.supply(0, 0.);
    running.supply(0, 0.);
    // Advance the real source encoder's no-buffer silence timer by six seconds;
    // no native devices or wall-clock audio playback are involved in this test.
    lock(&running.platforms[0].0)
        .encoder
        .as_mut()
        .unwrap()
        .tick(Instant::now() + oink_audio::source::SILENCE_TIMEOUT + Duration::from_secs(1));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            running.pump().await;
            render_test_exit(&running, Instant::now());
            if running.owners[1].playback_idle
                && running.owners[1].starting_playback.is_none()
                && running.owners[1]
                    .receivers
                    .values()
                    .all(|receiver| receiver.backlog.is_empty() && receiver.reader.bursts.asleep(Instant::now()))
            {
                break;
            }
        }
    })
    .await
    .expect("remote exit sleeps after the source silence event");
    assert!(running.owners[0].capture.is_some());
    assert!(running.owners[0].encoded.as_ref().unwrap().sleeping());
    running
}

// Keep M0 preparation real, but drive M1 synchronously for backlog-capacity
// assertions. An async pump per packet makes the amount supplied in three
// seconds depend on OS timer granularity, network scheduling and other tests.
async fn queued_sleeping_opus_remote_exit() -> Running {
    use crate::streams::audio::{ForwardFrom, ForwardTo};
    use oink_network::link::{QueueSpec, testing::partitioned_datagram_pair};

    let mut running = prepared_opus_remote_exit().await;
    let outgoing = running.owners[0].streams.forward_table();
    let incoming = running.owners[1].streams.forward_table();
    let to = outgoing.load();
    let from = incoming.load();
    assert_eq!(to.to.len(), 1);
    assert_eq!(from.from.len(), 1);
    let to = &to.to[0];
    let from = &from.from[0];
    let stream = from.packets.follows();
    let (tx, [rx, _]) = partitioned_datagram_pair(
        stream.link,
        stream.tag,
        QueueSpec::datagram(stream.tag.kind),
        [[0x0604, 0x0606], [0x0618, 0x0619]],
    );
    outgoing.publish(ForwardTable {
        to: vec![ForwardTo {
            packets: Arc::new(tx),
            peer: to.peer,
            stream_id: to.stream_id,
            format: to.format,
            gain: to.gain,
        }],
        from: vec![],
    });
    incoming.publish(ForwardTable {
        to: vec![],
        from: vec![ForwardFrom {
            packets: Arc::new(rx),
            admission: from.admission.clone(),
            peer: from.peer,
            stream_id: from.stream_id,
            format: from.format,
            gain: from.gain,
            playing: from.playing,
        }],
    });
    // No media has been supplied yet. Bind the reader to the synchronous M1
    // before exercising the real encoder's Sleep and subsequent wake.
    running.owners[1].receivers.clear();
    let at = Instant::now();
    // No-buffer silence emits only Sleep. Supplying a silent audio frame here
    // could start a new open worker if preparation already idled the output.
    lock(&running.platforms[0].0)
        .encoder
        .as_mut()
        .unwrap()
        .tick(at + oink_audio::source::SILENCE_TIMEOUT + Duration::from_secs(1));
    for tick in 0..4 {
        running.owners[1].tick_exit_at(at);
        render_test_exit(&running, at + Duration::from_nanos(tick * 2_666_667));
    }
    assert!(running.owners[1].playback_idle);
    assert!(running.owners[1].starting_playback.is_none());
    assert!(
        running.owners[1]
            .receivers
            .values()
            .all(|receiver| receiver.backlog.is_empty() && receiver.reader.bursts.asleep(at))
    );
    assert!(running.owners[0].encoded.as_ref().unwrap().sleeping());
    running
}

fn supply_opus_at(running: &Running, level: f32, at: Instant) {
    // A whole 5 ms stereo frame avoids the encoder's intentional >20 ms stale
    // partial-frame discard if the test is descheduled between half frames.
    lock(&running.platforms[0].0).encoder.as_mut().unwrap().supply(
        &[level; 480],
        oink_audio::source::Stamp {
            first_sample_at: None,
            callback_at: Some(at),
            delivered_at: at,
            source_frame_position: None,
            discontinuity: false,
        },
    );
}

#[tokio::test]
async fn slow_remote_exit_wake_preserves_backlog_larger_than_receiver_capacity() {
    let mut running = queued_sleeping_opus_remote_exit().await;
    let peer = running.owners[0].links.id();
    let session = running.owners[0].source_session.prepared().unwrap().session();
    let quality_id = running.owners[1].receivers[&peer].reader.quality_id;
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    lock(&running.platforms[1].0).playback_start_gate = Some(gate.clone());
    let supply_at = Instant::now();
    // Wake emits two pre-roll packets plus this first complete Opus frame.
    supply_opus_at(&running, 0.2, supply_at);
    running.owners[1].tick_exit_at(supply_at);
    assert_eq!(running.owners[1].receivers[&peer].backlog.len(), 3);
    for packet in 3..60 {
        let at = supply_at + Duration::from_millis((packet - 2) * 5);
        supply_opus_at(&running, 0.2, at);
        running.owners[1].tick_exit_at(at);
    }
    // 60 five-ms packets fit the owner's fixed 64-slot startup backlog, but
    // exceed the production Opus receiver's 54-packet handoff/DSP budget.
    let backlog = running.owners[1].receivers[&peer].backlog.len() as u64;
    assert_eq!(backlog, 60);
    for (index, packet) in running.owners[1].receivers[&peer].backlog.iter().enumerate() {
        assert_eq!(packet.sequence, index as u64 + 1);
    }
    assert!(running.owners[1].starting_playback.is_some());
    *lock(&gate.0) = true;
    gate.1.notify_one();
    let wake = running.owners[1].streams.wake();
    // This timeout only guards worker completion; media supply and rendering
    // have fixed packet/sample counts and never race a wall-clock deadline.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            running.owners[1].tick_exit();
            if running.owners[1].starting_playback.is_none() {
                break;
            }
            wake.notified().await;
        }
    })
    .await
    .expect("released playback worker completes");
    assert!(
        !running.owners[1].playback_failed,
        "a full startup handoff is backpressure, not a device failure: {:?}",
        running.owners[1].error
    );
    assert!(!running.owners[1].receivers[&peer].backlog.is_empty());
    let at = Instant::now();
    let mut audible = false;
    let mut tick = 0;
    while tick < 240 {
        let output = render_test_exit(&running, at + Duration::from_nanos(tick * 2_666_667));
        audible |= output.iter().any(|sample| sample.abs() > 0.01);
        if tick % 8 == 0 {
            running.owners[1].tick_exit();
        }
        tick += 1;
        let receiver = running.owners[1].receivers.get_mut(&peer).unwrap();
        let source = receiver.source.as_mut().unwrap();
        source.poll();
        if receiver.backlog.is_empty() && source.stats().received == backlog && audible {
            break;
        }
    }
    let receiver = running.owners[1].receivers.get_mut(&peer).unwrap();
    assert!(receiver.backlog.is_empty());
    let source = receiver.source.as_mut().unwrap();
    source.poll();
    assert_eq!(source.stats().received, backlog);
    assert_eq!(source.stats().decode_errors, 0);
    assert_eq!(
        receiver.reader.quality_id, quality_id,
        "sleep/wake keeps the quality session"
    );
    assert!(audible, "the resumed source reaches the real pull/Opus/mix core");
    assert_eq!(running.owners[0].source_session.prepared().unwrap().session(), session);
    assert!(running.owners[0].capture.is_some());
    assert!(!running.owners[1].playback_failed);
    // After the startup handoff, newly arriving media must be admitted by the
    // real M1 reader again, in the same session. Leave space for one new packet.
    for _ in 0..4 {
        render_test_exit(&running, at + Duration::from_nanos(tick * 2_666_667));
        tick += 1;
    }
    let bytes = running.owners[1].receivers[&peer].reader.payload_bytes();
    supply_opus_at(&running, 0.3, supply_at + Duration::from_millis(300));
    for _ in 0..8 {
        running.owners[1].tick_exit();
        render_test_exit(&running, at + Duration::from_nanos(tick * 2_666_667));
        tick += 1;
    }
    let receiver = running.owners[1].receivers.get_mut(&peer).unwrap();
    let source = receiver.source.as_mut().unwrap();
    source.poll();
    assert_eq!(source.stats().received, backlog + 1);
    assert!(receiver.reader.payload_bytes() > bytes);
    running.stop().await;
}

#[tokio::test]
async fn failed_remote_exit_wake_recovers_without_a_new_user_claim() {
    let mut running = sleeping_opus_remote_exit().await;
    let claim = running.owners[1].resolution.stamp();
    let old_session = running.owners[0].source_session.prepared().unwrap().session();
    lock(&running.platforms[1].0).playback_init_error = Some("synthetic transient wake failure".into());
    running.supply(0, 0.2);
    running.supply(0, 0.2);
    running.until(|r| r.owners[1].playback_failed).await;
    assert_eq!(
        running.owners[1].problem().unwrap().code,
        dto::UserFacingCode::DeviceUnavailable
    );
    let starts = lock(&running.platforms[1].0)
        .events
        .iter()
        .filter(|e| **e == "playback")
        .count();
    let due = running.owners[1].playback_retry_at.unwrap();
    assert_eq!(running.owners[1].playback_retry_delay, PLAYBACK_RETRY_INITIAL * 2);
    // Eight async pumps can exceed the shared 250 ms initial retry interval
    // on Windows. Check the deadline explicitly rather than scheduler speed.
    for _ in 0..8 {
        running.owners[1].tick_exit_at(due - Duration::from_nanos(1));
    }
    assert_eq!(
        lock(&running.platforms[1].0)
            .events
            .iter()
            .filter(|e| **e == "playback")
            .count(),
        starts,
        "failed opens must back off, not spin"
    );
    lock(&running.platforms[1].0).playback_init_error = None;
    running.owners[1].tick_exit_at(due);
    running
        .until(|r| !r.owners[1].playback_failed && r.prepared_route_ready())
        .await;
    assert_eq!(running.owners[1].resolution.stamp(), claim);
    assert_ne!(
        running.owners[0].source_session.prepared().unwrap().session(),
        old_session
    );
    assert!(running.owners[1].problem().is_none());
    running.stop().await;
}

#[tokio::test]
async fn remote_exit_wake_overflow_waits_for_native_open_before_retrying() {
    let mut running = queued_sleeping_opus_remote_exit().await;
    let peer = running.owners[0].links.id();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    {
        let mut state = lock(&running.platforms[1].0);
        state.playback_start_gate = Some(gate.clone());
        state.playback_entered = Some(entered.clone());
    }
    let at = Instant::now();
    supply_opus_at(&running, 0.2, at);
    running.owners[1].tick_exit_at(at);
    // Establish the first blocked open before counting attempts. Spawning its
    // OS thread does not mean it has entered Platform::playback yet.
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .expect("playback worker reaches the held native open");
    assert_eq!(running.owners[1].receivers[&peer].backlog.len(), 3);
    for packet in 3..64 {
        let at = at + Duration::from_millis((packet - 2) * 5);
        supply_opus_at(&running, 0.2, at);
        running.owners[1].tick_exit_at(at);
    }
    assert_eq!(running.owners[1].receivers[&peer].backlog.len(), 64);
    assert!(!running.owners[1].playback_failed);
    // The next packet, rather than elapsed time or pump throughput, causes the
    // capacity failure while the same native-open worker remains blocked.
    supply_opus_at(&running, 0.2, at + Duration::from_millis(310));
    running.owners[1].tick_exit_at(at + Duration::from_millis(310));
    assert!(running.owners[1].playback_failed);
    assert_eq!(
        running.owners[1].error.as_deref(),
        Some("audio wake backlog capacity exceeded")
    );
    let starts = lock(&running.platforms[1].0)
        .events
        .iter()
        .filter(|e| **e == "playback")
        .count();
    // Advance the retry deadline while the first native open remains blocked.
    // It must wait for that worker, even though its route was already retired.
    let due = running.owners[1].playback_retry_at.unwrap();
    for _ in 0..8 {
        running.owners[1].tick_exit_at(due + PLAYBACK_RETRY_MAX);
    }
    assert_eq!(
        lock(&running.platforms[1].0)
            .events
            .iter()
            .filter(|e| **e == "playback")
            .count(),
        starts
    );
    assert!(running.owners[1].starting_playback.is_some());
    *lock(&gate.0) = true;
    gate.1.notify_one();
    running
        .until(|r| !r.owners[1].playback_failed && r.prepared_route_ready())
        .await;
    assert!(running.owners[1].playback_retry_at.is_none());
    assert!(running.owners[1].problem().is_none());
    running.stop().await;
}

#[tokio::test]
async fn native_key_failure_publishes_actual_readback_before_returning_refusal() {
    let mut running = Running::start(false).await;
    let requester = boot(running.owners[0].links.boot());
    let context =
        oink_audio::session::volume_context(&requester, running.owners[1].exit_volume.state().unwrap()).unwrap();
    lock(&running.platforms[1].0).key_failed_after_write = true;
    let result = running.owners[1].exit_volume_key(oink_audio::volume::KeyEvent {
        source: requester,
        session: context,
        press_id: 1,
        action: oink_audio::volume::KeyAction::Up,
        pressed: true,
        repeat: false,
        modifiers: 0,
    });
    assert_eq!(result, Err(dto::ControlFailure::ApplyFailed));
    let fact = running.owners[1].group.local_facts().exit_volume().unwrap().unwrap();
    assert_eq!(fact.volume_percent().unwrap(), Some(60));
    assert_eq!(fact, running.owners[1].exit_volume.state().unwrap().clone());
    running.stop().await;
}

#[tokio::test]
async fn publication_failure_after_save_returns_actual_persisted_owner_result() {
    let mut running = Running::new().await;
    running.owners[0].group.stop().await;
    let owner = &mut running.owners[0];
    let requester = boot(owner.links.boot());
    let group = owner.group.applied().group;
    let option = dto::LocalOption::ForwardingEnabled;
    let revision = lock(&owner.store).owner().revision(option).unwrap();
    let applied = owner
        .set_option(&requester, group, option, revision, false.write())
        .unwrap();
    assert_eq!(applied.outcome, dto::ControlOutcome::Failed);
    assert_eq!(applied.reason, dto::ControlFailure::ApplyFailed);
    assert!(applied.persisted);
    assert_eq!(applied.current.value().unwrap(), false.write());
    assert_eq!(applied.fact_sequence, None);
    assert!(
        !AgentConfig::load_from(running.directories[0].as_path())
            .unwrap()
            .settings
            .audio
            .forwarding_enabled
    );
    running.stop().await;
}

#[tokio::test]
async fn removing_the_selected_exit_retires_media_without_fallback_or_player_pause() {
    let mut running = Running::start(true).await;
    let removed = running.owners[1].links.id();
    let selected = running.owners[0].resolution.stamp().unwrap();
    assert_eq!(selected.owner, removed);
    running.owners[0].group.remove_member(removed).unwrap();
    running
        .until(|r| {
            !r.owners[0].members().contains(&removed)
                && r.owners[0].resolution
                    == Resolution::Exit {
                        stamp: selected,
                        available: false,
                    }
                && r.owners[0].capture.is_none()
        })
        .await;
    assert!(running.owners[0].volume.load().is_none());
    assert!(running.owners[0].source_session.prepared().is_none());
    assert_eq!(pauses(&running.platforms[0]), 0);
    // A later explicit claim may take over; removal itself never elects a fallback.
    running.claim(0, true);
    let local = running.owners[0].links.id();
    running.until(|r| r.owners[0].resolution.owner() == Some(local)).await;
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

fn permission_card(group: &ConfigModule, source: DeviceId, nonce: [u8; 16]) -> crate::device_cards::SpeakerIcon {
    speaker_card(group, source, nonce, true).resource.icon
}

fn speaker_card(
    group: &ConfigModule,
    source: DeviceId,
    nonce: [u8; 16],
    exit_baseline: bool,
) -> crate::device_cards::SpeakerCard {
    use crate::device_cards::*;
    let facts = if group
        .local_facts()
        .boot()
        .unwrap()
        .device_id()
        .unwrap()
        .public_key()
        .unwrap()
        .0
        == source.0
    {
        group.local_facts()
    } else {
        group
            .members()
            .members
            .iter()
            .find(|m| m.device == source)
            .unwrap()
            .facts
            .clone()
            .unwrap()
    };
    let inputs = CardInputs {
        group: Some(group.applied().group.into()),
        members: vec![CardMember {
            device: source.into(),
            name: "synthetic".into(),
            os: "macos".into(),
            is_self: facts.boot().unwrap() == group.local_facts().boot().unwrap(),
            ready: true,
            offline_reason: OfflineKind::Disconnected,
            last_seen_ms: None,
            rtt_ms: None,
            live_boot: Some(nonce),
            facts: Some(facts),
            capabilities: vec!["oink.device-options/1".into(), "oink.device-resources/1".into()],
        }],
        focus: FocusObservation::default(),
        exit_baseline,
        microphone_resolution: Default::default(),
        local_exit_selection_ready: true,
        selected_exit: None,
        exit_claim: None,
        emergency_delivery: None,
    };
    Projector::new().unwrap().update(&inputs, 1).unwrap().cards[0]
        .status
        .as_ref()
        .unwrap()
        .speaker
        .clone()
}

#[tokio::test]
async fn fresh_member_cards_can_precede_audio_baseline_reconciliation() {
    use crate::device_cards::SpeakerMode;
    // Config fact replication runs, but neither audio owner is pumped. This
    // forces the ordering that a card-only waiter can observe after pairing.
    let mut r = Running::before_reconciliation(false).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let complete = r.owners.iter().all(|owner| {
                owner.group.members().members.iter().all(|member| {
                    let facts = if member.is_self {
                        Some(owner.group.local_facts())
                    } else {
                        member.facts.clone()
                    };
                    facts
                        .and_then(|facts| facts.preferences().ok().flatten())
                        .is_some_and(|items| {
                            items
                                .0
                                .iter()
                                .any(|item| item.option() == Ok(dto::LocalOption::AudioExitClaim))
                        })
                })
            });
            if complete {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("current owner preferences replicate without audio reconciliation");
    for owner in &r.owners {
        assert_eq!(owner.resolution, Resolution::Unknown);
        assert_eq!(lock(&owner.state).resolution, Resolution::Unknown);
        for peer in &r.owners {
            let speaker = speaker_card(&owner.group, peer.links.id(), peer.links.boot().nonce, false);
            assert_eq!(speaker.forwarding_enabled, Some(false));
            assert_eq!(speaker.resource.configured_mode, Some(SpeakerMode::Unused));
        }
    }
    // One reconciliation consumes the already authenticated null claims. No
    // capability/permission change, retry, or elapsed-time fallback is needed.
    for owner in &mut r.owners {
        owner.apply();
        owner.publish(owner.phase());
        let status = lock(&owner.state);
        assert!(matches!(status.resolution, Resolution::None { .. }), "{status:?}");
        assert!(!status.settings.forwarding_enabled);
        assert!(!status.source_muted);
        assert_eq!(status.sent, 0);
    }
    r.stop().await;
}

#[tokio::test]
async fn permission_changes_update_capture_description_peer_facts_and_cards() {
    use crate::device_cards::SpeakerIcon;
    let mut r = Running::new().await;
    for owner in &r.owners {
        owner.group.hold_periodic_facts();
    }
    let source = r.owners[0].links.id();
    let nonce = r.owners[0].links.boot().nonce;
    let queries = lock(&r.platforms[0].0).queries;
    for (preflight, permission, capture, fault, icon) in [
        (
            CapturePreflight::NotDetermined,
            CapturePermission::Unknown,
            "synthetic_permission_unknown",
            dto::ResourceFaultCode::None,
            SpeakerIcon::Forwarding,
        ),
        (
            CapturePreflight::Authorized,
            CapturePermission::Authorized,
            "synthetic",
            dto::ResourceFaultCode::None,
            SpeakerIcon::Forwarding,
        ),
        (
            CapturePreflight::Denied,
            CapturePermission::Denied,
            "synthetic_permission_denied",
            dto::ResourceFaultCode::PermissionDenied,
            SpeakerIcon::Fault,
        ),
        (
            CapturePreflight::Authorized,
            CapturePermission::Authorized,
            "synthetic",
            dto::ResourceFaultCode::None,
            SpeakerIcon::Forwarding,
        ),
        (
            CapturePreflight::Unavailable("missing SPI".into()),
            CapturePermission::Unknown,
            "synthetic_permission_unknown",
            dto::ResourceFaultCode::None,
            SpeakerIcon::Forwarding,
        ),
    ] {
        lock(&r.platforms[0].0).permission = Some(preflight.clone());
        let owner = &mut r.owners[0];
        owner.accept_permission(preflight);
        owner.publish_observations().unwrap();
        owner.publish(owner.phase());
        let facts = owner.group.local_facts();
        assert_eq!(
            facts.capabilities().unwrap().capture_permission().unwrap(),
            permission.to_wire()
        );
        assert_eq!(
            facts
                .resources()
                .unwrap()
                .unwrap()
                .faults()
                .unwrap()
                .unwrap()
                .speaker()
                .unwrap()
                .expect("an observed healthy speaker needs an explicit None fault")
                .code()
                .unwrap(),
            fault
        );
        assert_eq!(
            lock(&owner.state)
                .devices
                .iter()
                .find(|d| d.device == source)
                .unwrap()
                .capture,
            capture
        );
        assert_eq!(permission_card(&owner.group, source, nonce), icon);
        r.until(|r| {
            r.owners[1]
                .group
                .members()
                .members
                .iter()
                .find(|m| m.device == source)
                .and_then(|m| m.facts.as_ref())
                .is_some_and(|f| {
                    f.capabilities().unwrap().capture_permission().unwrap() == permission.to_wire()
                        && f.resources()
                            .unwrap()
                            .unwrap()
                            .faults()
                            .unwrap()
                            .unwrap()
                            .speaker()
                            .unwrap()
                            .is_some_and(|s| s.code().unwrap() == fault)
                })
        })
        .await;
        assert_eq!(permission_card(&r.owners[1].group, source, nonce), icon);
        assert_eq!(
            lock(&r.platforms[0].0).queries,
            queries,
            "permission changes must not enumerate outputs"
        );
        assert!(r.owners[0].capture.is_none());
    }
    r.stop().await;
}

#[tokio::test]
async fn permission_poll_continues_after_authorization_to_observe_revocation() {
    let mut r = Running::new().await;
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::Authorized);
    r.owners[0].accept_permission(CapturePreflight::Authorized);
    for (preflight, expected) in [
        (CapturePreflight::Denied, CapturePermission::Denied),
        (CapturePreflight::Authorized, CapturePermission::Authorized),
        (CapturePreflight::NotDetermined, CapturePermission::Unknown),
        (CapturePreflight::Authorized, CapturePermission::Authorized),
    ] {
        lock(&r.platforms[0].0).permission = Some(preflight);
        r.owners[0].permission_checked = Instant::now() - Duration::from_secs(3);
        r.until(|r| r.owners[0].permission == expected).await;
    }
    r.stop().await;
}

#[tokio::test]
async fn permission_snapshot_is_not_overwritten_by_an_older_output_query() {
    let mut r = Running::new().await;
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::Denied);
    let owner = &mut r.owners[0];
    owner.discovery.request();
    let old = owner.discovery.wait().unwrap();
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::Authorized);
    owner.accept_permission(CapturePreflight::Authorized);
    owner.accept_devices(old);
    assert_eq!(owner.permission, CapturePermission::Authorized);
    r.stop().await;
}

#[tokio::test]
async fn permission_revocation_retires_active_capture_without_changing_playback_permission() {
    let mut r = Running::start(false).await;
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::Authorized);
    r.owners[0].accept_permission(CapturePreflight::Authorized);
    assert!(r.owners[0].capture.is_some());
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::Denied);
    r.owners[0].accept_permission(CapturePreflight::Denied);
    assert!(
        r.owners[0].capture.is_none(),
        "a revoked capture must release its source mute"
    );
    assert_eq!(
        r.owners[0].problem().unwrap().code,
        dto::UserFacingCode::PermissionDenied
    );
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::NotDetermined);
    r.owners[0].accept_permission(CapturePreflight::NotDetermined);
    assert_eq!(
        r.owners[0].problem().unwrap().code,
        dto::UserFacingCode::PermissionDenied,
        "an indeterminate query must not turn a prior source denial into a hardware fault"
    );
    let receiver_problem = r.owners[1].problem();
    let receiving = r.owners[1].playback.is_some();
    lock(&r.platforms[1].0).permission = Some(CapturePreflight::Denied);
    r.owners[1].accept_permission(CapturePreflight::Denied);
    assert_eq!(
        r.owners[1].problem(),
        receiver_problem,
        "capture denial must not add a fault to this device's receiver role"
    );
    assert_eq!(r.owners[1].playback.is_some(), receiving);
    lock(&r.platforms[0].0).permission = Some(CapturePreflight::Authorized);
    r.owners[0].accept_permission(CapturePreflight::Authorized);
    r.until(|r| r.owners[0].capture.is_some()).await;
    r.stop().await;
}

#[tokio::test]
async fn driver_operation_retires_mute_before_installer_and_holds_until_worker_exits() {
    let mut running = Running::start(false).await;
    let before = running.owners[0].settings.clone();
    let active = Arc::new(AtomicBool::new(true));
    running.owners[0].prepare_driver_change(active.clone()).unwrap();
    assert!(running.owners[0].capture.is_none());
    assert!(
        lock(&running.platforms[0].0).events.contains(&"restore"),
        "installer may run only after capture/mute restoration"
    );
    assert!(running.owners[0].receivers.is_empty());
    for _ in 0..3 {
        running.owners[0].tick();
    }
    assert!(
        running.owners[0].capture.is_none(),
        "UAC wait must not reacquire a lease"
    );
    // Success, cancellation and failed installation all release the worker's
    // guard and reconcile the same route without changing user preferences.
    active.store(false, Ordering::Release);
    running.until(Running::prepared_route_ready).await;
    assert_eq!(running.owners[0].settings, before);
    assert!(running.owners[0].mute_cleanup_error.is_none());
    running.stop().await;
}

#[tokio::test]
async fn driver_operation_refuses_unconfirmed_mute_cleanup_and_cancelled_admission() {
    let mut running = Running::start(false).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    assert!(running.owners[0].prepare_driver_change(cancelled).is_err());
    assert!(running.owners[0].capture.is_some());
    lock(&running.platforms[0].0).cleanup_error = Some("mute restore failed".into());
    let active = Arc::new(AtomicBool::new(true));
    assert!(running.owners[0].prepare_driver_change(active.clone()).is_err());
    assert!(running.owners[0].capture.is_none());
    assert!(
        running.owners[0].mute_cleanup_error.is_some(),
        "a real restore failure must still block the installer"
    );
    active.store(false, Ordering::Release);
    running.stop().await;
}

#[tokio::test]
async fn manual_update_interrupts_open_silent_audio_and_abort_restores_preferences() {
    let mut running = Running::start(false).await;
    assert!(running.owners[0].capture.is_some());
    assert_eq!(running.owners[0].sent.load(Ordering::Relaxed), 0);
    let preferences: Vec<_> = running.owners.iter().map(|o| o.settings.clone()).collect();
    let resolution = running.owners[0].resolution;
    for owner in &mut running.owners {
        owner.suspend_for_update().unwrap();
        assert!(owner.capture.is_none() && owner.receivers.is_empty());
        assert!(owner.playback.is_none() && owner.starting_playback.is_none());
        assert!(owner.update_activity.is_none());
    }
    assert!(lock(&running.platforms[0].0).events.contains(&"restore"));
    // Opening admission after an aborted replacement reconciles the same route;
    // there is no persisted forwarding toggle or lost exit claim to restore.
    running.until(Running::prepared_route_ready).await;
    for (owner, original) in running.owners.iter().zip(preferences) {
        assert_eq!(owner.settings, original);
    }
    assert_eq!(running.owners[0].resolution, resolution);
    running.stop().await;
}

#[tokio::test]
async fn manual_update_reports_native_audio_restore_failure_instead_of_waiting() {
    let mut running = Running::start(false).await;
    lock(&running.platforms[0].0).cleanup_error = Some("restore failed".into());
    assert!(running.owners[0].suspend_for_update().is_err());
    assert!(running.owners[0].capture.is_none());
    running.stop().await;
}

#[tokio::test]
async fn manual_update_ignores_historical_cleanup_fault_but_checks_current_capture() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[0];
    owner.mute_cleanup_error = Some("historical restore error".into());
    lock(&running.platforms[0].0).cleanup_failed = true;
    assert!(owner.capture.is_some());
    assert!(owner.suspend_for_update().is_ok(), "current capture cleanup succeeded");
    // Keep the diagnostic/capture safety latch; only Install must ignore history.
    assert!(owner.mute_cleanup_error.is_some());
    assert!(owner.platform.capture_cleanup_failed());
    running.stop().await;
}

#[tokio::test]
async fn manual_update_remembers_current_cleanup_failure_until_admission_reopens() {
    let mut running = Running::start(false).await;
    lock(&running.platforms[0].0).cleanup_error = Some("current restore error".into());
    let owner = &mut running.owners[0];
    assert!(owner.suspend_for_update().is_err());
    assert!(owner.capture.is_none());
    assert!(
        owner.suspend_for_update().is_err(),
        "a second request cannot erase this attempt's error"
    );
    running.stop().await;
}

#[tokio::test]
async fn forwarding_drag_applies_and_publishes_each_revision_before_durability() {
    let mut running = Running::start(true).await;
    let owner = &mut running.owners[0];
    let option = dto::LocalOption::ForwardingControls;
    let mut revision = lock(&owner.store).owner().revision(option).unwrap();
    // A released peer's request has no opt-in and retains synchronous semantics.
    let legacy = owner
        .set_option(
            &boot(owner.links.boot()),
            owner.group.applied().group,
            option,
            revision,
            RawValue::Map([(1, 20u16.write()), (2, false.write())].into_iter().collect()),
        )
        .unwrap();
    assert!(legacy.persisted);
    revision += 1;
    assert_eq!(
        AgentConfig::load_from(running.directories[0].as_path())
            .unwrap()
            .settings
            .audio
            .forwarding_volume_percent,
        20
    );
    let durable_before_drag = std::fs::read(lock(&owner.store).settings_path()).unwrap();
    let mut sequence = owner.group.local_facts().fact_sequence().unwrap();
    for percent in [25u16, 40, 65, 80] {
        let patch = RawValue::Map([(1, percent.write()), (2, false.write())].into_iter().collect());
        let applied = owner
            .set_option_with_persistence(
                &boot(owner.links.boot()),
                owner.group.applied().group,
                option,
                revision,
                patch,
                true,
            )
            .unwrap();
        assert_eq!(applied.outcome, dto::ControlOutcome::Applied);
        assert!(!applied.persisted);
        revision += 1;
        assert_eq!(applied.current.revision().unwrap(), revision);
        assert_eq!(owner.settings.forwarding_volume_percent, percent);
        assert_eq!(lock(&owner.store).settings.audio.forwarding_volume_percent, percent);
        let next = applied.fact_sequence.unwrap();
        assert!(next > sequence);
        sequence = next;
        let preferences = owner.group.local_facts().preferences().unwrap().unwrap();
        assert!(preferences.0.contains(&applied.current));
        let durable = AgentConfig::load_from(running.directories[0].as_path()).unwrap();
        assert_ne!(durable.owner().revision(option), Some(revision));
        assert_eq!(
            std::fs::read(lock(&owner.store).settings_path()).unwrap(),
            durable_before_drag
        );
    }
    // Release sends the same final value without opt-in: no revision change,
    // but it must flush the pending snapshot before reporting durable success.
    let final_reply = owner
        .set_option(
            &boot(owner.links.boot()),
            owner.group.applied().group,
            option,
            revision,
            RawValue::Map([(1, 80u16.write()), (2, false.write())].into_iter().collect()),
        )
        .unwrap();
    assert_eq!(final_reply.outcome, dto::ControlOutcome::NoChange);
    assert!(final_reply.persisted);
    assert!(lock(&owner.store).forwarding_persisted());
    let durable = AgentConfig::load_from(running.directories[0].as_path()).unwrap();
    assert_eq!(durable.settings.audio.forwarding_volume_percent, 80);
    assert_eq!(durable.owner().revision(option), Some(revision));
    running.stop().await;
}

#[tokio::test]
async fn default_output_callbacks_publish_coalesced_facts_without_being_an_exit() {
    let mut running = Running::new().await;
    assert!(running.owners[0].exit_volume.state().is_none());
    let now = Instant::now() + Duration::from_secs(1);
    {
        let mut state = lock(&running.platforms[0].0);
        state.volume = Some(oink_audio::volume::Reading {
            percent: 9,
            muted: false,
        });
        state.volume_revision += 1;
    }
    running.owners[0].local_output.tick(now);
    running.owners[0].publish_observations().unwrap();
    let initial = running.owners[0].published_observations.clone();
    for (ms, percent) in [(1, 12), (50, 35), (150, 87)] {
        let wake = {
            let mut state = lock(&running.platforms[0].0);
            state.volume = Some(oink_audio::volume::Reading { percent, muted: true });
            state.volume_revision += 1;
            state.volume_wake.clone().unwrap()
        };
        wake();
        running.owners[0].local_output.tick(now + Duration::from_millis(ms));
        running.owners[0].publish_observations().unwrap();
        // Deliver an unrelated native observation inside the coalescing window.
        // Device Facts share a sequence with link/RTT and display observations.
        running.owners[0]
            .group
            .publish_native_arrangement(format!("synthetic-arrangement-{ms}"))
            .unwrap();
        assert_eq!(running.owners[0].published_observations, initial);
    }
    running.owners[0].local_output.tick(now + Duration::from_millis(200));
    running.owners[0].publish_observations().unwrap();
    let facts = running.owners[0].group.local_facts();
    assert_ne!(running.owners[0].published_observations, initial);
    let volume = facts
        .resources()
        .unwrap()
        .unwrap()
        .speaker()
        .unwrap()
        .unwrap()
        .local_output()
        .unwrap()
        .unwrap();
    assert_eq!(volume.volume_percent().unwrap(), 87);
    assert!(volume.muted().unwrap());
    assert_eq!(lock(&running.platforms[0].0).volume_writes, 0);
    running
        .until(|running| {
            running.owners[1]
                .group
                .members()
                .members
                .iter()
                .find(|member| member.device == running.owners[0].links.id())
                .and_then(|member| member.facts.as_ref())
                .and_then(|facts| facts.resources().ok().flatten())
                .and_then(|resources| resources.speaker().ok().flatten())
                .and_then(|speaker| speaker.local_output().ok().flatten())
                .is_some_and(|volume| volume.volume_percent() == Ok(87) && volume.muted() == Ok(true))
        })
        .await;
    running.stop().await;
}

#[tokio::test]
async fn unchanged_audio_maintenance_does_not_replace_realtime_routes() {
    let mut r = Running::start(false).await;
    let streams = &mut r.owners[0].streams;
    let forward = streams.forward_table();
    let microphone = streams.microphone_table();
    streams.set_forwarding(true, 1.0);
    let initial = forward.load();
    let initial_microphone = microphone.load();
    assert!(!initial.to.is_empty());
    for _ in 0..100 {
        streams.poll(Instant::now());
        streams.set_forwarding(true, 1.0);
        assert!(
            Arc::ptr_eq(&initial, &forward.load()),
            "unchanged maintenance republished the RT table"
        );
        assert!(Arc::ptr_eq(&initial_microphone, &microphone.load()));
    }
    streams.set_forwarding(true, 0.5);
    assert_eq!(forward.load().to[0].gain, 0.5, "gain changes publish synchronously");
    streams.set_forwarding(false, 0.5);
    assert!(
        forward.load().to.is_empty(),
        "stopping forwarding withdraws authority synchronously"
    );
    r.stop().await;
}

#[tokio::test]
async fn silent_audio_control_sleeps_but_onset_is_sent_without_a_control_tick() {
    let mut r = Running::start(false).await;
    r.supply(0, 0.0);
    let now = Instant::now();
    lock(&r.platforms[0].0)
        .encoder
        .as_mut()
        .unwrap()
        .tick(now + oink_audio::source::SILENCE_TIMEOUT);
    assert!(r.owners[0].encoded.as_ref().unwrap().sleeping());
    r.owners[0].report_at = now;
    assert_eq!(r.owners[0].maintenance_deadline(now), now + Duration::from_secs(1));
    let before = r.owners[0].sent.load(Ordering::Relaxed);
    for _ in 0..4 {
        r.supply(0, 0.2);
    }
    assert!(
        r.owners[0].sent.load(Ordering::Relaxed) > before,
        "onset goes straight from the encoder to the admitted media sender"
    );
    assert!(!r.owners[0].encoded.as_ref().unwrap().sleeping());
    assert_eq!(r.owners[0].maintenance_deadline(now), now + Duration::from_millis(20));
    r.stop().await;
}

#[tokio::test]
async fn microphone_receive_without_source_is_a_durable_owner_preference() {
    let mut running = Running::new().await;
    let owner = &mut running.owners[0];
    let option = dto::LocalOption::MicrophoneReceiveMuted;
    for muted in [false, true] {
        let revision = lock(&owner.store).owner().revision(option).unwrap();
        let applied = owner
            .set_option(
                &boot(owner.links.boot()),
                owner.group.applied().group,
                option,
                revision,
                muted.write(),
            )
            .unwrap();
        assert!(matches!(
            applied.outcome,
            dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange
        ));
        assert!(applied.persisted);
        assert!(owner.microphone.resolution.owner().is_none());
        assert_eq!(
            lock(&owner.store).owner().item(option).unwrap().value().unwrap(),
            muted.write()
        );
    }
    running.stop().await;
}

#[tokio::test]
async fn service_reset_recovers_after_delayed_notification_and_empty_device_list() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    owner.service_generation = Some(1);
    lock(&running.platforms[1].0).service_generation = Some(1);
    owner.accept_devices(Ok(DeviceSnapshot {
        service_generation: Some(1),
        outputs: vec![],
        latencies: vec![],
        capture: "synthetic".into(),
    }));
    owner.refresh_exit_binding(false).unwrap();
    // HAL enumeration can block longer than the former one-second window.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    lock(&running.platforms[1].0).service_generation = Some(2);
    owner.observe_service_restart();
    assert!(
        owner.restart_playback,
        "confirmed reset must rearm invalidated playback"
    );
    owner.accept_devices(Ok(DeviceSnapshot {
        service_generation: Some(2),
        outputs: vec![OutputDevice {
            id: "synthetic-output".into(),
            name: "Synthetic".into(),
            is_default: true,
        }],
        latencies: vec![],
        capture: "synthetic".into(),
    }));
    owner.refresh_exit_binding(false).unwrap();
    assert!(!owner.restart_playback);
    assert!(!owner.playback_failed);
    running.until(Running::prepared_route_ready).await;
    running.stop().await;
}

#[tokio::test]
async fn remote_exit_fault_keeps_source_speaker_card_healthy() {
    let mut running = Running::start(true).await;
    let owner = &mut running.owners[0];
    let session = owner.source_session.prepared().unwrap().session();
    let fault = dto::AudioRouteFault::from_raw(
        [
            (1, session.write()),
            (
                3,
                Problem::new(dto::UserFacingCode::DeviceUnavailable).to_wire().write(),
            ),
        ]
        .into(),
    )
    .unwrap();
    let peer = owner.resolution.owner().unwrap();
    owner.message(peer, Message::AudioRouteFault(fault));
    owner.publish_observations().unwrap();
    assert_eq!(
        permission_card(&owner.group, owner.links.id(), owner.links.boot().nonce),
        crate::device_cards::SpeakerIcon::Forwarding
    );
    assert_eq!(owner.phase(), AudioPhase::WaitingForExit);
    assert!(owner.capture.is_none());
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn service_reset_requires_fresh_metadata_and_same_device_notifications_do_not_bypass_backoff() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    owner.service_generation = Some(1);
    lock(&running.platforms[1].0).service_generation = Some(1);
    owner.fail_playback("HAL output callback stalled".into());
    let snapshot = |generation| {
        Ok(DeviceSnapshot {
            service_generation: Some(generation),
            outputs: vec![OutputDevice {
                id: "synthetic-output".into(),
                name: "Synthetic".into(),
                is_default: true,
            }],
            latencies: vec![],
            capture: "synthetic".into(),
        })
    };
    owner.accept_devices(snapshot(1));
    owner.tick_exit_at(owner.playback_retry_at.unwrap() - Duration::from_nanos(1));
    assert!(
        owner.playback_failed,
        "same-endpoint notifications must respect retry backoff"
    );
    lock(&running.platforms[1].0).service_generation = Some(2);
    owner.observe_service_restart();
    owner.accept_devices(snapshot(1));
    assert!(
        owner.awaiting_output_snapshot,
        "old service snapshots cannot authorize IO"
    );
    owner.accept_devices(snapshot(2));
    owner.refresh_exit_binding(false).unwrap();
    assert!(!owner.playback_failed);
    running.until(Running::prepared_route_ready).await;
    running.stop().await;
}

#[tokio::test]
async fn failed_playback_follows_a_new_default_output_without_a_new_exit_claim() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    let claim = owner.resolution.stamp();
    owner.fail_playback("temporary endpoint initialization failure".into());
    owner.accept_devices(Ok(DeviceSnapshot {
        service_generation: None,
        outputs: vec![OutputDevice {
            id: "new-default".into(),
            name: "New output".into(),
            is_default: true,
        }],
        latencies: vec![],
        capture: "synthetic".into(),
    }));
    owner.refresh_exit_binding(false).unwrap();
    owner.tick_exit();
    assert!(
        !owner.playback_failed,
        "a different default output must not inherit the failed endpoint's latch"
    );
    assert!(owner.playback.is_some());
    assert_eq!(owner.bound_output.as_deref(), Some("new-default"));
    assert_eq!(owner.resolution.stamp(), claim);
    running.stop().await;
}

#[tokio::test]
async fn failed_playback_retries_the_same_endpoint_without_another_notification() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    owner.playback = None;
    owner.playback_idle = false;
    lock(&running.platforms[1].0).playback_init_error = Some("Initialize legacy shared fallback: 0x8889000A".into());
    owner.tick_exit();
    assert!(owner.playback_failed);
    lock(&running.platforms[1].0).playback_init_error = None;
    owner.tick_exit_at(Instant::now() + Duration::from_secs(10));
    assert!(
        !owner.playback_failed,
        "transient open failure must recover without a new endpoint event"
    );
    assert!(owner.playback.is_some());
    running.stop().await;
}

#[tokio::test]
async fn playback_retry_is_bounded_and_keeps_claim_but_replaces_output_generation() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    let claim = owner.resolution.stamp();
    let binding = owner.output_binding();
    let queries = lock(&running.platforms[1].0).queries;
    lock(&running.platforms[1].0).playback_init_error = Some("endpoint temporarily unavailable".into());
    owner.fail_playback("device invalidated".into());
    for _ in 0..8 {
        let due = owner.playback_retry_at.unwrap();
        let opens = lock(&running.platforms[1].0)
            .events
            .iter()
            .filter(|event| **event == "playback")
            .count();
        owner.tick_exit_at(due - Duration::from_nanos(1));
        assert_eq!(
            lock(&running.platforms[1].0)
                .events
                .iter()
                .filter(|event| **event == "playback")
                .count(),
            opens
        );
        owner.tick_exit_at(due);
        assert!(owner.playback_failed);
        assert!(owner.playback_retry_at.is_some());
        assert!(owner.playback_retry_delay <= PLAYBACK_RETRY_MAX);
    }
    assert_eq!(owner.playback_retry_delay, PLAYBACK_RETRY_MAX);
    lock(&running.platforms[1].0).playback_init_error = None;
    owner.tick_exit_at(owner.playback_retry_at.unwrap());
    assert!(!owner.playback_failed);
    assert!(owner.playback.is_some());
    assert!(owner.playback_retry_at.is_none());
    assert_eq!(owner.playback_retry_delay, PLAYBACK_RETRY_INITIAL);
    assert_eq!(owner.resolution.stamp(), claim);
    assert_ne!(
        owner.output_binding(),
        binding,
        "retired handles must not reuse their volume/media generation"
    );
    assert_eq!(
        lock(&running.platforms[1].0).queries,
        queries,
        "retry must not poll the device list"
    );
    running.until(Running::prepared_route_ready).await;
    running.stop().await;
}

#[tokio::test]
async fn playback_retry_waits_for_resume_and_never_bypasses_cleanup_or_fresh_metadata() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    owner.fail_playback("temporary output failure".into());
    let due = owner.playback_retry_at.unwrap();
    for gate in 0..6 {
        owner.update_suspended = gate == 5;
        owner.paused = gate == 0;
        owner.power_state.sleeping = gate == 1;
        owner.awaiting_output_snapshot = gate == 2;
        owner.mute_cleanup_error = (gate == 3).then(|| "restore pending".into());
        lock(&running.platforms[1].0).cleanup_failed = gate == 4;
        owner.tick_exit_at(due);
        assert!(owner.playback_failed);
        assert!(owner.playback.is_none());
        assert!(!owner.playback_retry_eligible());
    }
    lock(&running.platforms[1].0).cleanup_failed = false;
    owner.update_suspended = false;
    owner.tick_exit_at(due);
    assert!(!owner.playback_failed);
    assert!(owner.playback.is_some());
    running.stop().await;
}

#[tokio::test]
async fn playback_retry_recovers_a_returning_selected_endpoint_without_falling_back() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[1];
    owner.settings.receive_output = Some("synthetic-output".into());
    let snapshot = |include_selected: bool| {
        Ok(DeviceSnapshot {
            service_generation: None,
            outputs: [
                Some(OutputDevice {
                    id: "other-default".into(),
                    name: "Other".into(),
                    is_default: true,
                }),
                include_selected.then(|| OutputDevice {
                    id: "synthetic-output".into(),
                    name: "Selected".into(),
                    is_default: false,
                }),
            ]
            .into_iter()
            .flatten()
            .collect(),
            latencies: vec![],
            capture: "synthetic".into(),
        })
    };
    owner.accept_devices(snapshot(false));
    owner.refresh_exit_binding(false).unwrap();
    owner.tick_exit_at(Instant::now() + Duration::from_secs(20));
    assert!(owner.bound_output.is_none());
    assert!(owner.playback.is_none());
    assert!(!owner.playback_retry_eligible());
    owner.accept_devices(snapshot(true));
    owner.refresh_exit_binding(false).unwrap();
    owner.tick_exit();
    assert_eq!(owner.bound_output.as_deref(), Some("synthetic-output"));
    assert!(!owner.playback_failed);
    assert!(owner.playback.is_some());
    running.stop().await;
}

#[tokio::test]
async fn cached_volume_context_tracks_consent_and_local_and_remote_output_generations() {
    let mut r = Running::start(false).await;
    r.until(|r| r.owners.iter().all(|owner| owner.volume.load().is_some()))
        .await;
    let original = r.owners[0].volume.load().as_ref().as_ref().unwrap().clone();
    r.owners[0].refresh_volume_context();
    assert!(Arc::ptr_eq(
        &original,
        r.owners[0].volume.load().as_ref().as_ref().unwrap()
    ));

    r.owners[0].settings.forwarding_enabled = false;
    r.owners[0].refresh_volume_context();
    assert!(r.owners[0].volume.load().is_none());
    r.owners[0].settings.forwarding_enabled = true;
    r.owners[0].refresh_volume_context();
    assert_eq!(
        r.owners[0].volume.load().as_ref().as_ref().unwrap().receiver_output(),
        original.receiver_output()
    );

    let old_generation = original.receiver_output().unwrap().generation().unwrap();
    r.owners[1].refresh_exit_binding(true).unwrap();
    let generation = r.owners[1]
        .volume
        .load()
        .as_ref()
        .as_ref()
        .unwrap()
        .receiver_output()
        .unwrap()
        .generation()
        .unwrap();
    assert!(
        generation > old_generation,
        "a local rebind invalidates the cached context immediately"
    );
    r.owners[1].publish_observations().unwrap();
    r.until(|r| {
        r.owners[0]
            .volume
            .load()
            .as_ref()
            .as_ref()
            .is_some_and(|context| context.receiver_output().unwrap().generation() == Ok(generation))
    })
    .await;

    // Freeze facts publication so the heap assertion measures unchanged inputs,
    // not an unrelated network fact arriving during the audit.
    r.owners[0].group.stop().await;
    r.owners[0].refresh_volume_context();
    crate::allocation_audit::assert_no_heap(|| {
        for _ in 0..100 {
            r.owners[0].refresh_volume_context();
        }
    });
    r.stop().await;
}

#[tokio::test]
async fn cached_peer_outputs_follow_facts_and_disappear_when_the_member_is_removed() {
    let mut r = Running::start(false).await;
    let peer = r.owners[1].links.id();
    r.until(|r| lock(&r.owners[0].state).devices.iter().any(|d| d.device == peer))
        .await;
    r.owners[1].accept_devices(Ok(DeviceSnapshot {
        service_generation: None,
        outputs: vec![OutputDevice {
            id: "synthetic-output".into(),
            name: "Renamed output".into(),
            is_default: true,
        }],
        latencies: vec![],
        capture: "synthetic".into(),
    }));
    r.owners[1].publish_observations().unwrap();
    r.until(|r| {
        lock(&r.owners[0].state)
            .devices
            .iter()
            .any(|d| d.device == peer && d.outputs.first().is_some_and(|output| output.name == "Renamed output"))
    })
    .await;
    lock(&r.platforms[1].0).permission = Some(CapturePreflight::Denied);
    r.owners[1].accept_permission(CapturePreflight::Denied);
    r.owners[1].publish_observations().unwrap();
    r.until(|r| {
        lock(&r.owners[0].state)
            .devices
            .iter()
            .any(|d| d.device == peer && d.capture == "Denied")
    })
    .await;
    r.owners[0].group.remove_member(peer).unwrap();
    r.until(|r| !lock(&r.owners[0].state).devices.iter().any(|d| d.device == peer))
        .await;
    assert!(r.owners[0].peer_outputs.is_empty());
    assert!(r.owners[0].volume.load().is_none());
    r.stop().await;
}
