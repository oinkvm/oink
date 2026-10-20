//! Local flight recorders are gathered only by a save. Every wire frame is Message.
#[cfg(all(test, debug_assertions))]
#[path = "recovery_wait_tests.rs"]
mod recovery_wait_tests;
use super::wire_values::{Value, dto};
#[cfg(all(test, debug_assertions))]
#[path = "collection_wire_tests.rs"]
mod wire_tests;
use super::*;
use crate::{
    config_module::ConfigChange,
    node::Node,
    streams::{StreamModule, diagnostics::DiagnosticsStreams},
};
use oink_network::link::{LinkLayer, LinkState, LowStream, ModuleEvent, ModulePort};
use oink_protocol::{
    frame::StreamKind,
    v1::{
        self, Message, RawValue, dto as w,
        schema::{WireDto, WireValue},
    },
};
const FORMAT: &str = "oink.diag-cbor/1";
const NONE: &str = "none";
const DEFLATE: &str = "deflate-raw";
const CHUNK: usize = 16 * 1024;
const HEADER: u64 = 4096;
// Framing allowance is bounded separately from the shared 64 MiB payload budget.
fn collection_options() -> oink_network::LowOptions {
    let receive = v1::limits::DIAGNOSTIC_COLLECTION as u64 + 4 * 1024 * 1024;
    oink_network::LowOptions {
        send_max: HEADER,
        receive_max: receive,
        operation_max: HEADER + receive,
    }
}
pub(super) const TIMEOUT: Duration = Duration::from_secs(30);
async fn idle<T, E: Into<anyhow::Error>>(
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T, E>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| anyhow::anyhow!("member:timeout"))?
        .map_err(Into::into)
}

/// Only lifecycle/phase boundaries, never payloads or per-chunk logs. The
/// limited duration is wall time observed under pacing, not claimed wait time.
pub(super) struct CollectionTrace {
    id: u64,
    role: &'static str,
    started: Instant,
    sampled: Instant,
    limited: Duration,
    bytes: u64,
    segment_work: Duration,
    complete: bool,
}
impl CollectionTrace {
    fn received(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes as u64)
            .ok_or_else(|| anyhow::anyhow!("member:size_limit"))?;
        anyhow::ensure!(
            self.bytes <= oink_protocol::v1::limits::DIAGNOSTIC_COLLECTION as u64,
            "member:size_limit"
        );
        Ok(())
    }
    pub(super) fn new(role: &'static str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let trace = Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            role,
            started: Instant::now(),
            sampled: Instant::now(),
            limited: Duration::ZERO,
            bytes: 0,
            segment_work: Duration::ZERO,
            complete: false,
        };
        trace.stage("start");
        trace
    }
    pub(super) fn finish(&mut self, bytes: u64, complete: bool) {
        self.bytes = bytes;
        self.complete = complete;
    }
    pub(super) fn stage(&self, stage: &'static str) {
        tracing::info!(target: "oink_collection_diagnostics", kind="collection_stage",
            collection_id=self.id, role=self.role, stage, bytes=self.bytes,
            duration_ms=self.started.elapsed().as_secs_f64()*1000.0, "diagnostics collection phase");
    }
    fn sample(&mut self, stream: &LowStream) {
        let now = Instant::now();
        if (stream.pacing_observer())().is_some_and(|p| p.limited) {
            self.limited += now.saturating_duration_since(self.sampled);
        }
        self.sampled = now;
    }
}
impl Drop for CollectionTrace {
    fn drop(&mut self) {
        tracing::info!(target: "oink_collection_diagnostics", kind="collection_end",
            collection_id=self.id, role=self.role, bytes=self.bytes,
            duration_ms=self.started.elapsed().as_secs_f64()*1000.0,
            limited_ms=(self.role == "sender").then_some(self.limited.as_secs_f64()*1000.0),
            segment_work_ms=self.segment_work.as_secs_f64()*1000.0, success=self.complete,
            "diagnostics collection finished");
    }
}

