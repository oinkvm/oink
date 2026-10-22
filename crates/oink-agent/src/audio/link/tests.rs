use super::*;
use crate::streams::lifecycle_tests::Fixture;
use oink_protocol::v1::schema::{FixedBytes, WireValue};

#[derive(Default)]
struct State {
    encoder: Option<oink_audio::source::Encoder>,
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
    cleanup_error: Option<String>,
    follows_default: bool,
    default_output: Option<String>,
    cleanup_failed: bool,
    playback_error: Option<String>,
    playback_start_gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
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
    fn finish(&mut self) -> Option<String> {
        lock(&self.0).cleanup_error.clone()
    }
    fn default_output_changed(&self) -> bool {
        lock(&self.0).default_output_changed
    }
    fn restart_reason(&self) -> Option<String> {
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
            state.playback_start_gate.clone()
        };
        if let Some(gate) = gate {
            let (mutex, ready) = &*gate;
            let mut opened = lock(mutex);
            while !*opened {
                opened = ready.wait(opened).unwrap();
            }
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
                        settings.audio.forwarding_enabled = true;
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
        let mut this = Self {
            fixture,
            owners,
            platforms,
            directories,
            held_prepared_acks: None,
        };
        this.until(|r| r.owners.iter().all(|o| matches!(o.resolution, Resolution::None { .. })))
            .await;
        this
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
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                self.pump().await;
                if test(self) {
                    break;
                }
            }
        })
        .await
        .expect("synthetic audio owners converge");
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
async fn live_volume_works_without_capture_and_publishes_fact_before_success() {
    let mut running = Running::new().await;
    for owner in &mut running.owners {
        let mut settings = owner.settings.clone();
        settings.forwarding_enabled = false;
        owner.install_settings(settings).unwrap();
    }
    running.claim(1, true);
    running.until(|r| r.owners[0].volume.load().is_some()).await;
    assert!(running.owners.iter().all(|o| o.capture.is_none()));
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
    assert!(running.owners[1].volume.load().is_some());
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
    lock(&running.platforms[0].0).local_controls_changed = true;
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
    running.owners[0].source_failed = false;
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
        exit_baseline: true,
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
        .resource
        .icon
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
    let initial = running.owners[0].group.local_facts().fact_sequence().unwrap();
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
        assert_eq!(running.owners[0].group.local_facts().fact_sequence().unwrap(), initial);
    }
    running.owners[0].local_output.tick(now + Duration::from_millis(200));
    running.owners[0].publish_observations().unwrap();
    let facts = running.owners[0].group.local_facts();
    assert_eq!(facts.fact_sequence().unwrap(), initial + 1);
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
