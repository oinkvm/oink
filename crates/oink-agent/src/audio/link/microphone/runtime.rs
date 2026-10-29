//! Native objects stay on the audio control worker; realtime callbacks reuse the
//! speaker encoder, media channel, decoder, jitter buffer and drift correction.
use super::*;
use oink_audio::{media::Kind, microphone as mic};
use oink_protocol::v1::schema::{FixedBytes, Text};

// Local logs retain the native endpoint ID. The diagnostic recorder keeps only
// this exact-in-f64 48-bit pseudonym, never the endpoint name or native UID.
fn input_key(id: &str) -> u64 {
    let hash = blake3::hash(id.as_bytes());
    let mut bytes = [0; 8];
    bytes[..6].copy_from_slice(&hash.as_bytes()[..6]);
    u64::from_le_bytes(bytes)
}
struct Receiving {
    peer: DeviceId,
    session: dto::MicrophoneSession,
    prepared: Option<dto::MicrophonePrepared>,
    renewed: Instant,
    ready: bool,
    playback: Option<Box<dyn PlaybackStream>>,
    receiver: Option<Receiver>,
}
#[derive(Default)]
pub(super) struct Runtime {
    output_quality: Arc<oink_audio::sink::DropoutCounters>,
    source: mic::session::Source,
    capture: Option<mic::Capture>,
    encoded: Option<oink_audio::source::Encoded>,
    receiving: Option<Receiving>,
    monitor: Option<mic::DemandMonitor>,
    demand: mic::Demand,
    sampled: Option<Instant>,
    monitor_revision: u64,
    revision: Option<u64>,
    service: Option<u64>,
    discovered: bool,
    inputs: Vec<mic::InputDevice>,
    candidates: Vec<mic::InputCandidate>,
    default_inputs: mic::DefaultInputHistory,
    output: Option<String>,
    selected: Option<String>,
    selected_claim: Option<Stamp>,
    route_claim: Option<Stamp>,
    settings: Option<mic::Settings>,
    fault: Option<dto::ResourceFaultCode>,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    capture_watchdog: CaptureWatchdog,
    published: Option<(dto::MicrophoneObservation, Option<dto::AudioExitClaim>)>,
}
impl Runtime {
    pub(super) fn selectable_inputs(&self) -> Vec<mic::InputDevice> {
        self.candidates
            .iter()
            .filter(|c| !c.forwarding_loopback && !c.input.is_forwarding_loopback())
            .map(|c| c.input.clone())
            .collect()
    }
    fn selected_input_fault(&self) -> Option<dto::ResourceFaultCode> {
        let selected = self.selected.as_ref()?;
        if selected.ends_with(mic::MAC_INPUT_UID)
            || self
                .inputs
                .iter()
                .any(|i| &i.id == selected && i.is_forwarding_loopback())
            || self
                .candidates
                .iter()
                .any(|c| &c.input.id == selected && c.forwarding_loopback)
        {
            Some(dto::ResourceFaultCode::MicrophoneLoopback)
        } else {
            None
        }
    }
    fn select_input(
        &mut self,
        explicit: Option<&str>,
        permitted: bool,
        now: Instant,
        mut switch: impl FnMut(&str, &str) -> Result<(), String>,
    ) {
        self.selected = None;
        self.fault = None;
        match self.default_inputs.select(explicit, &self.candidates) {
            Ok(selection) => {
                let switched = if let Some(from) = selection.switch_from.as_deref() {
                    // User policy: a system-default forwarding loopback is
                    // repaired by changing only the system capture default.
                    // Explicit input selections never enter this path; do
                    // not open a different mic if the native write fails.
                    let result = if permitted {
                        switch(from, &selection.input)
                    } else {
                        Err("microphone capture permission is required".into())
                    };
                    match result {
                        Ok(()) => {
                            tracing::info!(target: "oink_microphone_routing", from_device_class="ours_virtual", to_device=%selection.input, to_device_key=input_key(&selection.input), reason=selection.reason, "system default microphone loopback replaced");
                            if let Some(input) = self.inputs.iter().find(|i| i.id == selection.input).cloned() {
                                self.default_inputs.observe(Some(input), &self.candidates);
                            }
                            true
                        }
                        Err(error) => {
                            tracing::warn!(target: "oink_microphone_routing", from_device_class="ours_virtual", to_device=%selection.input, to_device_key=input_key(&selection.input), reason="switch_failed", %error, "system default microphone loopback replacement failed");
                            self.record_fault_code(error, dto::ResourceFaultCode::MicrophoneLoopback, now);
                            false
                        }
                    }
                } else {
                    true
                };
                if switched {
                    self.selected = Some(selection.input);
                    self.healthy();
                }
            }
            Err(error) => {
                let code = match error {
                    mic::SelectionError::Loopback => dto::ResourceFaultCode::MicrophoneLoopback,
                    mic::SelectionError::Unavailable => dto::ResourceFaultCode::DeviceUnavailable,
                };
                tracing::warn!(target: "oink_microphone_routing", from_device_class=if error == mic::SelectionError::Loopback { "ours_virtual" } else { "unavailable" }, reason=if explicit.is_some() { "explicit_selection" } else { "no_alternative" }, "microphone input selection refused");
                self.record_fault_code(format!("microphone input selection: {error:?}"), code, now);
            }
        }
    }
    fn record_fault(&mut self, error: String, now: Instant) {
        self.record_fault_code(error, dto::ResourceFaultCode::BackendFailed, now);
    }
    fn record_fault_code(&mut self, error: String, code: dto::ResourceFaultCode, now: Instant) {
        tracing::warn!(?code, %error, "microphone source or receiver failed");
        self.fault = Some(code);
        let delay = self.retry_delay.max(PLAYBACK_RETRY_INITIAL);
        self.retry_at = Some(now + delay);
        self.retry_delay = (delay * 2).min(PLAYBACK_RETRY_MAX);
    }
    fn fail(&mut self, owner: &mut Owner, error: String, now: Instant) {
        self.stop_resources(owner);
        self.record_fault(error, now);
    }
    fn retry_if_due(&mut self, now: Instant) {
        if self.retry_at.is_some_and(|at| now >= at) {
            self.retry_at = None;
            self.fault = None;
            // Re-observe demand even if no device/consumer revision changed.
            // The selected input and virtual output remain pinned to the same
            // authorized endpoints; retry never falls back to another input.
            self.sampled = None;
        }
    }
    fn healthy(&mut self) {
        self.retry_at = None;
        self.retry_delay = PLAYBACK_RETRY_INITIAL;
    }
    pub(super) fn refresh(&mut self, owner: &mut Owner) {
        self.stop(owner);
        self.discovered = false;
        self.fault = None;
    }
    pub(super) fn active(&self) -> bool {
        self.capture.is_some() || self.receiving.is_some()
    }
    pub(super) fn output_quality(
        &self,
        at_ms: u64,
    ) -> [Option<oink_audio::sink::DropoutBucket>; oink_audio::sink::QUALITY_SECONDS] {
        self.output_quality.snapshot(at_ms)
    }
    pub(super) fn quality(&self, now: Instant) -> Option<crate::audio_quality::Observation> {
        self.receiving
            .as_ref()?
            .receiver
            .as_ref()
            .filter(|r| r.reader.payload_bytes() > 0)
            .map(|r| r.quality_observation(now, false))
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.source
            .leases()
            .map(|(_, l)| l.expires)
            .chain(self.receiving.as_ref().map(|r| r.renewed + mic::session::RENEW))
            .chain(self.retry_at)
            .min()
    }
    fn retire_source(&mut self, owner: &mut Owner) {
        self.capture = None;
        self.encoded = None;
        for (peer, lease) in self.source.leases() {
            if let Ok(session) = lease.prepared.session() {
                let stop =
                    dto::MicrophoneStop::from_raw([(1, session.write()), (2, Failure::RouteChanged.write())].into())
                        .unwrap();
                let _ = owner.streams.send(peer, &Message::MicrophoneStop(stop), Instant::now());
            }
            owner.streams.forget_microphone(peer);
        }
        self.source.clear();
    }
    fn stop_receiving(&mut self, owner: &mut Owner) {
        if let Some(receiving) = self.receiving.take() {
            if let Ok(stop) = dto::MicrophoneStop::from_raw(
                [(1, receiving.session.write()), (2, Failure::RouteChanged.write())].into(),
            ) {
                let _ = owner
                    .streams
                    .send(receiving.peer, &Message::MicrophoneStop(stop), Instant::now());
            }
            owner.streams.forget_microphone(receiving.peer);
        }
    }
    pub(super) fn stop(&mut self, owner: &mut Owner) {
        self.stop_resources(owner);
        self.fault = None;
        self.healthy();
    }
    fn stop_resources(&mut self, owner: &mut Owner) {
        self.retire_source(owner);
        self.stop_receiving(owner);
        self.monitor = None;
        self.demand = Default::default();
    }
    fn update_output(&mut self, owner: &mut Owner, output: Option<String>) {
        if output != self.output {
            self.stop_receiving(owner);
            self.monitor = None;
        }
        self.output = output;
    }
    fn capable(owner: &Owner, peer: DeviceId) -> bool {
        owner.current_boot(peer).is_some() && owner.group.peer_supports(peer, "oink.audio.microphone/1")
    }
    pub(super) fn tick(&mut self, owner: &mut Owner) {
        let now = Instant::now();
        let settings = lock(&owner.store).settings.microphone.clone();
        let input_changed = self.settings.as_ref().is_none_or(|old| old.input != settings.input);
        let revision = owner.platform.device_revision();
        let service = owner.platform.service_generation();
        if service != self.service {
            self.stop(owner);
            self.discovered = false;
            self.fault = None;
            self.service = service;
        }
        let previous_default = self.default_inputs.current.as_ref().map(|i| i.id.clone());
        let devices_changed = !self.discovered || revision != self.revision;
        if devices_changed {
            self.candidates = mic::input_candidates().unwrap_or_default();
            self.inputs = self.candidates.iter().map(|c| c.input.clone()).collect();
            match mic::default_input() {
                Ok(current) => self.default_inputs.observe(current, &self.candidates),
                Err(error) => {
                    self.default_inputs.observe(None, &self.candidates);
                    tracing::warn!(%error, "microphone default input inspection failed");
                }
            }
            #[cfg(target_os = "macos")]
            let output = match mic::virtual_output() {
                Ok(mic::VirtualOutput::Ready { render_id, .. }) => Some(render_id),
                _ => None,
            };
            #[cfg(target_os = "macos")]
            self.update_output(owner, output);
            self.revision = revision;
            self.discovered = true;
        }
        // Windows endpoint discovery completes asynchronously in the same cache
        // used by Settings. Observe it on every control tick, including completion
        // after a device revision, without another OS enumeration.
        #[cfg(windows)]
        self.update_output(owner, crate::microphone_plugin::virtual_output());
        if owner.paused || owner.power_state.sleeping || owner.update_suspended {
            self.stop(owner);
            self.publish(owner);
            return;
        }
        let resolution = owner.microphone.resolution;
        if self.route_claim != resolution.stamp() || self.settings.as_ref() != Some(&settings) {
            if self.route_claim != resolution.stamp() {
                self.stop(owner);
            }
            self.route_claim = resolution.stamp();
            self.settings = Some(settings.clone());
            self.fault = None;
        }
        self.retry_if_due(now);
        let source_boot = boot(owner.links.boot());
        let stamp = resolution.stamp().map(Stamp::to_wire);
        let removed = self.source.retain(now, |peer, lease| {
            resolution.owner() == Some(owner.links.id())
                && resolution.available()
                && Self::capable(owner, peer)
                && lease.prepared.session().is_ok_and(|s| {
                    s.source() == Ok(source_boot.clone())
                        && s.share_claim().ok() == stamp
                        && s.receiver_output().and_then(|o| o.receiver()).ok() == owner.current_boot(peer)
                })
        });
        for peer in removed {
            owner.streams.forget_microphone(peer);
        }
        let permitted = matches!(
            mic::permission(),
            mic::Permission::Granted | mic::Permission::SystemControlled
        );
        let local_claim = resolution.stamp().filter(|stamp| stamp.owner == owner.links.id());
        let selected_missing = self
            .selected
            .as_ref()
            .is_some_and(|id| !self.inputs.iter().any(|i| &i.id == id));
        if selected_missing && let Some(capture) = &mut self.capture {
            capture.stop_after_device_invalidation();
        }
        let default_changed = previous_default != self.default_inputs.current.as_ref().map(|i| i.id.clone());
        if self.selected_claim != local_claim
            || input_changed
            || selected_missing
            || (devices_changed && local_claim.is_some() && self.selected.is_none())
            || (settings.input.is_none() && default_changed)
            || (local_claim.is_some() && self.selected.is_none() && self.fault.is_none())
        {
            // Follow-default means observing actual default-input changes, not
            // freezing the first UID forever. Explicit selections stay pinned.
            self.retire_source(owner);
            self.fault = None;
            self.selected_claim = local_claim;
            self.selected = None;
            if local_claim.is_some() {
                self.select_input(settings.input.as_deref(), permitted, now, mic::switch_default_input);
            }
        }
        if !permitted && self.source.active() {
            self.retire_source(owner);
        }
        if !self.source.active() || !permitted {
            self.capture = None;
            self.encoded = None;
        } else if self.capture.is_none()
            && self.fault.is_none()
            && let Some(input) = &self.selected
        {
            if let Some(code) = self.selected_input_fault() {
                self.fail(
                    owner,
                    "select a hardware microphone, not a forwarding loopback device".into(),
                    now,
                );
                self.fault = Some(code);
                self.publish(owner);
                return;
            }
            let result = oink_audio::source::channel(StreamFormat::Opus5ms.config(), settings.volume_percent, false)
                .and_then(|(encoder, encoded)| {
                    encoded.set_muted(settings.muted);
                    let writer =
                        ForwardTable::source_writer_kind(owner.streams.microphone_table(), now, Kind::Microphone);
                    let links = owner.links.clone();
                    encoded.set_packet_writer(Arc::new(move |packet| {
                        let accepted = writer(packet);
                        if accepted && !packet.silent && !packet.sleep {
                            links.audio_activity();
                        }
                        accepted
                    }));
                    let capture = mic::Capture::start(
                        input,
                        mic::StartPermit {
                            user_enabled: true,
                            receiver_has_consumer: true,
                        },
                        encoder,
                    )?;
                    Ok((capture, encoded))
                });
            match result {
                Ok((capture, encoded)) => {
                    self.capture_watchdog.opened(now.max(Instant::now()));
                    self.capture = Some(capture);
                    self.encoded = Some(encoded);
                }
                Err(error) => self.fail(owner, error, now),
            }
        }
        let capture_error = self
            .capture
            .as_ref()
            .and_then(|c| c.error())
            .or_else(|| self.encoded.as_ref().and_then(|e| e.error()).map(str::to_owned));
        if let Some(error) = capture_error {
            self.fail(owner, error, now);
        } else if let Some(encoded) = &self.encoded {
            if self.capture_watchdog.stalled(now, encoded.frames()) {
                self.fail(owner, "microphone capture produced no frames".into(), now);
            } else if encoded.frames() != 0 {
                self.healthy();
            }
        }
        if let Some(encoded) = &mut self.encoded {
            owner.streams.report_media(Kind::Microphone, encoded, now);
            encoded.set_percent(settings.volume_percent);
            encoded.set_muted(settings.muted);
            while encoded.observation().is_some() {}
            let _ = encoded.levels();
        }
        let remote = resolution
            .owner()
            .filter(|p| *p != owner.links.id() && resolution.available() && Self::capable(owner, *p));
        if let Some(peer) = remote
            && !settings.receive_muted
            && self.output.is_some()
        {
            if self.monitor.is_none() && self.fault.is_none() {
                let wake = owner.streams.wake();
                match mic::DemandMonitor::with_wake(Arc::new(move || wake.notify_one())) {
                    Ok(monitor) => {
                        self.monitor = Some(monitor);
                        self.sampled = None;
                    }
                    Err(error) => self.fail(owner, error, now),
                }
            }
            if let Some(monitor) = &mut self.monitor
                && (self.sampled.is_none()
                    || (self.demand.active
                        && self
                            .sampled
                            .is_some_and(|at| now.duration_since(at) >= mic::session::RENEW))
                    || self.monitor_revision != monitor.revision())
            {
                self.monitor_revision = monitor.revision();
                let demand = monitor.sample();
                if let Some(error) = demand.error.clone() {
                    self.fail(owner, error, now);
                    self.publish(owner);
                    return;
                }
                if demand.epoch != self.demand.epoch || !demand.active {
                    self.stop_receiving(owner);
                }
                self.demand = demand;
                self.sampled = Some(now);
                if !self.demand.active {
                    self.healthy();
                }
            }
            if self.receiving.as_ref().is_some_and(|r| {
                r.peer != peer
                    || r.session.source().ok() != owner.current_boot(peer)
                    || r.session.share_claim().ok() != stamp
            }) {
                self.stop_receiving(owner);
            }
            if self.demand.active && self.receiving.is_none() && self.fault.is_none() {
                self.begin_receiving(owner, peer, now);
            }
            self.receive(owner, now);
            if self
                .receiving
                .as_ref()
                .and_then(|r| r.receiver.as_ref())
                .is_some_and(|r| r.reader.payload_bytes() > 0)
            {
                self.healthy();
            }
        } else {
            self.stop_receiving(owner);
            self.monitor = None;
            self.demand = Default::default();
        }
        self.publish(owner);
    }
    fn begin_receiving(&mut self, owner: &mut Owner, peer: DeviceId, now: Instant) {
        let Some(source) = owner.current_boot(peer) else { return };
        let Some(stamp) = owner.microphone.resolution.stamp() else {
            return;
        };
        let output = dto::ReceiverOutputBinding::from_raw(
            [
                (1, boot(owner.links.boot()).write()),
                (2, self.demand.epoch.max(1).write()),
                (3, self.output.clone().map(Text::<512>).write()),
            ]
            .into(),
        )
        .expect("validated virtual output binding");
        let mut request = [0; 16];
        if getrandom::fill(&mut request).is_err() {
            return;
        }
        let session = dto::MicrophoneSession::from_raw(
            [
                (1, source.write()),
                (2, output.write()),
                (3, stamp.to_wire().write()),
                (4, FixedBytes(request).write()),
            ]
            .into(),
        )
        .expect("microphone session");
        let demand = dto::MicrophoneDemand::from_raw(
            [
                (1, session.write()),
                (2, RawValue::Array(vec![StreamFormat::Opus5ms.descriptor().write()])),
            ]
            .into(),
        )
        .unwrap();
        let message = Message::MicrophoneDemand(demand);
        owner.streams.set_microphone_opening(peer, vec![message.clone()]);
        let _ = owner.streams.send(peer, &message, now);
        self.receiving = Some(Receiving {
            peer,
            session,
            prepared: None,
            renewed: now,
            ready: false,
            playback: None,
            receiver: None,
        });
    }
    fn receive(&mut self, owner: &mut Owner, now: Instant) {
        let Some(receiving) = &mut self.receiving else { return };
        let Some(prepared) = &receiving.prepared else {
            if now.duration_since(receiving.renewed) >= mic::session::RENEW {
                let demand = dto::MicrophoneDemand::from_raw(
                    [
                        (1, receiving.session.write()),
                        (2, RawValue::Array(vec![StreamFormat::Opus5ms.descriptor().write()])),
                    ]
                    .into(),
                )
                .unwrap();
                let _ = owner
                    .streams
                    .send(receiving.peer, &Message::MicrophoneDemand(demand), now);
                receiving.renewed = now;
            }
            return;
        };
        let stream_id = prepared.stream_id().unwrap();
        let table = owner.streams.microphone_table();
        let snapshot = table.load();
        let route = snapshot
            .from
            .iter()
            .find(|f| f.peer == receiving.peer && f.stream_id == stream_id);
        if let Some(route) = route {
            if receiving
                .receiver
                .as_ref()
                .is_some_and(|r| r.stream != route.packets.follows())
            {
                self.stop_receiving(owner);
                return;
            }
            if receiving.receiver.is_none() {
                let result = owner
                    .platform
                    .playback(self.output.as_deref().unwrap())
                    .and_then(|mut playback| {
                        playback.set_output_quality_counters(self.output_quality.clone());
                        let port = playback
                            .realtime_source(StreamFormat::Opus5ms.config())?
                            .ok_or("realtime microphone receiver unavailable")?;
                        let reader = Arc::new(MediaReader::new_kind(
                            table.clone(),
                            receiving.peer,
                            None,
                            Kind::Microphone,
                        ));
                        Receiver::connect_source(&port, &reader);
                        reader.give_realtime();
                        Ok((
                            playback,
                            Receiver {
                                quality_callback: owner.on_quality.clone(),
                                peer: receiving.peer,
                                stream: route.packets.follows(),
                                format: Negotiated {
                                    generation: stream_id,
                                    format: StreamFormat::Opus5ms,
                                },
                                source: Some(port),
                                idle_stats: Default::default(),
                                reader,
                                backlog: Default::default(),
                                dropped_at_start: route.packets.dropped(),
                                observed_sleep: false,
                                observed_burst: 0,
                                events: Default::default(),
                            },
                        ))
                    });
                match result {
                    Ok((playback, receiver)) => {
                        receiving.playback = Some(playback);
                        receiving.receiver = Some(receiver);
                    }
                    Err(error) => {
                        self.fail(owner, error, now);
                        return;
                    }
                }
            }
        }
        if let Some(error) = receiving.receiver.as_ref().and_then(|r| r.reader.health_error(now)) {
            self.fail(owner, error.into(), now);
            return;
        }
        if let Some(error) = receiving.playback.as_ref().and_then(|p| p.error()) {
            self.fail(owner, error, now);
            return;
        }
        if !receiving.ready
            && receiving.playback.is_none()
            && now.duration_since(receiving.renewed) >= mic::session::RENEW
        {
            self.stop_receiving(owner);
            return;
        }
        if receiving.playback.is_some() && !receiving.ready {
            // Native output open can outlast the consumer's StopIO. Reconfirm
            // after opening and before authorizing the source to capture.
            let demand = self
                .monitor
                .as_mut()
                .map(|monitor| monitor.sample())
                .unwrap_or_default();
            if let Some(error) = demand.error.clone() {
                self.fail(owner, error, now);
                return;
            }
            if !demand.active || demand.epoch != self.demand.epoch {
                self.demand = demand;
                self.stop_receiving(owner);
                return;
            }
            let ready = dto::MicrophoneReady::from_raw([(1, receiving.session.write()), (2, stream_id.write())].into())
                .unwrap();
            let message = Message::MicrophoneReady(ready);
            owner
                .streams
                .set_microphone_opening(receiving.peer, vec![message.clone()]);
            let _ = owner.streams.send(receiving.peer, &message, now);
            receiving.ready = true;
            receiving.renewed = now;
        }
        if receiving.ready && now.duration_since(receiving.renewed) >= mic::session::RENEW {
            let renew = dto::MicrophoneRenew::from_raw([(1, receiving.session.write()), (2, stream_id.write())].into())
                .unwrap();
            let _ = owner
                .streams
                .send(receiving.peer, &Message::MicrophoneRenew(renew), now);
            receiving.renewed = now;
        }
        if let Some(receiver) = &mut receiving.receiver
            && let Some(source) = &mut receiver.source
        {
            source.poll();
        }
    }
    pub(super) fn message(&mut self, owner: &mut Owner, peer: DeviceId, message: &Message) {
        if !Self::capable(owner, peer) || owner.paused || owner.power_state.sleeping || owner.update_suspended {
            return;
        }
        let now = Instant::now();
        match message {
            Message::AudioMediaState(state) => {
                if let Some(receiving) = &self.receiving
                    && receiving.peer == peer
                    && let Some(receiver) = &receiving.receiver
                {
                    receiver.reader.observe_media(state, now);
                }
            }
            Message::MicrophoneDemand(demand) => {
                if self.fault.is_some()
                    || !matches!(
                        mic::permission(),
                        mic::Permission::Granted | mic::Permission::SystemControlled
                    )
                {
                    return;
                }
                if owner.microphone.resolution.owner() != Some(owner.links.id())
                    || !owner.microphone.resolution.available()
                    || !owner.profile_allowed(peer, StreamFormat::Opus5ms)
                {
                    return;
                }
                let Ok(formats) = demand.formats() else { return };
                if !formats
                    .0
                    .iter()
                    .any(|d| StreamFormat::from_descriptor(d) == Ok(StreamFormat::Opus5ms))
                {
                    return;
                }
                let (Ok(session), Some(remote), Some(stamp)) = (
                    demand.session(),
                    owner.current_boot(peer),
                    owner.microphone.resolution.stamp(),
                ) else {
                    return;
                };
                let Ok(prepared) = self.source.offer(
                    peer,
                    session,
                    mic::session::SessionContext {
                        source: &boot(owner.links.boot()),
                        receiver: &remote,
                        claim: &stamp.to_wire(),
                    },
                    StreamFormat::Opus5ms,
                    now,
                ) else {
                    return;
                };
                let message = Message::MicrophonePrepared(prepared);
                owner.streams.set_microphone_opening(peer, vec![message.clone()]);
                let _ = owner.streams.send(peer, &message, now);
            }
            Message::MicrophoneReady(ready) => {
                let (Ok(session), Ok(stream)) = (ready.session(), ready.stream_id()) else {
                    return;
                };
                if self.source.ready(peer, &session, stream, now) {
                    owner
                        .streams
                        .admit_microphone_source(peer, stream, StreamFormat::Opus5ms);
                } else {
                    let stop = dto::MicrophoneStop::from_raw(
                        [(1, session.write()), (2, Failure::StateConflict.write())].into(),
                    )
                    .unwrap();
                    let _ = owner.streams.send(peer, &Message::MicrophoneStop(stop), now);
                }
            }
            Message::MicrophoneRenew(renew) => {
                if let (Ok(session), Ok(stream)) = (renew.session(), renew.stream_id())
                    && !self.source.renew(peer, &session, stream, now)
                {
                    let stop = dto::MicrophoneStop::from_raw(
                        [(1, session.write()), (2, Failure::StateConflict.write())].into(),
                    )
                    .unwrap();
                    let _ = owner.streams.send(peer, &Message::MicrophoneStop(stop), now);
                }
            }
            Message::MicrophoneStop(stop) => {
                if let Ok(session) = stop.session() {
                    if self.source.stop(peer, &session) {
                        owner.streams.forget_microphone(peer);
                    }
                    if self
                        .receiving
                        .as_ref()
                        .is_some_and(|r| r.peer == peer && r.session == session)
                    {
                        self.stop_receiving(owner);
                    }
                }
            }
            Message::MicrophonePrepared(prepared) => {
                let Some(receiving) = &mut self.receiving else { return };
                if receiving.peer != peer
                    || prepared.session().as_ref() != Ok(&receiving.session)
                    || prepared
                        .descriptor()
                        .ok()
                        .and_then(|d| StreamFormat::from_descriptor(&d).ok())
                        != Some(StreamFormat::Opus5ms)
                    || !owner.profile_allowed(peer, StreamFormat::Opus5ms)
                {
                    return;
                }
                let Ok(stream) = prepared.stream_id() else { return };
                if receiving.prepared.as_ref().is_some_and(|p| p.stream_id() != Ok(stream)) {
                    self.stop_receiving(owner);
                    return;
                }
                receiving.prepared = Some(prepared.clone());
                owner
                    .streams
                    .admit_microphone_receiver(peer, stream, StreamFormat::Opus5ms);
            }
            _ => {}
        }
    }
    fn publish(&mut self, owner: &mut Owner) {
        let permission = match mic::permission() {
            mic::Permission::Granted => dto::MicrophonePermission::Granted,
            mic::Permission::NotDetermined => dto::MicrophonePermission::NotDetermined,
            mic::Permission::Denied => dto::MicrophonePermission::Denied,
            mic::Permission::Restricted => dto::MicrophonePermission::Restricted,
            mic::Permission::SystemControlled => dto::MicrophonePermission::SystemControlled,
            mic::Permission::Unsupported => dto::MicrophonePermission::Unsupported,
        };
        let inputs = self.inputs.iter().take(64).filter_map(|i| {
            dto::MicrophoneInput::from_raw(
                [
                    (1, Text::<512>(i.id.clone()).write()),
                    (2, Text::<256>(i.name.clone()).write()),
                ]
                .into(),
            )
            .ok()
        });
        let receiving = self
            .receiving
            .as_ref()
            .filter(|r| {
                r.ready
                    && r.playback.is_some()
                    && r.receiver
                        .as_ref()
                        .is_some_and(|receiver| receiver.reader.payload_bytes() > 0)
                    && self.fault.is_none()
            })
            .map(|r| oink_network::wire_identity::device(r.peer));
        let observation = dto::MicrophoneObservation::from_raw(
            [
                (1, permission.write()),
                (2, self.output.is_some().write()),
                (3, RawValue::Array(inputs.map(|i| i.write()).collect())),
                (4, self.selected.clone().map(Text::<512>).write()),
                (5, self.demand.active.write()),
                (6, self.capture.is_some().write()),
                (7, receiving.write()),
                (
                    8,
                    self.fault
                        .as_ref()
                        .map(|code| dto::ResourceFault::from_raw([(1, code.write())].into()).unwrap())
                        .write(),
                ),
            ]
            .into(),
        );
        if let Ok(observation) = observation {
            let state = (observation.clone(), owner.microphone.claims.watermark().cloned());
            if self.published.as_ref() != Some(&state)
                && owner
                    .group
                    .publish_local_microphone(observation.clone(), owner.microphone.claims.watermark().cloned())
                    .is_ok()
            {
                self.published = Some(state);
            }
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    #[test]
    fn loopback_default_is_an_input_selection_fault_before_capture_opens() {
        let mut runtime = Runtime {
            selected: Some(format!("coreaudio:{}", mic::MAC_INPUT_UID)),
            ..Default::default()
        };
        assert!(runtime.selected_input_fault().is_some());
        runtime.selected = Some("coreaudio:hardware-mic".into());
        runtime.inputs = vec![mic::InputDevice {
            id: "coreaudio:hardware-mic".into(),
            name: "Hardware microphone".into(),
        }];
        assert!(runtime.selected_input_fault().is_none());
    }
    #[test]
    fn selector_excludes_renamed_current_legacy_and_native_identified_loopbacks() {
        let make = |id: &str, name: &str, forwarding_loopback| mic::InputCandidate {
            input: mic::InputDevice {
                id: id.into(),
                name: name.into(),
            },
            physical: true,
            built_in: false,
            forwarding_loopback,
        };
        let runtime = Runtime {
            candidates: vec![
                make(
                    &format!("coreaudio:{}", mic::MAC_INPUT_UID),
                    "Renamed current input",
                    false,
                ),
                make(
                    "coreaudio:com.across.microphone.input.v1",
                    "Renamed legacy input",
                    false,
                ),
                make("wasapi:renamed-vendor-endpoint", "Renamed cable", true),
                make("coreaudio:hardware", "Oink Microphone", false),
            ],
            ..Default::default()
        };
        let inputs = runtime.selectable_inputs();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].id, "coreaudio:hardware");
    }
    #[test]
    fn changed_system_default_clears_the_cached_loopback_and_old_fault() {
        let own = mic::InputDevice {
            id: format!("coreaudio:{}", mic::MAC_INPUT_UID),
            name: "Renamed virtual input".into(),
        };
        let hardware = mic::InputDevice {
            id: "coreaudio:hardware".into(),
            name: "Hardware input".into(),
        };
        let mut runtime = Runtime {
            selected: Some(own.id.clone()),
            fault: Some(dto::ResourceFaultCode::BackendFailed),
            candidates: vec![mic::InputCandidate {
                input: hardware.clone(),
                physical: true,
                built_in: true,
                forwarding_loopback: false,
            }],
            ..Default::default()
        };
        runtime.default_inputs.observe(Some(own), &runtime.candidates);
        runtime
            .default_inputs
            .observe(Some(hardware.clone()), &runtime.candidates);
        runtime.select_input(None, true, Instant::now(), |_, _| {
            panic!("a physical default must not be changed")
        });
        assert_eq!(runtime.selected.as_deref(), Some(hardware.id.as_str()));
        assert!(runtime.fault.is_none() && runtime.retry_at.is_none());
    }
    #[test]
    fn failed_switch_never_authorizes_capture_on_a_different_input() {
        let own = mic::InputDevice {
            id: format!("coreaudio:{}", mic::MAC_INPUT_UID),
            name: "Renamed virtual input".into(),
        };
        let hardware = mic::InputDevice {
            id: "coreaudio:hardware".into(),
            name: "Hardware input".into(),
        };
        let mut runtime = Runtime {
            inputs: vec![own.clone(), hardware.clone()],
            candidates: vec![
                mic::InputCandidate {
                    input: own.clone(),
                    physical: false,
                    built_in: false,
                    forwarding_loopback: true,
                },
                mic::InputCandidate {
                    input: hardware.clone(),
                    physical: true,
                    built_in: true,
                    forwarding_loopback: false,
                },
            ],
            ..Default::default()
        };
        runtime.default_inputs.observe(Some(own.clone()), &runtime.candidates);
        let mut writes = 0;
        runtime.select_input(None, true, Instant::now(), |from, to| {
            assert_eq!(from, own.id);
            assert_eq!(to, hardware.id);
            writes += 1;
            Err("native setter failed".into())
        });
        assert_eq!(writes, 1);
        assert!(runtime.selected.is_none());
        assert_eq!(runtime.fault, Some(dto::ResourceFaultCode::MicrophoneLoopback));
        runtime.select_input(Some(&own.id), true, Instant::now(), |_, _| {
            panic!("explicit selection must not change the system")
        });
        assert_eq!(runtime.fault, Some(dto::ResourceFaultCode::MicrophoneLoopback));
    }
    #[test]
    fn a_loopback_without_alternatives_keeps_backoff_until_selection_succeeds() {
        let own = mic::InputDevice {
            id: format!("coreaudio:{}", mic::MAC_INPUT_UID),
            name: "Virtual input".into(),
        };
        let mut runtime = Runtime {
            candidates: vec![mic::InputCandidate {
                input: own.clone(),
                physical: false,
                built_in: false,
                forwarding_loopback: true,
            }],
            ..Default::default()
        };
        runtime.default_inputs.observe(Some(own), &runtime.candidates);
        let now = Instant::now();
        runtime.select_input(None, true, now, |_, _| panic!("no replacement exists"));
        let first = runtime.retry_at.unwrap() - now;
        runtime.select_input(None, true, now, |_, _| panic!("no replacement exists"));
        assert!(runtime.retry_at.unwrap() - now > first);
    }
    #[test]
    fn fault_retry_does_not_need_a_revision_or_change_the_authorized_endpoint() {
        let mut runtime = Runtime {
            selected: Some("authorized-input".into()),
            output: Some("authorized-output".into()),
            ..Default::default()
        };
        let now = Instant::now();
        for attempt in 0..8 {
            let at = now + Duration::from_secs(attempt * 10);
            runtime.record_fault("demand sample failed".into(), at);
            let retry = runtime.retry_at.unwrap();
            assert!(retry > at && retry <= at + PLAYBACK_RETRY_MAX);
            runtime.retry_if_due(retry - Duration::from_nanos(1));
            assert!(runtime.fault.is_some());
            runtime.retry_if_due(retry);
            assert!(runtime.fault.is_none() && runtime.sampled.is_none());
            assert_eq!(runtime.selected.as_deref(), Some("authorized-input"));
            assert_eq!(runtime.output.as_deref(), Some("authorized-output"));
        }
        runtime.healthy();
        runtime.record_fault("encoding failed".into(), now);
        assert_eq!(runtime.retry_at, Some(now + PLAYBACK_RETRY_INITIAL));
    }
}