#[derive(Clone)]
pub(crate) struct Context {
    links: LinkLayer,
    config: crate::config_module::ConfigModule,
    pub(super) runtime: tokio::runtime::Handle,
    timeout: Duration,
    #[cfg(all(test, debug_assertions))]
    deadline_gate: Option<tokio::sync::watch::Receiver<bool>>,
    #[cfg(all(test, debug_assertions))]
    recovery_wait_entered: Option<tokio::sync::watch::Sender<Option<i64>>>,
    node: Option<Weak<crate::node::Inner>>,
}
impl Context {
    pub(crate) fn for_node(node: &Node) -> anyhow::Result<Self> {
        Ok(Self {
            links: node.links()?,
            config: node.link_config()?,
            runtime: tokio::runtime::Handle::current(),
            timeout: TIMEOUT,
            #[cfg(all(test, debug_assertions))]
            deadline_gate: None,
            #[cfg(all(test, debug_assertions))]
            recovery_wait_entered: None,
            node: Some(Arc::downgrade(&node.inner)),
        })
    }
    pub(super) async fn wait_recovery_ready(
        &self,
        snapshot: &store::Store,
        wake: &tokio::sync::Notify,
    ) -> anyhow::Result<()> {
        // Subscribe before reading snapshots, so readiness/membership changes
        // cannot be missed between the check and the await.
        let mut changes = self.config.subscribe();
        let mut stop = self
            .node
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|inner| Node { inner }.stop_receiver());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if stop.as_ref().is_some_and(|stop| *stop.borrow()) {
                snapshot.cancelled.store(true, Ordering::Release);
            }
            snapshot.check_cancelled()?;
            let links = self.links.links();
            if self
                .config
                .members()
                .members
                .iter()
                .all(|member| member.device == self.links.id() || links.ready(member.device))
            {
                return Ok(());
            }
            #[cfg(all(test, debug_assertions))]
            if let Some(entered) = &self.recovery_wait_entered {
                entered.send_replace(Some(snapshot.export_clock().utc_us));
            }
            tokio::select! {
                _ = async {
                    #[cfg(all(test, debug_assertions))]
                    if let Some(mut gate) = self.deadline_gate.clone() {
                        while !*gate.borrow_and_update() {
                            gate.changed().await.expect("recovery deadline controller remains alive");
                        }
                        return;
                    }
                    tokio::time::sleep_until(deadline).await;
                } => return snapshot.check_cancelled(),
                _ = wake.notified() => {},
                change = changes.next() => {
                    anyhow::ensure!(change.is_some(), "diagnostic collection configuration stopped");
                },
                _ = async {
                    if let Some(stop) = &mut stop { let _ = stop.changed().await; }
                    else { std::future::pending::<()>().await; }
                } => {},
            }
        }
    }

    async fn progress<T, E: Into<anyhow::Error>>(
        &self,
        work: impl std::future::Future<Output = Result<T, E>>,
    ) -> anyhow::Result<T> {
        #[cfg(all(test, debug_assertions))]
        if let Some(mut gate) = self.deadline_gate.clone() {
            return tokio::select! {
                result = work => result.map_err(Into::into),
                _ = async { while !*gate.borrow_and_update() { gate.changed().await.expect("deadline controller remains alive"); } }
                    => anyhow::bail!("member:timeout"),
            };
        }
        idle(self.timeout, work).await
    }
    pub(super) fn environment(&self, anon: &Anonymizer) -> Option<Snapshot> {
        let node = Node {
            inner: self.node.as_ref()?.upgrade()?,
        };
        Some(super::runtime_environment(&node, anon))
    }
    pub(super) fn sample(&self, recorder: &Recorder) {
        if let Some(inner) = self.node.as_ref().and_then(Weak::upgrade) {
            let node = Node { inner };
            node.report_visual_health();
            node.drain_input_trace();
        }
        self.config.flush_layout_diagnostics();
        recorder.refresh_environment();
        recorder.configuration(&self.config);
        recorder.sample_links(&self.links.links());
        recorder.sample_scheduler(&self.links.scheduler());
        if let Some(inner) = self.node.as_ref().and_then(Weak::upgrade) {
            let node = Node { inner };
            if let Ok(input) = node.input_thread() {
                recorder.sample_scheduler_role("Input", &input.scheduler_status());
                recorder.sample_scheduler_role("Inject", &input.injection_scheduler_status());
            }
        }
    }
    pub(super) fn export(
        &self,
        mut snapshot: store::Store,
        reason: Reason,
        dropped: u64,
        errors: u64,
    ) -> anyhow::Result<BundleInfo> {
        let mut trace = CollectionTrace::new("save");
        if snapshot.requested_range.is_none() {
            snapshot.prepare_range(reason, None)?;
        }
        let (start, end) = snapshot.requested_range.unwrap();
        trace.stage("prepared");
        let members = self.config.members().members.clone();
        let me = self.links.id();
        let group = self.config.applied().group;
        let anon = Anonymizer::new(group);
        snapshot.check_cancelled()?;
        anyhow::ensure!(
            snapshot.device == anon.device(me),
            "diagnostic layout domain changed during export"
        );
        if let Some(inner) = self.node.as_ref().and_then(Weak::upgrade) {
            Node { inner }.drain_input_trace();
        }
        self.config.flush_layout_diagnostics();
        // Automatic/recovery exports also refresh the local layout here; their
        // immutable timeline snapshot may have been fixed earlier than this save.
        snapshot.snapshot.layout = self.config.diagnostic_layout(&anon);
        let ready = self.links.links();
        let mut targets = Vec::new();
        for member in members.iter().filter(|m| m.device != me) {
            let device = anon.device(member.device);
            snapshot.missing_members.insert(device.clone(), "offline".into());
            if let Some(link) = ready.by_peer(member.device).filter(|l| l.state == LinkState::Ready) {
                targets.push((member.device, link.link, device));
            }
        }
        let snapshot = Arc::new(Mutex::new(snapshot));
        self.runtime.block_on(async {
            let mut transfers = tokio::task::JoinSet::new();
            for (peer, link, device) in targets {
                let context = self.clone();
                let snapshot = snapshot.clone();
                transfers.spawn(async move {
                    let result = context.gather(peer, link, &device, start, end, snapshot.clone()).await;
                    let mut store = lock(&snapshot);
                    let reason = match result {
                        Ok(()) => {
                            if store.segments.iter().any(|m| m.device == device) {
                                store.missing_members.remove(&device);
                                return;
                            }
                            "no_recording_in_range".to_owned()
                        }
                        Err(error) => {
                            // Error strings can include endpoint addresses: manifest stores fixed reasons only.
                            let text = error.to_string();
                            match text.as_str() {
                                "member:timeout" => "timeout",
                                "member:size_limit" => "size_limit",
                                "member:recording_disabled" => "recording_disabled",
                                "member:recovering" => "recovering",
                                "member:unsupported" => "unsupported",
                                "member:partial_coverage" => "partial_coverage",
                                _ => "transfer_failed_or_unsupported",
                            }
                            .to_owned()
                        }
                    };
                    store.missing_members.insert(device, reason);
                });
            }
            while transfers.join_next().await.is_some() {}
        });
        let mut store = lock(&snapshot);
        // Final four-stamp calibration can refine the provisional source bounds.
        // Clip staging segments, so coverage describes exactly the exported rows.
        store.restrict_range(start, end)?;
        for member in members.iter().filter(|m| m.device != me) {
            let device = anon.device(member.device);
            if !store.segments.iter().any(|m| m.device == device) {
                store
                    .missing_members
                    .entry(device)
                    .or_insert_with(|| "no_recording_in_range".into());
            }
        }
        let complete = store.missing_members.is_empty();
        trace.stage("zip");
        let bundle = store.export_sync(reason, dropped, errors, Some(complete))?;
        trace.bytes = bundle.bytes;
        trace.complete = complete;
        Ok(bundle)
    }
    fn rich(&self, peer: DeviceId) -> bool {
        self.config
            .peer_agreement(peer)
            .is_some_and(|a| a.contains("oink.diagnostics.layout/1"))
    }
    fn check(&self, link: oink_network::link::LinkId, type_id: u16, valid: bool) -> anyhow::Result<()> {
        if !valid {
            crate::streams::wire::report_low_payload(
                &self.links,
                link,
                StreamKind::Diagnostics,
                type_id,
                v1::ErrorCode::MalformedFrame,
            );
            anyhow::bail!("invalid diagnostic operation");
        }
        Ok(())
    }
    async fn receive(
        &self,
        stream: &mut LowStream,
        changes: &mut crate::config_module::ConfigChanges,
        request: [u8; 16],
    ) -> anyhow::Result<Option<Message>> {
        let link = stream.link();
        let peer = stream.peer();
        self.progress(async {
            loop {
                tokio::select! {
                    message = stream.recv() => return message.map_err(anyhow::Error::from),
                    change = changes.next() => {
                        let Some(change) = change else {
                            anyhow::bail!("diagnostic collection configuration stopped");
                        };
                        if let ConfigChange::ProtocolError {link: source, peer: sender, error} = change
                            && source == link && sender == peer
                            && error.scope()? == w::ErrorScope::LowOperation
                            && error.operation()?.is_some_and(|operation| operation.0 == request)
                        {
                            if error.code()? == v1::ErrorCode::UnsupportedValue.number() as u16 {
                                anyhow::bail!("member:unsupported");
                            }
                            anyhow::bail!("member:protocol_error");
                        }
                    }
                }
            }
        })
        .await
    }
    async fn gather(
        &self,
        peer: DeviceId,
        link: oink_network::link::LinkId,
        device: &str,
        start: i64,
        end: i64,
        snapshot: Arc<Mutex<store::Store>>,
    ) -> anyhow::Result<()> {
        let mut trace = CollectionTrace::new("receiver");
        let mut changes = self.config.subscribe();
        let mut stream = self
            .progress(self.links.open_low(link, StreamKind::Diagnostics, collection_options()))
            .await?;
        let valid = stream.effect_validity();
        let t1 = lock(&snapshot).current_stamp();
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).map_err(|_| anyhow::anyhow!("diagnostic request entropy"))?;
        let request_id = w::Id16::read(&RawValue::Bytes(id.to_vec()))?;
        let request = dto::<w::DiagnosticsRequest>([
            ("t1", t1.wire()?),
            ("start_us", start.wire()?),
            ("end_us", end.wire()?),
            ("request_id", request_id.write()),
            ("accepted_formats", strings(&[FORMAT])),
            ("accepted_compressions", strings(&[DEFLATE, NONE])),
        ])?;
        self.progress(stream.send(&Message::DiagnosticsRequest(request)))
            .await?;
        let first = self
            .receive(&mut stream, &mut changes, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing diagnostic response"))?;
        let response = match first {
            Message::DiagnosticsResponse(r) => r,
            Message::DiagnosticsUnavailable(r) => {
                self.check(link, 0x0a07, r.request_id()? == request_id)?;
                anyhow::bail!("member:{}", failure_reason(r.reason()?));
            }
            other => {
                self.check(link, other.type_id(), false)?;
                unreachable!()
            }
        };
        let compression = response.compression()?.0;
        self.check(
            link,
            0x0a03,
            response.request_id()? == request_id
                && response.format()?.0 == FORMAT
                && [NONE, DEFLATE].contains(&compression.as_str())
                && store::token_ok(&response.instance()?.0),
        )?;
        let instance = response.instance()?.0;
        let t4 = lock(&snapshot).current_stamp();
        {
            let mut store = lock(&snapshot);
            store
                .evicted_by_device
                .insert(device.into(), response.evicted_segments()?);
            if let Some(mut sample) = clock_sample(
                device.into(),
                instance.clone(),
                t1,
                stamp(&response.t2()?)?,
                stamp(&response.t3()?)?,
                t4,
            ) {
                sample.local_instance = store.instance.clone();
                sample.first_source_mono_us = 0;
                store.clocks.push(sample);
            }
        }
        let mut payload = Vec::new();
        let mut environment = false;
        let mut segment_id = 0u64;
        loop {
            let message = self
                .receive(&mut stream, &mut changes, id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing diagnostic end"))?;
            anyhow::ensure!(
                (valid)() && self.config.members().members.iter().any(|m| m.device == peer),
                "collection revoked"
            );
            match message {
                Message::DiagnosticsEnvironment(e) => {
                    self.check(
                        link,
                        0x0a09,
                        !environment
                            && payload.is_empty()
                            && segment_id == 0
                            && e.request_id()? == request_id
                            && e.format()?.0 == FORMAT,
                    )?;
                    let remote = e.snapshot()?;
                    self.check(link, 0x0a09, self.rich(peer) || remote.layout()?.is_none())?;
                    let size = remote.encode(MAX_SEGMENT_RAW)?.len();
                    trace.received(size)?;
                    let mut store = lock(&snapshot);
                    store.receive_bytes(size)?;
                    store.receive_expanded(size)?;
                    store.network_environment(device, &instance, remote)?;
                    environment = true;
                }
                Message::DiagnosticsChunk(c) => {
                    let bytes = c.bytes()?.0;
                    self.check(
                        link,
                        0x0a04,
                        environment
                            && c.request_id()? == request_id
                            && c.segment_id()? == segment_id
                            && c.offset()? as usize == payload.len()
                            && !bytes.is_empty()
                            && bytes.len() <= CHUNK,
                    )?;
                    anyhow::ensure!(payload.len() + bytes.len() <= MAX_SEGMENT_RAW, "member:size_limit");
                    trace.received(bytes.len())?;
                    lock(&snapshot).receive_bytes(bytes.len())?;
                    payload.extend(bytes);
                }
                Message::DiagnosticsSegmentEnd(e) => {
                    self.check(
                        link,
                        0x0a05,
                        environment
                            && e.request_id()? == request_id
                            && e.segment_id()? == segment_id
                            && !payload.is_empty()
                            && e.compressed_length()? as usize == payload.len()
                            && e.raw_length()? as usize <= MAX_SEGMENT_RAW,
                    )?;
                    let digest = e.digest()?;
                    self.check(
                        link,
                        0x0a05,
                        digest.algorithm()?.0 == "sha256"
                            && digest.bytes()?.0.as_slice() == v1::retained::sha256(&payload).0,
                    )?;
                    lock(&snapshot).receive_expanded(e.raw_length()? as usize)?;
                    let bytes = std::mem::take(&mut payload);
                    let compression = compression.clone();
                    let destination = snapshot.clone();
                    let validity = valid.clone();
                    let device = device.to_owned();
                    let rich = self.rich(peer);
                    let raw_length = e.raw_length()? as usize;
                    let work = tokio::task::spawn_blocking(move || -> anyhow::Result<Duration> {
                        let started = Instant::now();
                        anyhow::ensure!((validity)(), "collection revoked");
                        let raw = expand(&bytes, &compression, raw_length)?;
                        let segment = store::wire::segment(&raw)?;
                        anyhow::ensure!(rich || baseline(&segment)?, "unnegotiated diagnostic layout");
                        anyhow::ensure!((validity)(), "collection revoked");
                        lock(&destination).add_network(&device, &raw, &segment)?;
                        Ok(started.elapsed())
                    })
                    .await?;
                    match work {
                        Ok(work) => trace.segment_work += work,
                        Err(error) => {
                            crate::streams::wire::report_low_payload(
                                &self.links,
                                link,
                                StreamKind::Diagnostics,
                                0x0a05,
                                v1::ErrorCode::MalformedFrame,
                            );
                            return Err(error);
                        }
                    }
                    segment_id += 1;
                }
                Message::DiagnosticsEnd(e) => {
                    let partial = e.partial()? || e.coverage()?.0.iter().any(|c| !c.complete().unwrap_or(false));
                    self.check(
                        link,
                        0x0a06,
                        (environment || (partial && segment_id == 0))
                            && payload.is_empty()
                            && e.request_id()? == request_id
                            && e.evicted_segments()? >= response.evicted_segments()?,
                    )?;
                    for coverage in e.coverage()?.0 {
                        self.check(link, 0x0a06, coverage.start_us()? <= coverage.end_us()?)?;
                    }
                    if !partial {
                        let covered = {
                            let store = lock(&snapshot);
                            store.segments.iter().filter(|m| m.device == device).all(|m| {
                                e.coverage().is_ok_and(|ranges| {
                                    ranges.0.iter().any(|r| {
                                        r.complete() == Ok(true)
                                            && r.start_us().is_ok_and(|s| s <= m.first_us)
                                            && r.end_us().is_ok_and(|e| e >= m.last_us)
                                    })
                                })
                            })
                        };
                        self.check(link, 0x0a06, covered)?;
                    }
                    {
                        let mut store = lock(&snapshot);
                        store.evicted_by_device.insert(device.into(), e.evicted_segments()?);
                        store.network_ends.insert(device.into(), e);
                    }
                    stream.finish();
                    self.check(link, 0x0a06, self.progress(stream.recv()).await?.is_none())?;
                    if partial {
                        anyhow::bail!("member:partial_coverage");
                    }
                    trace.complete = true;
                    return Ok(());
                }
                Message::DiagnosticsUnavailable(e) => {
                    self.check(link, 0x0a07, e.request_id()? == request_id)?;
                    anyhow::bail!("member:{}", failure_reason(e.reason()?));
                }
                other => {
                    self.check(link, other.type_id(), false)?;
                    unreachable!()
                }
            }
        }
    }
}
fn failure_reason(reason: w::CollectionFailure) -> &'static str {
    match reason {
        w::CollectionFailure::Disabled => "recording_disabled",
        w::CollectionFailure::Recovering => "recovering",
        _ => "unsupported",
    }
}
#[derive(Clone)]
pub(crate) struct Collector {
    pub recorder: Recorder,
    context: Context,
}
impl Collector {
    pub fn save_group_range(&self, last_minutes: Option<u32>) -> anyhow::Result<BundleInfo> {
        self.recorder.save_range(last_minutes)
    }
    pub async fn group_changed(&self, group: Option<GroupId>) -> anyhow::Result<()> {
        if *lock(&self.recorder.inner.group) == group {
            return Ok(());
        }
        let recorder = self.recorder.clone();
        let config = self.context.config.clone();
        tokio::task::spawn_blocking(move || {
            recorder.rekey(recorder.inner.me, group)?;
            config.observe_layout(&recorder).map_err(anyhow::Error::from)
        })
        .await?
    }
    async fn serve(&self, mut stream: LowStream, first: Message) -> anyhow::Result<()> {
        let mut trace = CollectionTrace::new("sender");
        let peer = stream.peer();
        let link = stream.link();
        let valid = stream.effect_validity();
        anyhow::ensure!(
            (valid)()
                && self
                    .context
                    .links
                    .links()
                    .by_peer(peer)
                    .is_some_and(|l| l.link == link && l.state == LinkState::Ready)
                && self.context.config.members().members.iter().any(|m| m.device == peer),
            "requester not ready member"
        );
        anyhow::ensure!(
            v1::frame::encode(&first, v1::limits::RELIABLE_FRAME)?.len() <= HEADER as usize,
            "invalid diagnostic header"
        );
        let Message::DiagnosticsRequest(request) = first else {
            anyhow::bail!("invalid diagnostic request")
        };
        let request_id = request.request_id()?;
        self.context.check(link, 0x0a02, validate_request(&request).is_ok())?;
        let t2 = self.recorder.inner.clock.now();
        let formats = request.accepted_formats()?.0;
        let compressions = request.accepted_compressions()?.0;
        let compression = if compressions.iter().any(|c| c.0 == DEFLATE) {
            DEFLATE
        } else {
            NONE
        };
        if !formats.iter().any(|f| f.0 == FORMAT) || !compressions.iter().any(|c| c.0 == compression) {
            let error = dto::<w::ProtocolError>([
                ("code", (v1::ErrorCode::UnsupportedValue.number() as u16).wire()?),
                ("scope", w::ErrorScope::LowOperation.write()),
                ("offending_type", 0x0a02u16.wire()?),
                ("operation", RawValue::Bytes(request_id.0.to_vec())),
            ])?;
            // C0 owns protocol errors; Low carries only its correlated refusal.
            let _ = self.context.config.send_protocol_error(link, error);
            self.context
                .progress(
                    stream.send(&Message::DiagnosticsUnavailable(dto::<w::DiagnosticsUnavailable>([
                        ("reason", w::CollectionFailure::Unsupported.write()),
                        ("request_id", request_id.write()),
                    ])?)),
                )
                .await?;
            stream.finish();
            let _ = self.context.progress(stream.recv()).await;
            return Ok(());
        }
        let unavailable = if !self.recorder.inner.enabled.load(Ordering::Acquire) {
            Some(w::CollectionFailure::Disabled)
        } else if self.recorder.ensure_recovered().is_err() {
            Some(w::CollectionFailure::Recovering)
        } else {
            None
        };
        if let Some(reason) = unavailable {
            self.context
                .progress(
                    stream.send(&Message::DiagnosticsUnavailable(dto::<w::DiagnosticsUnavailable>([
                        ("reason", reason.write()),
                        ("request_id", request_id.write()),
                    ])?)),
                )
                .await?;
            stream.finish();
            self.context
                .check(link, 0x0a02, self.context.progress(stream.recv()).await?.is_none())?;
            return Ok(());
        }
        let recorder = self.recorder.clone();
        let context = self.context.clone();
        let mut source = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            context.sample(&recorder);
            let mut store = lock(&recorder.inner.store);
            anyhow::ensure!(recorder.inner.enabled.load(Ordering::Acquire), "diagnostics disabled");
            if !recorder.inner.queue.load().is_empty() {
                recorder.drain(&mut store)?;
            }
            store.prune()?;
            store.export_snapshot()
        })
        .await??;
        let t1 = stamp(&request.t1()?)?;
        let shift = t2.utc_us.saturating_sub(t1.utc_us);
        let start = if request.start_us()? == i64::MIN {
            i64::MIN
        } else {
            request.start_us()?.saturating_add(shift)
        };
        let end = request.end_us()?.saturating_add(shift);
        source.restrict_range(start, end)?;
        let rich = self.context.rich(peer);
        self.context
            .progress(
                stream.send(&Message::DiagnosticsResponse(dto::<w::DiagnosticsResponse>([
                    ("instance", self.recorder.inner.instance.wire()?),
                    ("evicted_segments", source.evicted.wire()?),
                    ("t2", t2.wire()?),
                    ("t3", self.recorder.inner.clock.now().wire()?),
                    ("request_id", request_id.write()),
                    ("format", RawValue::from(FORMAT)),
                    ("compression", RawValue::from(compression)),
                ])?)),
            )
            .await?;
        let environment = wire_values::snapshot(&source.snapshot, rich)
            .and_then(|environment| Ok((environment.encode(MAX_SEGMENT_RAW)?.len(), environment)));
        let (size, environment) = match environment {
            Ok(value) => value,
            Err(_) => {
                let coverage = dto::<w::Coverage>([
                    ("start_us", start.wire()?),
                    ("end_us", end.wire()?),
                    ("complete", false.wire()?),
                    ("failure", (v1::ErrorCode::LimitExceeded.number() as u16).wire()?),
                ])?;
                self.context
                    .progress(stream.send(&Message::DiagnosticsEnd(dto::<w::DiagnosticsEnd>([
                        ("request_id", request_id.write()),
                        ("coverage", RawValue::Array(vec![coverage.write()])),
                        ("partial", true.wire()?),
                        ("evicted_segments", source.evicted.wire()?),
                    ])?)))
                    .await?;
                stream.finish();
                let _ = self.context.progress(stream.recv()).await;
                return Ok(());
            }
        };
        trace.received(size)?;
        self.context
            .progress(
                stream.send(&Message::DiagnosticsEnvironment(dto::<w::DiagnosticsEnvironment>([
                    ("snapshot", environment.write()),
                    ("request_id", request_id.write()),
                    ("format", RawValue::from(FORMAT)),
                ])?)),
            )
            .await?;
        let mut raw_total = size as u64;
        let mut coverage = Vec::new();
        let mut partial = false;
        for (segment_id, meta) in source.segments.iter().enumerate() {
            anyhow::ensure!(
                (valid)()
                    && self.recorder.inner.enabled.load(Ordering::Acquire)
                    && self.context.config.members().members.iter().any(|m| m.device == peer),
                "collection revoked"
            );
            let work_started = Instant::now();
            let bytes = (|| -> anyhow::Result<_> {
                let segment = store::decode(&source.read(meta)?)?;
                let raw = wire_values::segment(&segment, rich)?.encode(MAX_SEGMENT_RAW)?;
                let bytes = compress(&raw, compression)?;
                Ok((raw.len(), bytes))
            })();
            trace.segment_work += work_started.elapsed();
            let (raw_len, bytes) = match bytes {
                Ok(v) => v,
                Err(_) => {
                    partial = true;
                    break;
                }
            };
            if segment_id >= 4096
                || trace.bytes + bytes.len() as u64 > v1::limits::DIAGNOSTIC_COLLECTION as u64
                || raw_total + raw_len as u64 > v1::limits::DIAGNOSTIC_COLLECTION as u64
            {
                partial = true;
                break;
            }
            raw_total += raw_len as u64;
            for (offset, chunk) in bytes.chunks(CHUNK).enumerate() {
                anyhow::ensure!(
                    (valid)() && self.recorder.inner.enabled.load(Ordering::Acquire),
                    "collection revoked"
                );
                self.context
                    .progress(stream.send(&Message::DiagnosticsChunk(dto::<w::DiagnosticsChunk>([
                        ("bytes", RawValue::Bytes(chunk.to_vec())),
                        ("request_id", request_id.write()),
                        ("segment_id", (segment_id as u64).wire()?),
                        ("offset", ((offset * CHUNK) as u32).wire()?),
                    ])?)))
                    .await?;
                trace.received(chunk.len())?;
                trace.sample(&stream);
            }
            let digest = dto::<w::Digest>([
                ("algorithm", RawValue::from("sha256")),
                ("bytes", v1::retained::sha256(&bytes).write()),
            ])?;
            self.context
                .progress(
                    stream.send(&Message::DiagnosticsSegmentEnd(dto::<w::DiagnosticsSegmentEnd>([
                        ("request_id", request_id.write()),
                        ("segment_id", (segment_id as u64).wire()?),
                        ("compressed_length", (bytes.len() as u32).wire()?),
                        ("raw_length", (raw_len as u32).wire()?),
                        ("digest", digest.write()),
                    ])?)),
                )
                .await?;
            coverage.push(
                dto::<w::Coverage>([
                    ("start_us", meta.first_us.wire()?),
                    ("end_us", meta.last_us.wire()?),
                    ("complete", true.wire()?),
                ])?
                .write(),
            );
        }
        if partial {
            coverage.push(
                dto::<w::Coverage>([
                    ("start_us", start.wire()?),
                    ("end_us", end.wire()?),
                    ("complete", false.wire()?),
                    ("failure", (v1::ErrorCode::LimitExceeded.number() as u16).wire()?),
                ])?
                .write(),
            );
        }
        self.context
            .progress(stream.send(&Message::DiagnosticsEnd(dto::<w::DiagnosticsEnd>([
                ("request_id", request_id.write()),
                ("coverage", RawValue::Array(coverage)),
                ("partial", partial.wire()?),
                ("evicted_segments", source.evicted.wire()?),
            ])?)))
            .await?;
        stream.finish();
        self.context
            .check(link, 0x0a02, self.context.progress(stream.recv()).await?.is_none())?;
        trace.complete = !partial;
        Ok(())
    }
}
fn validate_request(request: &w::DiagnosticsRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        request.start_us()? <= request.end_us()? && request.end_us()? <= request.t1()?.utc_us()?,
        "invalid diagnostic bounds"
    );
    Ok(())
}
fn strings(values: &[&str]) -> RawValue {
    RawValue::Array(values.iter().map(|v| RawValue::from(*v)).collect())
}
fn stamp(value: &w::ClockStamp) -> v1::Result<clock::Stamp> {
    Ok(clock::Stamp {
        mono_us: value.mono_us()?,
        utc_us: value.utc_us()?,
        clock_id: value.clock_id()?,
    })
}
fn compress(raw: &[u8], compression: &str) -> anyhow::Result<Vec<u8>> {
    if compression == NONE {
        return Ok(raw.to_vec());
    }
    use std::io::Write;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(raw)?;
    let bytes = encoder.finish()?;
    anyhow::ensure!(bytes.len() <= MAX_SEGMENT_RAW, "member:size_limit");
    Ok(bytes)
}
fn expand(bytes: &[u8], compression: &str, raw_length: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    anyhow::ensure!(
        bytes.len() <= MAX_SEGMENT_RAW && raw_length <= MAX_SEGMENT_RAW,
        "member:size_limit"
    );
    let raw = if compression == NONE {
        bytes.to_vec()
    } else {
        let mut decoder = flate2::read::DeflateDecoder::new(bytes);
        let mut raw = Vec::new();
        (&mut decoder).take(MAX_SEGMENT_RAW as u64 + 1).read_to_end(&mut raw)?;
        anyhow::ensure!(decoder.total_in() as usize == bytes.len(), "trailing compressed data");
        raw
    };
    anyhow::ensure!(raw.len() == raw_length, "invalid diagnostic expansion length");
    Ok(raw)
}
fn baseline(segment: &w::DiagnosticSegment) -> v1::Result<bool> {
    Ok(segment.snapshot()?.layout()?.is_none()
        && segment.rows()?.0.iter().all(|row| {
            row.event()
                .and_then(|e| e.kind())
                .is_ok_and(|kind| kind.0 != "layout-change/1")
        }))
}
fn clock_sample(
    device: String,
    instance: String,
    t1: clock::Stamp,
    t2: clock::Stamp,
    t3: clock::Stamp,
    t4: clock::Stamp,
) -> Option<ClockSample> {
    if t1.clock_id < 0
        || t2.clock_id < 0
        || t1.clock_id != t4.clock_id
        || t2.clock_id != t3.clock_id
        || t1.mono_us > t4.mono_us
        || t2.mono_us > t3.mono_us
    {
        return None;
    }
    let mut sample = ClockSample::estimate(device, instance, t1.utc_us, t2.utc_us, t3.utc_us, t4.utc_us)?;
    sample.uncertainty_us = sample.uncertainty_us.saturating_add(4 * clock::EPOCH_TOLERANCE_US);
    sample.first_source_mono_us = t2.mono_us;
    sample.last_source_mono_us = t3.mono_us;
    sample.source_clock = t3.clock_id;
    sample.local_clock = t4.clock_id;
    Some(sample)
}

pub(crate) fn start_link_runtime(
    node: Node,
    mut port: ModulePort,
    recorder: Recorder,
) -> anyhow::Result<(Collector, tokio::task::JoinHandle<()>)> {
    let links = node.links()?;
    let group = node.link_config()?;
    let context = recorder
        .inner
        .collection
        .get()
        .expect("startup collection context")
        .clone();
    let collector = Collector {
        recorder: recorder.clone(),
        context,
    };
    let handle = collector.clone();
    let task = tokio::spawn(async move {
        let mut owner = DiagnosticsStreams::new(links, group.clone());
        owner.set_recorder(recorder.clone());
        owner.set_hello(
            recorder.inner.instance.clone(),
            recorder.enabled(),
            recorder.inner.previous_instance.clone(),
        );
        let wake = owner.wake();
        let mut changes = group.subscribe();
        let mut stop = node.stop_receiver();
        let mut serving = tokio::task::JoinSet::new();
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        loop {
            let deadline = owner.deadline();
            tokio::select! {
                _ = stop.changed() => break,
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady {link, peer}) => owner.link_ready(link, peer),
                    Some(ModuleEvent::LinkLost {link, peer, reason}) => owner.link_lost(link, peer, reason),
                    Some(ModuleEvent::Inbound(stream)) => owner.inbound(stream),
                    Some(ModuleEvent::InboundLow(operation)) => owner.inbound_low(*operation),
                    Some(ModuleEvent::StreamClosed {stream, reason}) => owner.stream_closed(stream, reason),
                    Some(ModuleEvent::PathChanged {link, path}) => owner.path_changed(link, path),
                    Some(ModuleEvent::ProtocolViolation {peer,violation}) => recorder.protocol_violation(peer,&violation),
                    None => break,
                },
                change = changes.next() => match change {
                    Some(ConfigChange::Facts {device, capabilities_changed}) => owner.facts_changed(device, capabilities_changed),
                    Some(ConfigChange::State {..}) => { let _ = collector.group_changed(Some(group.applied().group)).await; recorder.configuration(&group); },
                    Some(_) => {}, None => break,
                },
                _ = wake.notified() => {},
                _ = async { if let Some(deadline) = deadline { tokio::time::sleep_until(deadline.into()).await } else { std::future::pending::<()>().await } } => {},
                _ = serving.join_next(), if !serving.is_empty() => {},
            }
            owner.poll(Instant::now());
            owner.take_closed();
            for operation in owner.take_low() {
                let collector = collector.clone();
                let slots = slots.clone();
                serving.spawn(async move {
                    if let Ok(Ok(_permit)) = tokio::time::timeout(TIMEOUT, slots.acquire()).await {
                        let _ = collector.serve(operation.stream, operation.first).await;
                    }
                });
            }
        }
        serving.abort_all();
        while serving.join_next().await.is_some() {}
    });
    Ok((handle, task))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::streams::lifecycle_tests::Fixture;
    use std::io::Read;
    struct Frozen(i64);
    impl clock::Source for Frozen {
        fn read(&self) -> (u64, i64) {
            (0, self.0)
        }
    }
    pub(super) async fn incoming(port: &mut ModulePort) -> oink_network::link::InboundLow {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(ModuleEvent::InboundLow(low)) = port.next().await {
                    return *low;
                }
            }
        })
        .await
        .unwrap()
    }
    pub(super) fn context(links: LinkLayer, config: crate::config_module::ConfigModule) -> Context {
        Context {
            links,
            config,
            runtime: tokio::runtime::Handle::current(),
            timeout: TIMEOUT,
            deadline_gate: None,
            #[cfg(all(test, debug_assertions))]
            recovery_wait_entered: None,
            node: None,
        }
    }
    pub(super) fn request(t1: clock::Stamp, start: i64, end: i64) -> w::DiagnosticsRequest {
        dto::<w::DiagnosticsRequest>([
            ("t1", t1.wire().unwrap()),
            ("start_us", start.wire().unwrap()),
            ("end_us", end.wire().unwrap()),
            ("request_id", RawValue::Bytes(vec![1; 16])),
            ("accepted_formats", strings(&[FORMAT])),
            ("accepted_compressions", strings(&[NONE])),
        ])
        .unwrap()
    }
    pub(super) fn pause(config: &crate::config_module::ConfigModule) {
        let facts = config.local_facts();
        let old = facts.pause().unwrap().unwrap();
        let pause = dto::<w::LocalPauseState>([
            ("paused", true.write()),
            ("reason", w::PauseReason::LocalUser.write()),
            ("revision", (old.revision().unwrap() + 1).write()),
        ])
        .unwrap();
        config
            .publish_local_owner(facts.preferences().unwrap().unwrap().0, pause)
            .unwrap();
    }
    fn manifest(bundle: &BundleInfo) -> serde_json::Value {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
        serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap()
    }

    #[test]
    fn compressed_and_expanded_lengths_are_bounded_and_trailing_data_is_rejected() {
        let raw = vec![0; MAX_SEGMENT_RAW];
        let bytes = compress(&raw, DEFLATE).unwrap();
        assert_eq!(expand(&bytes, DEFLATE, raw.len()).unwrap(), raw);
        assert!(expand(&bytes, DEFLATE, raw.len() - 1).is_err());
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(&bytes);
        assert!(expand(&trailing, DEFLATE, raw.len()).is_err());
        let bomb = compress(&vec![0; MAX_SEGMENT_RAW + 1], DEFLATE).unwrap();
        assert!(expand(&bomb, DEFLATE, MAX_SEGMENT_RAW).is_err());
        assert!(expand(&[1, 2, 3], NONE, 2).is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn progress_refreshes_the_thirty_second_idle_deadline() {
        let begin = tokio::time::Instant::now();
        for _ in 0..3 {
            idle(TIMEOUT, async {
                tokio::time::sleep(Duration::from_secs(20)).await;
                Ok::<_, anyhow::Error>(())
            })
            .await
            .unwrap();
        }
        assert_eq!(begin.elapsed(), Duration::from_secs(60));
        let stalled = idle(TIMEOUT, std::future::pending::<anyhow::Result<()>>())
            .await
            .unwrap_err();
        assert_eq!(stalled.to_string(), "member:timeout");
        assert_eq!(begin.elapsed(), Duration::from_secs(90));
    }
    #[test]
    fn selected_range_is_honoured_locally_and_by_every_peer_at_one_cutoff() {
        crate::test_watchdog::run(
            module_path!(),
            "selected_range_is_honoured_locally_and_by_every_peer_at_one_cutoff",
            Some(2),
            selected_range_scenario(),
        );
    }
    async fn selected_range_scenario() {
        let mut fixture = Fixture::<DiagnosticsStreams>::start_members(3).await;
        let end = 10 * 24 * 60 * 60 * 1_000_000;
        let old = end - 7 * 24 * 60 * 60 * 1_000_000;
        let mut recorders = Vec::new();
        let mut contexts = Vec::new();
        let mut dirs = Vec::new();
        for index in 0..3 {
            let (links, config) = fixture.context(index);
            let dir = tempfile::tempdir().unwrap();
            let recorder = Recorder::start_inner(
                dir.path().into(),
                links.id(),
                Some(config.applied().group),
                true,
                true,
                clock::Clock::new(Arc::new(Frozen(end))),
                None,
                None,
                false,
            )
            .unwrap();
            recorder.wait_recovery();
            if index == 1 {
                // Model a peer on a different installed build. Gathering must
                // preserve its producer identity, never copy the exporter's.
                lock(&recorder.inner.store).snapshot.source_tree_sha256 = "2".repeat(40);
            }
            for (utc_us, count) in [(old, 1), (end - 60_000_000, 2), (end + 60_000_000, 3)] {
                lock(&recorder.inner.store)
                    .append(
                        vec![Row {
                            mono_us: count,
                            clock_id: 0,
                            utc_us,
                            event: Event::State {
                                kind: StateKind::RecorderTick,
                                active: true,
                                count,
                                value: 0.,
                            },
                        }],
                        0,
                    )
                    .unwrap();
            }
            // Current layout must be gathered even when the chosen timeline
            // range contains only earlier segments with no layout snapshot.
            let mut screen = crate::config_module::runtime_tests::display("PRIVATE-SCREEN");
            screen.pixel_width = 200 + index as u32;
            config.initialize_displays(vec![screen]).unwrap();
            assert!(
                crate::config_module::runtime_tests::until(|| config
                    .members()
                    .members
                    .iter()
                    .find(|m| m.device == links.id())
                    .and_then(|m| m.facts.as_ref())
                    .is_some_and(|f| f
                        .displays()
                        .is_ok_and(|d| d.0.first().is_some_and(|d| d.pixel_width() == Ok(200 + index as u32)))))
                .await
            );
            recorder.configuration(&config);
            contexts.push(context(links, config));
            recorders.push(recorder);
            dirs.push(dir);
        }
        for last in [None, Some(30), Some(1440)] {
            let mut snapshot = lock(&recorders[0].inner.store).export_snapshot().unwrap();
            snapshot.prepare_range(Reason::Manual, last).unwrap();
            let export = contexts[0].clone();
            let saving = tokio::task::spawn_blocking(move || export.export(snapshot, Reason::Manual, 0, 0));
            let mut tasks = Vec::new();
            for index in 1..3 {
                let operation = incoming(fixture.port(index)).await;
                let Message::DiagnosticsRequest(request) = &operation.first else {
                    panic!("not request")
                };
                assert_eq!(request.end_us().unwrap(), end);
                assert_eq!(
                    request.start_us().unwrap(),
                    last.map_or(i64::MIN, |m| end - i64::from(m) * 60_000_000)
                );
                let collector = Collector {
                    recorder: recorders[index].clone(),
                    context: contexts[index].clone(),
                };
                tasks.push(tokio::spawn(async move {
                    collector.serve(operation.stream, operation.first).await
                }));
            }
            for task in tasks {
                task.await.unwrap().unwrap();
            }
            let bundle = saving.await.unwrap().unwrap();
            assert_eq!(bundle.included_device_count, Some(3));
            assert_eq!(bundle.cutoff_complete, Some(true));
            let m = manifest(&bundle);
            // None of these peers invoked local export/snapshot_node. Even a
            // selected historical range must carry each recorder's build.
            let mut zip = zip::ZipArchive::new(std::fs::File::open(&bundle.path).unwrap()).unwrap();
            for (index, recorder) in recorders.iter().enumerate() {
                let device = recorder.anonymizer().device(fixture.context(index).0.id());
                let expected_sha = if index == 1 {
                    "2".repeat(40)
                } else {
                    env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256").into()
                };
                assert_eq!(m["versions"][&device]["source_tree_sha256"], expected_sha);
                assert_eq!(m["versions"][&device]["version"], env!("OINK_RELEASE_VERSION"));
                let alias = m["devices"][&device].as_str().unwrap();
                let environment: serde_json::Value =
                    serde_json::from_reader(zip.by_name(&format!("devices/{alias}/environment.json")).unwrap())
                        .unwrap();
                if index > 0 {
                    let mut bytes = Vec::new();
                    zip.by_name(&format!("devices/{alias}/current-environment.cbor"))
                        .unwrap()
                        .read_to_end(&mut bytes)
                        .unwrap();
                    let environment = w::Snapshot::decode(&bytes, MAX_SEGMENT_RAW).unwrap();
                    assert_eq!(environment.source_tree_sha256().unwrap().0, expected_sha);
                    assert_eq!(environment.os().unwrap().0, std::env::consts::OS);
                    let layout = environment.layout().unwrap().unwrap();
                    let own = layout
                        .devices()
                        .unwrap()
                        .0
                        .into_iter()
                        .find(|d| d.device().unwrap().0 == device)
                        .unwrap();
                    assert_eq!(own.displays().unwrap().0[0].pixel_width().unwrap(), 200 + index as u32);
                    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE"));
                    continue;
                }
                assert_eq!(environment["source_tree_sha256"], expected_sha);
                assert_eq!(environment["os"], std::env::consts::OS);
                let layout = environment["layout"].as_object().expect("per-device current layout");
                let own = layout["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|d| d["device"] == device)
                    .unwrap();
                assert_eq!(own["displays"][0]["pixel_width"], 200 + index);
                assert_eq!(own["displays"][0]["scale"], (200. + index as f64) / 800.);
                assert!(!serde_json::to_string(&environment).unwrap().contains("PRIVATE"));
            }
            assert_eq!(
                m["requested_range_us"],
                serde_json::json!([last.map(|m| end - i64::from(m) * 60_000_000), end])
            );
            let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
            assert_eq!(rows.len(), if last.is_none() { 6 } else { 3 });
            for device in m["devices"].as_object().unwrap().keys() {
                let own: Vec<_> = rows.iter().filter(|r| r["device"] == device.as_str()).collect();
                assert_eq!(own.len(), if last.is_none() { 2 } else { 1 });
                assert_eq!(m["evicted_segments_by_device"][device], 0);
                assert!(own.iter().all(|r| r["original_utc_us"].as_i64().unwrap() <= end));
                assert_eq!(own.iter().any(|r| r["original_utc_us"] == old), last.is_none());
            }
            assert_eq!(m["coverage"].as_array().unwrap().len(), rows.len());
        }
        for recorder in recorders {
            recorder.shutdown();
        }
        fixture.stop().await;
    }
    #[test]
    fn collection_bounds_allow_all_history_and_reject_future_or_reversed_ranges() {
        let stamp = clock::Stamp {
            mono_us: 0,
            utc_us: AUTOMATIC_RANGE_US,
            clock_id: 0,
        };
        for (start, end, valid) in [
            (i64::MIN, AUTOMATIC_RANGE_US, true),
            (-AUTOMATIC_RANGE_US * 100, AUTOMATIC_RANGE_US, true),
            (0, AUTOMATIC_RANGE_US + 1, false),
            (AUTOMATIC_RANGE_US + 1, AUTOMATIC_RANGE_US, false),
        ] {
            assert_eq!(validate_request(&request(stamp, start, end)).is_ok(), valid);
        }
    }
    #[test]
    fn clock_handshake_scopes_offsets_to_epochs_and_bounds_uncertainty() {
        let stamp = |utc_us, clock_id| clock::Stamp {
            mono_us: 0,
            utc_us,
            clock_id,
        };
        let sample = clock_sample(
            "a".repeat(24),
            "b".repeat(24),
            stamp(100, 2),
            stamp(1110, 3),
            stamp(1120, 3),
            stamp(130, 2),
        )
        .unwrap();
        assert_eq!(
            (
                sample.offset_us,
                sample.local_clock,
                sample.source_clock,
                sample.uncertainty_us
            ),
            (1000, 2, 3, 400010)
        );
        for (local, remote) in [(3, 3), (2, 4), (-1, 3)] {
            assert!(
                clock_sample(
                    "a".repeat(24),
                    "b".repeat(24),
                    stamp(100, 2),
                    stamp(1110, 3),
                    stamp(1120, remote),
                    stamp(130, local)
                )
                .is_none()
            );
        }
    }
    #[test]
    fn save_timeout_mid_segment_failure_and_unsupported_keep_arrivals_and_report_reasons() {
        crate::test_watchdog::run(
            module_path!(),
            "save_timeout_mid_segment_failure_and_unsupported_keep_arrivals_and_report_reasons",
            Some(2),
            save_timeout_mid_segment_failure_and_unsupported_keep_arrivals_and_report_reasons_scenario(),
        );
    }

    async fn save_timeout_mid_segment_failure_and_unsupported_keep_arrivals_and_report_reasons_scenario() {
        for mode in ["slow", "failed", "unsupported"] {
            let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
            let (links, group) = fixture.context(0);
            let (remote, remote_group) = fixture.context(1);
            let dir = tempfile::tempdir().unwrap();
            // The fake responder echoes t1 as t2/t3. Freeze the receiver's t4
            // too: a live clock would fabricate -RTT/2 skew and could clip the
            // completed row under load. This fixture owns its rows and does
            // not subscribe to other tests' process-wide trace traffic.
            let recorder = Recorder::start_inner(
                dir.path().into(),
                links.id(),
                Some(group.applied().group),
                true,
                true,
                clock::Clock::new(Arc::new(Frozen(10 * 24 * 60 * 60 * 1_000_000))),
                None,
                None,
                false,
            )
            .unwrap();
            recorder.wait_recovery();
            let snapshot = {
                let mut s = lock(&recorder.inner.store);
                recorder.drain(&mut s).unwrap();
                s.export_snapshot().unwrap()
            };
            let staging = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.file_name().unwrap().to_string_lossy().starts_with("export-"))
                .unwrap();
            let mut context = context(links, group);
            let (expire, expired) = tokio::sync::watch::channel(false);
            if mode == "slow" {
                context.deadline_gate = Some(expired);
            }
            let saving = tokio::task::spawn_blocking(move || context.export(snapshot, Reason::Manual, 0, 0));
            let mut operation = incoming(fixture.port(1)).await;
            // Expiration is caused only after the real request arrives. Scheduling
            // and paced transport cannot spend a synthetic 500 ms test budget.
            if mode == "slow" {
                expire.send(true).unwrap();
            }
            let serving = tokio::spawn(async move {
                let Message::DiagnosticsRequest(request) = operation.first else {
                    panic!()
                };
                if mode == "slow" {
                    std::future::pending::<()>().await;
                    return;
                }
                if mode == "unsupported" {
                    operation
                        .stream
                        .send(&Message::DiagnosticsUnavailable(
                            dto::<w::DiagnosticsUnavailable>([
                                ("reason", w::CollectionFailure::Unsupported.write()),
                                ("request_id", request.request_id().unwrap().write()),
                            ])
                            .unwrap(),
                        ))
                        .await
                        .unwrap();
                    operation.stream.finish();
                    // Keep the operation alive until the receiver consumes the
                    // refusal and closes. Dropping its connection immediately
                    // can race the final frame on Windows.
                    let _ = operation.stream.recv().await;
                    return;
                }
                let request_id = request.request_id().unwrap();
                let response = dto::<w::DiagnosticsResponse>([
                    ("instance", RawValue::Text("b".repeat(24))),
                    ("evicted_segments", 0u64.write()),
                    ("t2", request.t1().unwrap().write()),
                    ("t3", request.t1().unwrap().write()),
                    ("request_id", request_id.write()),
                    ("format", RawValue::from(FORMAT)),
                    ("compression", RawValue::from(NONE)),
                ])
                .unwrap();
                operation
                    .stream
                    .send(&Message::DiagnosticsResponse(response))
                    .await
                    .unwrap();
                operation
                    .stream
                    .send(&Message::DiagnosticsEnvironment(
                        dto::<w::DiagnosticsEnvironment>([
                            (
                                "snapshot",
                                wire_values::snapshot(&Snapshot::default(), true).unwrap().write(),
                            ),
                            ("request_id", request_id.write()),
                            ("format", RawValue::from(FORMAT)),
                        ])
                        .unwrap(),
                    ))
                    .await
                    .unwrap();
                let segment = store::Segment {
                    schema: SCHEMA,
                    device: Anonymizer::new(remote_group.applied().group).device(remote.id()),
                    instance: "b".repeat(24),
                    seq: 1,
                    dropped: 0,
                    snapshot: Snapshot::default(),
                    clock_samples: vec![],
                    rows: vec![Row {
                        mono_us: 0,
                        clock_id: 0,
                        utc_us: request.end_us().unwrap(),
                        event: Event::State {
                            kind: StateKind::RecorderTick,
                            active: true,
                            count: 1,
                            value: 0.,
                        },
                    }],
                };
                let committed = staging.join(format!("{}-{}-{}.cbor", segment.device, segment.instance, segment.seq));
                let bytes = wire_values::segment(&segment, true)
                    .unwrap()
                    .encode(MAX_SEGMENT_RAW)
                    .unwrap();
                operation
                    .stream
                    .send(&Message::DiagnosticsChunk(
                        dto::<w::DiagnosticsChunk>([
                            ("bytes", RawValue::Bytes(bytes.clone())),
                            ("request_id", request_id.write()),
                            ("segment_id", 0u64.write()),
                            ("offset", 0u32.write()),
                        ])
                        .unwrap(),
                    ))
                    .await
                    .unwrap();
                let digest = dto::<w::Digest>([
                    ("algorithm", RawValue::from("sha256")),
                    ("bytes", v1::retained::sha256(&bytes).write()),
                ])
                .unwrap();
                operation
                    .stream
                    .send(&Message::DiagnosticsSegmentEnd(
                        dto::<w::DiagnosticsSegmentEnd>([
                            ("request_id", request_id.write()),
                            ("segment_id", 0u64.write()),
                            ("compressed_length", (bytes.len() as u32).write()),
                            ("raw_length", (bytes.len() as u32).write()),
                            ("digest", digest.write()),
                        ])
                        .unwrap(),
                    ))
                    .await
                    .unwrap();
                operation
                    .stream
                    .send(&Message::DiagnosticsChunk(
                        dto::<w::DiagnosticsChunk>([
                            ("bytes", RawValue::Bytes(vec![1, 2, 3])),
                            ("request_id", request_id.write()),
                            ("segment_id", 1u64.write()),
                            ("offset", 0u32.write()),
                        ])
                        .unwrap(),
                    ))
                    .await
                    .unwrap();
                crate::test_watchdog::phase("first complete diagnostic segment staged before cancellation");
                while !committed.exists() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                // Now cancellation tests partial next-segment failure without
                // cancelling the previously unacknowledged complete segment.
            });
            let bundle = saving.await.unwrap().unwrap();
            let manifest = manifest(&bundle);
            let reason = manifest["missing_members"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap();
            assert_eq!(
                reason,
                match mode {
                    "slow" => "timeout",
                    "unsupported" => "unsupported",
                    _ => "transfer_failed_or_unsupported",
                }
            );
            assert_eq!(bundle.cutoff_complete, Some(false));
            assert_eq!(bundle.included_device_count, Some(if mode == "failed" { 2 } else { 1 }));
            serving.abort();
            let _ = serving.await;
            recorder.shutdown();
            fixture.stop().await;
        }
    }
    #[test]
    fn serving_while_suspended_filters_rows() {
        crate::test_watchdog::run(
            module_path!(),
            "serving_while_suspended_filters_rows",
            Some(2),
            serving_while_suspended_filters_rows_scenario(),
        );
    }

    async fn serving_while_suspended_filters_rows_scenario() {
        let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
        let (links, group) = fixture.context(0);
        let (remote, _) = fixture.context(1);
        let dir = tempfile::tempdir().unwrap();
        // This fixture owns exactly two historical segments. Start suspended
        // so the process-global subscriber cannot add another live segment.
        let recorder = Recorder::start_suspended(
            dir.path().into(),
            links.id(),
            Some(group.applied().group),
            true,
            true,
            None,
        )
        .unwrap();
        recorder.wait_recovery();
        let now = recorder.inner.clock.now();
        {
            let mut s = lock(&recorder.inner.store);
            for utc_us in [now.utc_us - 10_000_000, now.utc_us] {
                s.append(
                    vec![Row {
                        mono_us: 0,
                        clock_id: now.clock_id,
                        utc_us,
                        event: Event::State {
                            kind: StateKind::RecorderTick,
                            active: true,
                            count: 0,
                            value: 0.,
                        },
                    }],
                    0,
                )
                .unwrap();
            }
        }
        recorder.set_suspended(true);
        pause(&group);
        let collector = Collector {
            recorder: recorder.clone(),
            context: context(links.clone(), group),
        };
        {
            let mut request = remote
                .open_low(
                    remote.links().by_peer(links.id()).unwrap().link,
                    StreamKind::Diagnostics,
                    collection_options(),
                )
                .await
                .unwrap();
            let t1 = recorder.inner.clock.now();
            request
                .send(&Message::DiagnosticsRequest(tests::request(
                    t1,
                    now.utc_us - 1_000_000,
                    now.utc_us,
                )))
                .await
                .unwrap();
            let operation = incoming(fixture.port(0)).await;
            let service = collector.clone();
            let serving = tokio::spawn(async move { service.serve(operation.stream, operation.first).await });
            let mut payload = Vec::new();
            let mut segments = 0;
            let mut environments = 0;
            while let Some(message) = request.recv().await.unwrap() {
                match message {
                    Message::DiagnosticsChunk(chunk) => payload.extend(chunk.bytes().unwrap().0),
                    Message::DiagnosticsSegmentEnd(_) => {
                        let segment = store::wire::segment(&payload).unwrap();
                        assert!(
                            segment
                                .rows()
                                .unwrap()
                                .0
                                .iter()
                                .all(|r| r.utc_us().unwrap() > now.utc_us - 1_000_000)
                        );
                        payload.clear();
                        segments += 1;
                    }
                    Message::DiagnosticsEnd(_) => break,
                    Message::DiagnosticsResponse(_) => {}
                    Message::DiagnosticsEnvironment(environment) => {
                        let environment = environment.snapshot().unwrap();
                        assert_eq!(
                            environment.source_tree_sha256().unwrap().0,
                            env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256")
                        );
                        environments += 1;
                    }
                    _ => panic!("unexpected collection frame"),
                }
            }
            request.finish();
            assert!(request.recv().await.unwrap().is_none());
            serving.await.unwrap().unwrap();
            assert_eq!(segments, 1);
            assert_eq!(environments, 1);
        }
        recorder.shutdown();
        fixture.stop().await;
    }
    #[test]
    fn old_remote_copies_are_discarded_without_migration_or_corrupt_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        store.recover_for_test().unwrap();
        let segment = store::Segment {
            schema: SCHEMA,
            device: "b".repeat(24),
            instance: "c".repeat(24),
            seq: 1,
            dropped: 0,
            snapshot: Snapshot::default(),
            clock_samples: vec![],
            rows: vec![Row {
                mono_us: 0,
                clock_id: 0,
                utc_us: utc_us(),
                event: Event::State {
                    kind: StateKind::RecorderTick,
                    active: true,
                    count: 0,
                    value: 0.,
                },
            }],
        };
        // Emulate unreleased old on-disk replica directly, not a new production writer.
        std::fs::write(
            dir.path()
                .join("buffer")
                .join(format!("{}-{}-1.dseg", segment.device, segment.instance)),
            store::encode(&segment).unwrap(),
        )
        .unwrap();
        store.clean_shutdown().unwrap();
        drop(store);
        let mut store = store::Store::open(dir.path().into(), "a".repeat(24), true).unwrap();
        store.recover_for_test().unwrap();
        assert!(store.segments.is_empty());
        assert_eq!(store.corrupt, 0);
        let bundle = store.export(Reason::Manual, 0, 0).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name("merged.ndjson").unwrap().read_to_string(&mut text).unwrap();
        assert!(text.is_empty());
    }
}
