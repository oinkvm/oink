//! Native-cadenced receiver/decoder/mixer. Session routing, media admission,
//! logs and device lifetime stay on the owning ordinary thread.
use crate::{
    dsp, handoff,
    playback::{FadeOut, Playback, PlaybackInfo, PlaybackMetrics, PlaybackOptions, PullBlock, PullInput},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

mod dropouts;
pub(crate) mod quality;
pub(crate) use dropouts::Ledger as DropoutLedger;
pub use dropouts::{DropoutBucket, DropoutCounters, QUALITY_SECONDS};

const MAX_SOURCES: usize = crate::playback::MIX_SOURCES;
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);
const MIX_SAMPLES: usize = 240;
const INPUT_BATCH: usize = 8;
#[cfg(test)]
use std::thread;
#[derive(Clone, Copy, Debug)]
pub struct Packet {
    pub burst: u64,
    pub sequence: u64,
    pub payload: [u8; dsp::MAX_PACKET_BYTES],
    pub len: usize,
    pub arrived: Instant,
    pub network_read: Option<Instant>,
    pub source_emit: Option<u64>,
}
type PacketReader = Arc<dyn Fn(&mut usize) -> (bool, Option<Packet>) + Send + Sync>;
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    pub sequence: u64,
    pub arrived: Instant,
    pub network_read: Option<Instant>,
    pub inserted: Instant,
    pub source_emit: Option<u64>,
    pub accepted: bool,
    pub late: Option<dsp::LatePacket>,
}
#[derive(Clone, Copy)]
struct Snapshot {
    stats: dsp::Stats,
    rejections: dsp::Rejections,
}
/// Optional diagnostic copy of decoded source samples before mixing.
#[derive(Clone, Copy, Debug)]
pub struct Rendered {
    pub samples: [f32; MIX_SAMPLES],
    pub len: usize,
    pub at: Instant,
}
struct Status {
    reader: std::sync::OnceLock<PacketReader>,
    sleeping: std::sync::OnceLock<Arc<dyn Fn(Instant) -> bool + Send + Sync>>,
    playing: AtomicBool,
    active: AtomicBool,
    // Packet handoff plus existing receiver occupancy share this exact budget.
    // There is no second jitter reserve in front of Receiver.
    occupied: AtomicUsize,
    accepted: AtomicU64,
    dropout_counters: Arc<std::sync::OnceLock<Arc<DropoutCounters>>>,
    highest_accepted: AtomicU64,
    accepted_at_ns: AtomicU64,
    epoch: Instant,
    feedback_dropped: AtomicU64,
    rendered_dropped: AtomicU64,
}
/// Ordinary-thread endpoint; the underlying DSP exists only on the native callback.
pub struct Source {
    tx: handoff::Producer<Packet>,
    outcomes: handoff::Consumer<Option<Outcome>>,
    snapshots: handoff::Consumer<Snapshot>,
    status: Arc<Status>,
    config: dsp::Config,
    budget: usize,
    stats: dsp::Stats,
    rejections: dsp::Rejections,
    observed_accepted: u64,
    local_rejected: u64,
    rendered: Option<handoff::Consumer<Option<Rendered>>>,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeSource")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
impl Source {
    /// Read M1 directly at the native callback's native cadence. False means paused:
    /// queued packets are consumed without playback and the receiver is retained.
    pub fn set_packet_reader(&self, reader: PacketReader) {
        let _ = self.status.reader.set(reader);
    }
    /// Observe validated stream sleep on the callback, preserving the finite tail.
    pub fn set_sleep_detector(&self, sleeping: Arc<dyn Fn(Instant) -> bool + Send + Sync>) {
        let _ = self.status.sleeping.set(sleeping);
    }
    pub fn rendered(&mut self) -> Option<Rendered> {
        self.rendered.as_mut()?.pop().flatten()
    }
    pub fn rendered_dropped(&self) -> u64 {
        self.status.rendered_dropped.load(Ordering::Relaxed)
    }

    pub fn push(
        &mut self,
        sequence: u64,
        payload: &[u8],
        arrived: Instant,
        source_emit: Option<u64>,
        network_read: Option<Instant>,
    ) -> bool {
        if !self.status.active.load(Ordering::Acquire) {
            return false;
        }
        if payload.is_empty() || payload.len() > dsp::MAX_PACKET_BYTES {
            self.local_rejected += 1;
            self.rejections.invalid_payload += 1;
            return false;
        }
        let frames = self.config.frame_size();
        if self.status.occupied.load(Ordering::Acquire).saturating_add(frames) > self.budget {
            self.local_rejected += 1;
            self.rejections.queue_capacity += 1;
            return false;
        }
        let mut packet = Packet {
            burst: 1,
            sequence,
            payload: [0; dsp::MAX_PACKET_BYTES],
            len: payload.len(),
            arrived,
            network_read,
            source_emit,
        };
        packet.payload[..payload.len()].copy_from_slice(payload);
        self.status.occupied.fetch_add(frames, Ordering::Release);
        if self.tx.push(packet).is_err() {
            self.status.occupied.fetch_sub(frames, Ordering::AcqRel);
            self.local_rejected += 1;
            self.rejections.queue_capacity += 1;
            return false;
        }
        true
    }
    /// Ordinary-owner handover of packets saved during device start-up.
    pub fn push_packet(&mut self, packet: Packet) -> bool {
        if packet.burst == 0
            || packet.len > dsp::MAX_PACKET_BYTES
            || !dsp::valid_payload(self.config, &packet.payload[..packet.len])
        {
            return false;
        }
        let frames = self.config.frame_size();
        if self.status.occupied.load(Ordering::Acquire).saturating_add(frames) > self.budget {
            return false;
        }
        self.status.occupied.fetch_add(frames, Ordering::Release);
        if self.tx.push(packet).is_err() {
            self.status.occupied.fetch_sub(frames, Ordering::AcqRel);
            return false;
        }
        true
    }
    pub fn poll(&mut self) {
        while let Some(snapshot) = self.snapshots.pop() {
            self.stats = snapshot.stats;
            self.rejections.invalid_payload += snapshot.rejections.invalid_payload;
            self.rejections.queue_capacity += snapshot.rejections.queue_capacity;
            self.rejections.sequence_window += snapshot.rejections.sequence_window;
        }
    }
    pub fn outcome(&mut self) -> Option<Outcome> {
        self.outcomes.pop().flatten()
    }
    pub fn accepted(&mut self) -> Option<(u64, Instant)> {
        let count = self.status.accepted.load(Ordering::Acquire);
        if count == self.observed_accepted {
            return None;
        }
        self.observed_accepted = count;
        let at = self.status.epoch + Duration::from_nanos(self.status.accepted_at_ns.load(Ordering::Acquire));
        Some((self.status.highest_accepted.load(Ordering::Acquire), at))
    }
    /// Optional local diagnostics counter, retained across port/burst resets by the owner.
    pub fn set_dropout_counters(&self, counter: Arc<DropoutCounters>) {
        let _ = self.status.dropout_counters.set(counter);
    }
    pub fn stats(&self) -> dsp::Stats {
        dsp::Stats {
            rejected: self.stats.rejected + self.local_rejected,
            ..self.stats
        }
    }
    pub fn take_rejections(&mut self) -> dsp::Rejections {
        std::mem::take(&mut self.rejections)
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.status.active.store(false, Ordering::Release);
    }
}
struct SourceState {
    id: u64,
    splice: quality::SpliceRamp,
    fade: FadeOut,
    packets: handoff::Consumer<Packet>,
    outcomes: handoff::Producer<Option<Outcome>>,
    snapshots: handoff::Producer<Snapshot>,
    status: Arc<Status>,
    receiver: dsp::Receiver,
    rendered: Option<handoff::Producer<Option<Rendered>>>,
    codec_budget: usize,
    retire_pending: bool,
    rejections: dsp::Rejections,
    burst: u64,
    quality: quality::Interruptions,
}
impl SourceState {
    fn prepare(config: dsp::Config, observe: bool) -> Result<(Source, Box<Self>), String> {
        let mut receiver = dsp::Receiver::new_with_config(config)?;
        let capacity = receiver.packet_capacity();
        let status = Arc::new(Status {
            reader: Default::default(),
            sleeping: Default::default(),
            playing: AtomicBool::new(true),
            active: AtomicBool::new(true),
            occupied: AtomicUsize::new(0),
            accepted: AtomicU64::new(0),
            dropout_counters: Default::default(),
            highest_accepted: AtomicU64::new(0),
            accepted_at_ns: AtomicU64::new(0),
            epoch: Instant::now(),
            feedback_dropped: AtomicU64::new(0),
            rendered_dropped: AtomicU64::new(0),
        });
        receiver.set_dropout_store(status.dropout_counters.clone());
        let (tx, packets) = handoff::channel(
            capacity,
            Packet {
                burst: 1,
                sequence: 0,
                payload: [0; dsp::MAX_PACKET_BYTES],
                len: 0,
                arrived: Instant::now(),
                network_read: None,
                source_emit: None,
            },
        );
        let (outcomes, feedback) = handoff::channel(capacity * 2, None);
        let stats = receiver.stats();
        let (snapshots, observation) = handoff::channel(
            8,
            Snapshot {
                stats,
                rejections: dsp::Rejections::default(),
            },
        );
        let (rendered, rendered_rx) = if observe {
            let (tx, rx) = handoff::channel(32, None);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let state = Box::new(SourceState {
            id: NEXT_SOURCE.fetch_add(1, Ordering::Relaxed),
            splice: quality::SpliceRamp::default(),
            fade: FadeOut::default(),
            rendered,
            packets,
            outcomes,
            snapshots,
            status: status.clone(),
            receiver,
            codec_budget: usize::MAX,
            retire_pending: false,
            rejections: dsp::Rejections::default(),
            burst: 0,
            quality: Default::default(),
        });
        let source = Source {
            rendered: rendered_rx,
            tx,
            outcomes: feedback,
            snapshots: observation,
            status,
            config,
            budget: capacity * config.frame_size(),
            stats,
            rejections: dsp::Rejections::default(),
            observed_accepted: 0,
            local_rejected: 0,
        };
        Ok((source, state))
    }

    #[cfg(test)]
    fn receive(&mut self, now: Instant) {
        self.receive_bounded(now, INPUT_BATCH, true);
    }
    fn receive_bounded(&mut self, now: Instant, mut budget: usize, external: bool) -> usize {
        let initial_budget = budget;
        while budget != 0 {
            let packet = if let Some(packet) = self.packets.pop() {
                budget -= 1;
                Some(packet)
            } else if let Some(read) = self.status.reader.get().filter(|_| external) {
                let before = budget;
                let (playing, packet) = read(&mut budget);
                if budget == before {
                    budget -= 1;
                }
                self.status.playing.store(playing, Ordering::Release);
                if !playing {
                    self.quality.reset();
                    continue;
                }
                if packet.is_some() {
                    self.status
                        .occupied
                        .fetch_add(self.receiver.config().frame_size(), Ordering::Release);
                }
                packet
            } else {
                self.packets.pop()
            };
            let Some(packet) = packet else {
                break;
            };
            if packet.burst < self.burst {
                self.status
                    .occupied
                    .fetch_sub(self.receiver.config().frame_size(), Ordering::AcqRel);
                continue;
            }
            if packet.burst != self.burst {
                let buffered = self.receiver.buffered_frames();
                self.receiver.reset_burst();
                self.quality.reset();
                self.status.occupied.fetch_sub(buffered, Ordering::AcqRel);
                self.burst = packet.burst;
            }
            let buffered_before = self.receiver.buffered_frames();
            let accepted = self.receiver.push_slice_at(
                packet.sequence,
                &packet.payload[..packet.len],
                packet.arrived,
                packet.source_emit,
            );
            // Admission may replace a stranded refill window as well as reject
            // this packet. Retire exactly the frames no longer owned by DSP;
            // otherwise the handoff capacity and idle drain retain phantom PCM.
            let retired =
                (buffered_before + self.receiver.config().frame_size()).saturating_sub(self.receiver.buffered_frames());
            self.status.occupied.fetch_sub(retired, Ordering::AcqRel);
            if accepted {
                let highest = self
                    .status
                    .highest_accepted
                    .load(Ordering::Relaxed)
                    .max(packet.sequence);
                self.status.highest_accepted.store(highest, Ordering::Release);
                self.status.accepted_at_ns.store(
                    now.saturating_duration_since(self.status.epoch)
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64,
                    Ordering::Release,
                );
                self.status.accepted.fetch_add(1, Ordering::Release);
            }
            let outcome = Outcome {
                sequence: packet.sequence,
                arrived: packet.arrived,
                network_read: packet.network_read,
                inserted: now,
                source_emit: packet.source_emit,
                accepted,
                late: self.receiver.take_late_packet(),
            };
            if self.outcomes.push(Some(outcome)).is_err() {
                self.status.feedback_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        initial_budget - budget
    }
    fn render(&mut self, now: Instant, demand: usize, out: &mut [f32]) {
        let before = self.receiver.buffered_frames();
        self.receiver.pull_into(now, demand, out, &mut self.codec_budget, None);
        let after = self.receiver.buffered_frames();
        if after >= before {
            self.status.occupied.fetch_add(after - before, Ordering::Release);
        } else {
            self.status.occupied.fetch_sub(before - after, Ordering::AcqRel);
        }
        if let Some(counters) = self.status.dropout_counters.get() {
            self.quality.block(
                (crate::diagnostics::monotonic_us(now).max(0) / 1000) as u64,
                self.receiver.output_kinds(),
                out,
                counters,
            );
        }
        for (frame, kind) in out.as_chunks_mut::<2>().0.iter_mut().zip(self.receiver.output_kinds()) {
            self.splice.apply(frame, *kind, dsp::SAMPLE_RATE);
            self.fade.apply(frame, *kind != quality::OutputKind::Intentional);
        }
        if let Some(rendered) = &mut self.rendered {
            let mut samples = [0.0; MIX_SAMPLES];
            samples[..out.len()].copy_from_slice(out);
            if rendered
                .push(Some(Rendered {
                    samples,
                    len: out.len(),
                    at: now,
                }))
                .is_err()
            {
                self.status.rendered_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    fn snapshot(&mut self) {
        let recent = self.receiver.take_rejections();
        self.rejections.invalid_payload += recent.invalid_payload;
        self.rejections.queue_capacity += recent.queue_capacity;
        self.rejections.sequence_window += recent.sequence_window;
        let snapshot = Snapshot {
            stats: self.receiver.stats(),
            rejections: self.rejections,
        };
        if self.snapshots.push(snapshot).is_ok() {
            self.rejections = dsp::Rejections::default();
        }
    }
}
#[cfg(test)]
trait Output {
    fn gap(&mut self) -> Option<crate::playback::OutputGap> {
        None
    }
    fn push_mix(&mut self, samples: &[f32], sources: [u64; MAX_SOURCES]) -> bool {
        let _ = sources;
        self.push(samples, None)
    }
    fn needs_frame(&self) -> bool;
    fn frames_needed(&self) -> usize;
    fn push(&mut self, samples: &[f32], sequence: Option<u64>) -> bool;
}
struct Registry {
    slots: [AtomicPtr<SourceState>; MAX_SOURCES],
    stop: AtomicBool,
    draining: AtomicBool,
    drained: AtomicBool,
    error: AtomicI32,
}
// Sources are published only by control. The callback is the sole DSP owner.
// Null release is its final access; control reclaims only after acquire or a
// native shutdown which proves callbacks can no longer run.
struct Frozen {
    sources: [*mut SourceState; MAX_SOURCES],
    playing: [bool; MAX_SOURCES],
    draining: bool,
    work: crate::playback::CallbackWork,
}
// SAFETY: transferred into exactly one serialized native callback; pointers
// are dereferenced only there and storage stays pinned until retirement ack.
unsafe impl Send for Frozen {}
impl Default for Frozen {
    fn default() -> Self {
        Self {
            sources: [std::ptr::null_mut(); MAX_SOURCES],
            playing: [false; MAX_SOURCES],
            draining: false,
            work: Default::default(),
        }
    }
}
struct PullRender {
    registry: Arc<Registry>,
    frozen: Frozen,
}
impl PullRender {
    fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            frozen: Default::default(),
        }
    }
}
impl PullInput for PullRender {
    fn begin(&mut self, frames: usize, now: Instant) {
        self.registry.begin(frames, now, &mut self.frozen);
    }
    fn pull(&mut self, output: &mut [f32], demand: usize, now: Instant) -> PullBlock {
        self.registry.pull(output, demand, now, &mut self.frozen)
    }
    fn end(&mut self) {
        self.registry.end(&mut self.frozen);
    }
    fn gap(&mut self, gap: crate::playback::OutputGap) {
        self.registry.gap(gap);
    }
    fn drained(&self) -> bool {
        self.registry.drained.load(Ordering::Acquire)
    }
    fn work(&self) -> crate::playback::CallbackWork {
        self.frozen.work
    }
}
impl Registry {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            stop: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            drained: AtomicBool::new(false),
            error: AtomicI32::new(0),
        }
    }
    fn gap(&self, gap: crate::playback::OutputGap) {
        // Called before begin can acknowledge last callback's retired sources.
        for slot in &self.slots {
            let ptr = slot.load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            // SAFETY: same sole callback executor as begin/pull/end.
            let state = unsafe { &*ptr };
            if gap.sources.contains(&state.id)
                && let Some(counters) = state.status.dropout_counters.get()
            {
                counters.record_quality(DropoutBucket {
                    output_interruptions: gap.delta.interruptions,
                    ..gap.delta
                });
            }
        }
    }
    fn begin(&self, frames: usize, now: Instant, frozen: &mut Frozen) {
        *frozen = Frozen::default();
        frozen.draining = self.draining.load(Ordering::Acquire);
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        for (index, slot) in self.slots.iter().enumerate() {
            let ptr = slot.load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            // SAFETY: sole RT owner; control retains until this slot is cleared.
            let state = unsafe { &mut *ptr };
            let active = state.status.active.load(Ordering::Acquire);
            if state.retire_pending || (!active && !state.status.playing.load(Ordering::Acquire)) {
                // No access after release, including gap attribution. The prior
                // callback's gap has already been delivered above.
                slot.store(std::ptr::null_mut(), Ordering::Release);
                continue;
            }
            if !active {
                state.fade.start(dsp::SAMPLE_RATE);
            }
            let codec = state.receiver.config().frame_size();
            let scans = INPUT_BATCH.max(frames.div_ceil(codec) + 2).min(32);
            frozen.work.scan_steps += state.receive_bounded(now, scans, active && !frozen.draining) as u64;
            // A sleeping source has a finite tail even while another source
            // keeps the device active. Drain that tail without inventing PLC.
            let sleeping = state.status.sleeping.get().is_some_and(|sleeping| sleeping(now));
            state.receiver.set_draining(frozen.draining || !active || sleeping);
            // Invalid packet decode may also need PLC: bound both C calls.
            state.codec_budget = 2 * ((frames * 1011).div_ceil(codec * 1000) + 1);
            frozen.playing[index] = state.status.playing.load(Ordering::Acquire) || !active;
            frozen.work.sources += u64::from(frozen.playing[index]);
            frozen.sources[index] = ptr;
        }
    }
    fn pull(&self, output: &mut [f32], demand: usize, now: Instant, frozen: &mut Frozen) -> PullBlock {
        let mut inputs = [[0.0; MIX_SAMPLES]; MAX_SOURCES];
        let mut sources = [0; MAX_SOURCES];
        let mut present = 0;
        let mut fault = false;
        let mut rendering = false;
        for (index, ptr) in frozen.sources.iter().copied().enumerate() {
            if ptr.is_null() || !frozen.playing[index] {
                continue;
            }
            // SAFETY: begin froze this live slot for the whole transaction.
            let state = unsafe { &mut *ptr };
            if frozen.draining && state.status.occupied.load(Ordering::Acquire) == 0 && !state.fade.fading() {
                continue;
            }
            let before = state.receiver.stats();
            let budget = state.codec_budget;
            state.render(now, demand, &mut inputs[present][..output.len()]);
            let after = state.receiver.stats();
            frozen.work.codec_calls += budget.saturating_sub(state.codec_budget) as u64;
            frozen.work.plc_frames += after.lost.saturating_sub(before.lost);
            frozen.work.decode_errors += after.decode_errors.saturating_sub(before.decode_errors);
            frozen.work.work_limited += after.work_limited.saturating_sub(before.work_limited);
            fault |= after.work_limited != before.work_limited;
            let source_rendering = state.fade.fading()
                || state
                    .receiver
                    .output_kinds()
                    .iter()
                    .any(|kind| *kind != quality::OutputKind::Intentional);
            rendering |= source_rendering;
            // Output-wide native gaps can only affect sources contributing to
            // this timeline. An intentionally idle port must not inherit them.
            if source_rendering {
                sources[present] = state.id;
            }
            present += 1;
        }
        let refs: [&[f32]; MAX_SOURCES] = std::array::from_fn(|i| &inputs[i][..output.len()]);
        dsp::mix(&refs[..present], output);
        if fault {
            self.error.store(1, Ordering::Release);
        }
        PullBlock {
            sources,
            kind: if fault {
                quality::OutputKind::Missing
            } else if !rendering {
                quality::OutputKind::Intentional
            } else {
                quality::OutputKind::Audio
            },
        }
    }
    fn end(&self, frozen: &mut Frozen) {
        let mut pending = false;
        for ptr in frozen.sources.iter().copied().filter(|ptr| !ptr.is_null()) {
            // SAFETY: no slot is acknowledged until a subsequent begin.
            let state = unsafe { &mut *ptr };
            state.snapshot();
            if !state.status.active.load(Ordering::Acquire) && state.fade.finished() {
                state.retire_pending = true;
            }
            pending |= state.status.occupied.load(Ordering::Acquire) != 0
                || (!state.status.active.load(Ordering::Acquire) && !state.fade.finished());
        }
        self.drained.store(frozen.draining && !pending, Ordering::Release);
    }
    // Test-only request collector, using exactly the callback begin/pull/end
    // core. This owns no PCM queue and has no producer/worker scheduling path.
    #[cfg(test)]
    fn work(&self, output: &mut impl Output, now: Instant) {
        for _ in 0..256 {
            let Some(gap) = output.gap() else { break };
            self.gap(gap);
        }
        let mut frozen = Frozen::default();
        self.begin(output.frames_needed(), now, &mut frozen);
        for _ in 0..137 {
            if !output.needs_frame() {
                break;
            }
            let frames = output.frames_needed().min(120);
            if frames == 0 {
                break;
            }
            let mut mixed = [0.0; MIX_SAMPLES];
            let block = self.pull(&mut mixed[..frames * 2], output.frames_needed(), now, &mut frozen);
            let _ = output.push_mix(&mixed[..frames * 2], block.sources);
        }
        self.end(&mut frozen);
    }
}

pub struct Sink {
    playback: Playback,
    registry: Arc<Registry>,
    // Boxing is intentional: published addresses must not move with this Vec.
    states: Vec<(usize, Box<SourceState>)>,
}
impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeSink")
            .field("playback", &self.playback)
            .finish_non_exhaustive()
    }
}
impl Sink {
    pub fn start(id: &str) -> Result<Self, String> {
        Self::start_with_options(id, &PlaybackOptions::default())
    }
    pub fn start_with_options(id: &str, options: &PlaybackOptions) -> Result<Self, String> {
        let registry = Arc::new(Registry::new());
        let playback = Playback::start_with_options(id, options, Box::new(PullRender::new(registry.clone())))?;
        Ok(Self {
            playback,
            registry,
            states: Vec::with_capacity(MAX_SOURCES),
        })
    }
    /// Test-only native probe: runs the full pull core but masks every native
    /// output buffer to zero before it reaches the device. Requires exclusive use of the machine.
    #[cfg(feature = "test-support")]
    pub fn start_silent_test(id: &str, options: &PlaybackOptions) -> Result<Self, String> {
        let registry = Arc::new(Registry::new());
        let playback = Playback::start_silent_test(id, options, Box::new(PullRender::new(registry.clone())))?;
        Ok(Self {
            playback,
            registry,
            states: Vec::with_capacity(MAX_SOURCES),
        })
    }
    fn reap(&mut self) {
        self.states
            .retain(|(slot, _)| !self.registry.slots[*slot].load(Ordering::Acquire).is_null());
    }
    pub fn source(&mut self, config: dsp::Config) -> Result<Source, String> {
        self.source_inner(config, false)
    }
    /// Enables bounded diagnostic sample copies; never increases playout reserve.
    pub fn source_observed(&mut self, config: dsp::Config) -> Result<Source, String> {
        self.source_inner(config, true)
    }
    fn source_inner(&mut self, config: dsp::Config, observe: bool) -> Result<Source, String> {
        self.reap();
        let index = self
            .registry
            .slots
            .iter()
            .position(|slot| slot.load(Ordering::Acquire).is_null())
            .ok_or("audio source slots awaiting retirement")?;
        let (source, mut state) = SourceState::prepare(config, observe)?;
        let ptr = &mut *state as *mut SourceState;
        self.states.push((index, state));
        self.registry.slots[index].store(ptr, Ordering::Release);
        Ok(source)
    }

    fn stop(&mut self) {
        self.registry.draining.store(true, Ordering::Release);
        for (_, state) in &self.states {
            state.status.active.store(false, Ordering::Release);
        }
        let quiescent = self.playback.shutdown();
        if quiescent {
            self.registry.stop.store(true, Ordering::Release);
            for slot in &self.registry.slots {
                slot.store(std::ptr::null_mut(), Ordering::Release);
            }
            self.states.clear();
        } else {
            // HAL retained its callback context: preserve every pointer that
            // context could still read. Never trade a leak for use-after-free.
            for (_, state) in self.states.drain(..) {
                let _ = Box::leak(state);
            }
        }
    }
    pub fn error(&self) -> Option<String> {
        if self.registry.error.load(Ordering::Acquire) != 0 {
            Some("audio pull exhausted per-source codec budget".into())
        } else {
            self.playback.error()
        }
    }
    pub fn set_output_quality_counters(&self, counters: Arc<DropoutCounters>) {
        self.playback.set_quality_counters(counters);
    }
    pub fn begin_idle_drain(&self) {
        self.registry.drained.store(false, Ordering::Release);
        self.registry.draining.store(true, Ordering::Release);
        self.playback.begin_idle_drain();
    }
    pub fn cancel_idle_drain(&self) {
        self.playback.cancel_idle_drain();
        self.registry.draining.store(false, Ordering::Release);
        self.registry.drained.store(false, Ordering::Release);
    }
    pub fn idle_drained(&self) -> bool {
        self.registry.drained.load(Ordering::Acquire)
            && self.playback.queued_frames() == 0
            && self.playback.idle_drained()
    }
    pub fn device_invalidated(&self) -> bool {
        self.playback.device_invalidated()
    }
    pub fn info(&self) -> PlaybackInfo {
        self.playback.info()
    }
    pub fn take_metrics(&self) -> PlaybackMetrics {
        self.playback.take_metrics()
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dormant_source_drains_without_plc_or_quality_events_while_another_source_plays() {
        let config = dsp::Config::low_latency_pcm();
        let now = Instant::now();
        let registry = Registry::new();
        let (mut dormant, mut quiet_state) = SourceState::prepare(config, false).unwrap();
        let (mut active, mut active_state) = SourceState::prepare(config, false).unwrap();
        registry.slots[0].store(&mut *quiet_state, Ordering::Release);
        registry.slots[1].store(&mut *active_state, Ordering::Release);
        let sleep = Arc::new(AtomicBool::new(false));
        let observed = sleep.clone();
        dormant.set_sleep_detector(Arc::new(move |_| observed.load(Ordering::Acquire)));
        let counters = Arc::new(DropoutCounters::default());
        dormant.set_dropout_counters(counters.clone());
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.2; 240]).unwrap();
        for seq in 0..8 {
            assert!(dormant.push(seq, &payload, now, None, None));
            assert!(active.push(seq, &payload, now, None, None));
        }
        let mut output = [0.; MIX_SAMPLES];
        let mut frozen = Frozen::default();
        let render = |tick: u64, frozen: &mut Frozen, output: &mut [f32]| {
            let at = now + Duration::from_micros(50_000 + tick * 2500);
            registry.begin(120, at, frozen);
            let block = registry.pull(output, 120, at, frozen);
            registry.end(frozen);
            block
        };
        render(0, &mut frozen, &mut output);
        assert!(output.iter().any(|v| *v > 0.1));
        sleep.store(true, Ordering::Release);
        for tick in 1..80 {
            assert!(active.push(7 + tick, &payload, now, None, None));
            let mut block = PullBlock {
                sources: [0; MAX_SOURCES],
                kind: quality::OutputKind::Intentional,
            };
            crate::allocation_audit::assert_no_heap(|| block = render(tick, &mut frozen, &mut output));
            assert!(output.iter().any(|v| *v > 0.1), "other source stopped");
            if tick > 12 {
                assert!(
                    !block.sources.contains(&quiet_state.id),
                    "dormant source blamed for native gaps"
                );
            }
        }
        assert_eq!(quiet_state.receiver.stats().lost, 0, "Sleep manufactured PLC");
        let at = (crate::diagnostics::monotonic_us(now).max(0) / 1000) as u64 + 1000;
        let buckets: Vec<_> = counters.snapshot(at).into_iter().flatten().collect();
        assert_eq!(buckets.iter().map(|b| b.interruptions).sum::<u64>(), 0);
        assert_eq!(buckets.iter().map(|b| b.concealed_ns).sum::<u64>(), 0);
        assert_eq!(buckets.iter().map(|b| b.missing_ns).sum::<u64>(), 0);
        // A new media burst resumes on the same port.
        sleep.store(false, Ordering::Release);
        assert!(dormant.push_packet(Packet {
            burst: 2,
            sequence: 1,
            payload: {
                let mut bytes = [0; dsp::MAX_PACKET_BYTES];
                bytes[..payload.len()].copy_from_slice(&payload);
                bytes
            },
            len: payload.len(),
            arrived: now,
            network_read: None,
            source_emit: None
        }));
        render(80, &mut frozen, &mut output);
        assert_eq!(quiet_state.burst, 2);
    }

    #[test]
    fn callback_work_counts_bad_decode_plc_and_resets_for_each_pull_transaction() {
        let config = dsp::Config::low_latency_opus();
        let now = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let registry = Registry::new();
        registry.slots[0].store(&mut *state, Ordering::Release);
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.2; 480]).unwrap();
        // Valid five-ms TOC, invalid compressed packet: one failed decode plus
        // one PLC, distinct from an exhausted per-callback work budget.
        assert!(source.push(0, &[0x8b, 0x41], now, None, None));
        for seq in 1..4 {
            assert!(source.push(seq, &payload, now, None, None));
        }
        let mut frozen = Frozen::default();
        let mut output = [0.0; MIX_SAMPLES];
        let at = now + Duration::from_millis(20);
        crate::allocation_audit::assert_no_heap(|| {
            registry.begin(128, at, &mut frozen);
            registry.pull(&mut output, 128, at, &mut frozen);
            registry.pull(&mut output[..16], 8, at, &mut frozen);
            registry.end(&mut frozen);
        });
        assert_eq!(frozen.work.sources, 1);
        assert_eq!(frozen.work.scan_steps, 4);
        assert_eq!(frozen.work.codec_calls, 2);
        assert_eq!(frozen.work.plc_frames, 1);
        assert_eq!(frozen.work.decode_errors, 1);
        assert_eq!(frozen.work.work_limited, 0);
        registry.begin(48, at + Duration::from_millis(3), &mut frozen);
        registry.pull(&mut output[..96], 48, at, &mut frozen);
        registry.end(&mut frozen);
        assert_eq!(frozen.work.scan_steps, 0);
        assert_eq!(frozen.work.codec_calls, 0, "remaining decoded PCM needs no codec call");
        assert_eq!(frozen.work.plc_frames, 0);
        assert_eq!(frozen.work.decode_errors, 0);
    }

    #[test]
    fn output_gap_is_attributed_to_affected_streams_without_reusing_retired_ids() {
        struct Feedback(Option<crate::playback::OutputGap>);
        impl Output for Feedback {
            fn gap(&mut self) -> Option<crate::playback::OutputGap> {
                self.0.take()
            }
            fn needs_frame(&self) -> bool {
                false
            }
            fn frames_needed(&self) -> usize {
                0
            }
            fn push(&mut self, _: &[f32], _: Option<u64>) -> bool {
                false
            }
        }
        let (source, mut state) = SourceState::prepare(dsp::Config::low_latency_pcm(), false).unwrap();
        let counters = Arc::new(DropoutCounters::default());
        source.set_dropout_counters(counters.clone());
        let registry = Registry::new();
        registry.slots[0].store(&mut *state, Ordering::Release);
        let mut ids = [0; MAX_SOURCES];
        ids[0] = state.id;
        let gap = crate::playback::OutputGap {
            sources: ids,
            delta: DropoutBucket {
                at_ms: 1000,
                interruptions: 1,
                discontinuities: 1,
                ..Default::default()
            },
        };
        registry.work(&mut Feedback(Some(gap)), Instant::now());
        assert_eq!(counters.snapshot(1000)[1].unwrap().output_interruptions, 1);
        assert_eq!(counters.snapshot(1000)[1].unwrap().interruptions, 1);
        state.id = NEXT_SOURCE.fetch_add(1, Ordering::Relaxed);
        registry.work(&mut Feedback(Some(gap)), Instant::now());
        assert_eq!(counters.snapshot(1000)[1].unwrap().interruptions, 1);
    }

    #[test]
    fn packet_gap_recovery_has_no_pcm_step_or_added_frames() {
        let config = dsp::Config::low_latency_pcm();
        let origin = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        for seq in [0, 1, 3, 4, 5, 6, 7] {
            let value = if seq < 2 { 0.5 } else { -0.5 };
            let payload = encoder.encode(&vec![value; config.frame_samples()]).unwrap();
            assert!(source.push(seq, &payload, origin, None, None));
        }
        state.receive(origin);
        let mut samples = Vec::new();
        for tick in 0..7 {
            let mut output = [0.0; MIX_SAMPLES];
            state.render(origin + Duration::from_millis(100 + tick * 3), 120, &mut output);
            samples.extend_from_slice(&output);
        }
        assert_eq!(samples.len(), 7 * MIX_SAMPLES);
        assert!(samples.last().unwrap() < &-0.49);
        for pair in samples.as_chunks::<2>().0.windows(2) {
            assert!((pair[1][0] - pair[0][0]).abs() < 0.02, "splice step: {pair:?}");
        }
    }

    #[test]
    fn exit_pop_retired_source_fades_buffered_pcm_before_acknowledgement() {
        struct Recording {
            samples: Vec<f32>,
            needed: usize,
        }
        impl Output for Recording {
            fn needs_frame(&self) -> bool {
                self.needed > 0
            }
            fn frames_needed(&self) -> usize {
                self.needed
            }
            fn push(&mut self, samples: &[f32], _: Option<u64>) -> bool {
                self.samples.extend_from_slice(samples);
                self.needed = self.needed.saturating_sub(samples.len() / 2);
                true
            }
        }
        for packets in [1, 12] {
            let now = Instant::now();
            let (mut source, mut state) = SourceState::prepare(dsp::Config::low_latency_pcm(), false).unwrap();
            let mut encoder = dsp::Encoder::new_with_config(dsp::Config::low_latency_pcm()).unwrap();
            let payload = encoder.encode(&[0.25; 240]).unwrap();
            for sequence in 0..packets {
                assert!(source.push(sequence, &payload, now, None, None));
            }
            let registry = Registry::new();
            registry.slots[0].store(&mut *state, Ordering::Release);
            let mut output = Recording {
                samples: Vec::new(),
                needed: 120,
            };
            registry.work(&mut output, now + Duration::from_millis(50));
            let before = *output.samples.last().unwrap();
            assert!(before > 0.24);
            output.samples.clear();
            drop(source); // Same retirement as link_lost / stream removal.
            for tick in 0..8 {
                output.needed = 120;
                registry.work(&mut output, now + Duration::from_millis(50 + tick * 3));
            }
            assert!(
                (output.samples[0] - before).abs() < 0.001,
                "retirement cuts nonzero PCM directly to silence"
            );
            assert!(output.samples[2..1440].iter().any(|s| *s > 0.01 && *s < 0.24));
            assert!(output.samples[1438..].iter().all(|s| *s == 0.0));
            for pair in output.samples.as_chunks::<2>().0.windows(2) {
                assert!((pair[1][0] - pair[0][0]).abs() < 0.001);
            }
            assert!(registry.slots[0].load(Ordering::Acquire).is_null());
        }
    }
    struct OutputProbe {
        needed: usize,
        frames: usize,
        peak: f32,
    }
    impl Output for OutputProbe {
        fn needs_frame(&self) -> bool {
            self.needed > 0
        }
        fn frames_needed(&self) -> usize {
            self.needed
        }
        fn push(&mut self, samples: &[f32], _: Option<u64>) -> bool {
            self.needed = self.needed.saturating_sub(samples.len() / 2);
            self.frames += samples.len() / 2;
            for sample in samples {
                self.peak = self.peak.max(sample.abs());
            }
            true
        }
    }
    #[test]
    fn exit_pop_retirement_does_not_replay_paused_pcm() {
        let config = dsp::Config::low_latency_pcm();
        let now = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.25; 240]).unwrap();
        for sequence in 0..12 {
            assert!(source.push(sequence, &payload, now, None, None));
        }
        let registry = Registry::new();
        registry.slots[0].store(&mut *state, Ordering::Release);
        let mut out = OutputProbe {
            needed: 120,
            frames: 0,
            peak: 0.0,
        };
        registry.work(&mut out, now + Duration::from_millis(50));
        assert!(out.peak > 0.24);
        source.status.playing.store(false, Ordering::Release);
        out.peak = 0.0;
        out.needed = 120;
        registry.work(&mut out, now + Duration::from_millis(53));
        assert_eq!(out.peak, 0.0);
        drop(source);
        out.needed = 120;
        registry.work(&mut out, now + Duration::from_millis(56));
        assert_eq!(out.peak, 0.0, "retirement must not make a paused source audible");
        assert!(registry.slots[0].load(Ordering::Acquire).is_null());
    }
    #[test]
    fn short_opus_concealment_is_measured_when_rendered_and_late_packets_are_separate() {
        let mut config = dsp::Config::low_latency_opus();
        config.jitter.adaptive = false;
        let origin = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let counter = Arc::new(DropoutCounters::default());
        source.set_dropout_counters(counter.clone());
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.2; 480]).unwrap();
        for seq in [0, 1, 3, 4, 5, 6] {
            assert!(source.push(seq, &payload, origin, None, None));
        }
        state.receive(origin);
        let mut output = [0.0; MIX_SAMPLES];
        for tick in 0..6 {
            state.render(origin + Duration::from_millis(100 + tick * 3), 120, &mut output);
        }
        let at = (crate::diagnostics::monotonic_us(origin + Duration::from_millis(200)).max(0) / 1000) as u64;
        let before: Vec<_> = counter.snapshot(at).into_iter().flatten().collect();
        let concealed = before.iter().map(|b| b.concealed_ns).sum::<u64>();
        assert!((4_900_000..=5_100_000).contains(&concealed), "{concealed}");
        assert_eq!(before.iter().map(|b| b.interruptions).sum::<u64>(), 0);
        assert!(source.push(2, &payload, origin + Duration::from_millis(150), None, None));
        state.receive(origin + Duration::from_millis(150));
        assert!(source.push(2, &payload, origin + Duration::from_millis(160), None, None));
        state.receive(origin + Duration::from_millis(160));
        let after: Vec<_> = counter.snapshot(at).into_iter().flatten().collect();
        assert_eq!(after.iter().map(|b| b.late_packets).sum::<u64>(), 1);
        assert_eq!(after.iter().map(|b| b.interruptions).sum::<u64>(), 0);
        assert_eq!(after.iter().map(|b| b.concealed_ns).sum::<u64>(), concealed);
    }
    #[test]
    fn local_dropout_counter_uses_receiver_classification_and_survives_bursts() {
        let config = dsp::Config::low_latency_pcm();
        let origin = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let counter = Arc::new(DropoutCounters::default());
        source.set_dropout_counters(counter.clone());
        let at = (crate::diagnostics::monotonic_us(origin).max(0) / 1000) as u64;
        let total = || {
            counter
                .snapshot(at)
                .into_iter()
                .flatten()
                .map(|b| b.count())
                .sum::<u64>()
        };
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.2; 240]).unwrap();
        for seq in 0..4 {
            assert!(source.push(seq, &payload, origin, None, None));
        }
        state.receive(origin);
        let (_, trace) = state.receiver.render_with_trace(origin + Duration::from_secs(1));
        let decoded = trace
            .decoded_first_sequence
            .expect("prefilled media must decode before testing late arrivals");
        assert!(source.push(decoded, &payload, origin, None, None));
        state.receive(origin);
        assert_eq!(total(), state.receiver.stats().late);
        assert_eq!(total(), 1);
        // Admission rejection and queued duplicate do not establish a new gap.
        assert!(!source.push(20, &[], origin, None, None));
        assert!(source.push(20, &payload, origin, None, None));
        assert!(source.push(20, &payload, origin, None, None));
        state.receive(origin);
        assert_eq!(total(), 1);
        state.receiver.reset_burst();
        assert_eq!(state.receiver.stats().late, 0);
        assert_eq!(total(), 1);
    }
    #[test]
    fn refill_window_replacement_releases_retired_pcm_and_idle_drain_completes() {
        let mut config = dsp::Config::low_latency_pcm();
        config.jitter.adaptive = false;
        let origin = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let registry = Registry::new();
        registry.slots[0].store(&mut *state, Ordering::Release);
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let payload = encoder.encode(&[0.2; 240]).unwrap();
        let mut output = OutputProbe {
            needed: 0,
            frames: 0,
            peak: 0.0,
        };
        assert!(source.push(1, &payload, origin, None, None));
        for tick in 0..100 {
            output.needed = config.frame_size();
            registry.work(&mut output, origin + Duration::from_millis(tick * 10));
        }
        let restored = origin + Duration::from_secs(2);
        assert!(source.push(100, &payload, restored, None, None));
        output.needed = config.frame_size();
        registry.work(&mut output, restored);
        assert_eq!(source.status.occupied.load(Ordering::Acquire), config.frame_size());
        // The new sequence window replaces this one-packet partial refill.
        let sequence = 100 + state.receiver.packet_capacity() as u64 * 2;
        for offset in 0..3 {
            assert!(source.push(sequence + offset, &payload, restored, None, None));
        }
        output.peak = 0.0;
        output.needed = config.frame_size();
        registry.work(&mut output, restored);
        // Recovery starts immediately, now with a five-millisecond splice ramp.
        assert!(output.peak > 0.01, "restored packets render in the same source/burst");
        assert_eq!(
            source.status.occupied.load(Ordering::Acquire),
            state.receiver.buffered_frames()
        );
        registry.draining.store(true, Ordering::Release);
        for tick in 0..10 {
            output.needed = config.frame_size();
            registry.work(&mut output, restored + Duration::from_millis(tick * 3));
        }
        assert!(registry.drained.load(Ordering::Acquire));
        assert_eq!(source.status.occupied.load(Ordering::Acquire), 0);
    }
    #[test]
    fn direct_reader_runs_with_stalled_control_and_keeps_receiver_on_pause() {
        let config = dsp::Config::low_latency_pcm();
        let (source, mut state) = SourceState::prepare(config, true).unwrap();
        let sequence = Arc::new(AtomicU64::new(0));
        let enabled = Arc::new(AtomicBool::new(true));
        let cursor = sequence.clone();
        let playing = enabled.clone();
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let mut payload = [0; dsp::MAX_PACKET_BYTES];
        let len = encoder.encode_into(&[0.1; 240], &mut payload).unwrap();
        source.set_packet_reader(Arc::new(move |budget| {
            *budget = budget.saturating_sub(1);
            let sequence = cursor.fetch_add(1, Ordering::Relaxed);
            (
                playing.load(Ordering::Acquire),
                Some(Packet {
                    burst: 1,
                    sequence,
                    payload,
                    len,
                    arrived: Instant::now(),
                    network_read: None,
                    source_emit: None,
                }),
            )
        }));
        let registry = Registry::new();
        registry.slots[0].store(&mut *state, Ordering::Release);
        let mut output = OutputProbe {
            needed: 0,
            frames: 0,
            peak: 0.0,
        };
        crate::allocation_audit::assert_no_heap(|| {
            for _ in 0..2000 {
                output.needed = config.frame_size();
                registry.work(&mut output, Instant::now());
            }
        });
        assert!(sequence.load(Ordering::Relaxed) >= 2000);
        assert!(output.peak > 0.01);
        enabled.store(false, Ordering::Release);
        output.peak = 0.0;
        output.needed = config.frame_size();
        registry.work(&mut output, Instant::now());
        assert_eq!(output.peak, 0.0);
        assert!(!registry.slots[0].load(Ordering::Acquire).is_null());
        assert!(source.status.active.load(Ordering::Acquire));
        enabled.store(true, Ordering::Release);
        state.receive(Instant::now());
        assert!(source.status.playing.load(Ordering::Acquire));
    }
    #[test]
    fn full_ingress_and_render_work_have_no_heap_and_share_the_original_budget() {
        for config in [
            dsp::Config::opus_10ms(),
            dsp::Config::low_latency_opus(),
            dsp::Config::low_latency_pcm(),
        ] {
            let registry = Registry::new();
            let (mut source, mut state) = SourceState::prepare(config, true).unwrap();
            registry.slots[0].store(&mut *state, Ordering::Release);
            let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
            let mut payload = [0; dsp::MAX_PACKET_BYTES];
            let len = encoder
                .encode_into(&[0.1; dsp::FRAME_SAMPLES][..config.frame_samples()], &mut payload)
                .unwrap();
            let mut out = OutputProbe {
                needed: 0,
                frames: 0,
                peak: 0.0,
            };
            let start = Instant::now();
            crate::allocation_audit::assert_no_heap(|| {
                for seq in 1..=2000 {
                    let now = start + Duration::from_micros(seq * config.frame_size() as u64 * 1_000_000 / 48_000);
                    source.push(seq, &payload[..len], now, None, None);
                    // Exercise a full ingress queue and full diagnostic queues.
                    if seq % 100 < 80 {
                        out.needed = config.frame_size();
                        registry.work(&mut out, now);
                        source.poll();
                        while source.outcome().is_some() {}
                    }
                    assert!(source.status.occupied.load(Ordering::Acquire) <= source.budget);
                }
            });
            assert!(out.peak > 0.01);
            assert!(source.stats().received > 0);
            assert!(source.take_rejections().queue_capacity > 0);
            drop(source);
            crate::allocation_audit::assert_no_heap(|| {
                // Retirement now consumes the finite fade before releasing its
                // storage; it uses the same RT budget and remains allocation-free.
                for _ in 0..8 {
                    out.needed = 120;
                    registry.work(&mut out, Instant::now());
                }
            });
            assert!(registry.slots[0].load(Ordering::Acquire).is_null());
        }
    }
    #[test]
    fn retired_source_is_acknowledged_before_storage_reuse() {
        let registry = Arc::new(Registry::new());
        let shared = registry.clone();
        let worker = thread::spawn(move || {
            let mut out = OutputProbe {
                needed: 0,
                frames: 0,
                peak: 0.0,
            };
            while !shared.stop.load(Ordering::Acquire) {
                out.needed = 120;
                crate::allocation_audit::assert_no_heap(|| shared.work(&mut out, Instant::now()));
                thread::yield_now();
            }
        });
        for _ in 0..1000 {
            let (mut source, mut state) = SourceState::prepare(dsp::Config::low_latency_pcm(), false).unwrap();
            let status = source.status.clone();
            assert!(registry.slots[0].load(Ordering::Acquire).is_null());
            registry.slots[0].store(&mut *state, Ordering::Release);
            assert!(source.push(1, &[0; 480], Instant::now(), None, None));
            drop(source);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !registry.slots[0].load(Ordering::Acquire).is_null() {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            assert!(!status.active.load(Ordering::Acquire));
            drop(state);
            thread::yield_now();
            assert!(!status.active.load(Ordering::Acquire));
        }
        registry.stop.store(true, Ordering::Release);
        worker.join().unwrap();
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;
    #[test]
    fn startup_backlog_keeps_complete_pcm_onset_through_real_receiver() {
        let config = dsp::Config::low_latency_pcm();
        let now = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        // Ten ms pre-roll, twenty ms nonzero linear PCM, then twenty ms digital zero.
        // Device opening took fifty ms; arrival timestamps retain their original spacing.
        for frame in 0..20_u64 {
            let mut pcm = [0.0; 240];
            if (4..12).contains(&frame) {
                for (index, pair) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let value = (2000 + (frame as usize - 4) * 120 + index) as f32 / 32767.0;
                    pair.copy_from_slice(&[value, -value]);
                }
            }
            let mut payload = [0; dsp::MAX_PACKET_BYTES];
            let len = encoder.encode_into(&pcm, &mut payload).unwrap();
            let arrived = now + Duration::from_micros(frame * 2500);
            assert!(source.push_packet(Packet {
                burst: 2,
                sequence: frame + 1,
                payload,
                len,
                arrived,
                network_read: Some(arrived),
                source_emit: Some(frame * 2_500_000)
            }));
        }
        let started = now + Duration::from_millis(50);
        let mut rendered = Vec::new();
        for _ in 0..3 {
            state.receive(started);
        }
        assert_eq!(state.receiver.stats().received, 20);
        assert_eq!(state.receiver.stats().arrival_jitter_ms, 0.0);
        assert_eq!(state.receiver.stats().current_target_ms, 5.0);
        for tick in 0..18 {
            let mut pcm = [0.0; MIX_SAMPLES];
            state.render(started + Duration::from_micros(tick * 2500), 120, &mut pcm);
            for pair in pcm.as_chunks::<2>().0 {
                assert!((pair[0] + pair[1]).abs() < 1e-6);
                rendered.push(pair[0] * 32768.0);
            }
        }
        let onset: Vec<_> = rendered
            .into_iter()
            .filter(|value| *value >= 2000.0 && *value <= 2959.1)
            .collect();
        assert!(!onset.is_empty());
        assert!(onset[0] < 2001.1, "first onset sample was skipped: {}", onset[0]);
        // Resampling may interpolate an edge sample; no codec frame or nonzero
        // block is trimmed to erase startup latency.
        let peak = onset.iter().copied().fold(0.0_f32, f32::max);
        assert!(peak > 2957.9, "end of onset missing: {peak}");
        let peak_index = onset.iter().position(|value| *value == peak).unwrap();
        // Existing receiver interpolation can hold one boundary sample; allow
        // that single-sample correction, never a skipped 120-frame PCM packet.
        for pair in onset[..=peak_index].windows(2) {
            assert!(
                (0.0..=2.025).contains(&(pair[1] - pair[0])),
                "PCM discontinuity: {pair:?}"
            );
        }
        assert_eq!(state.receiver.stats().latency_trimmed_frames, 0);
        assert_eq!(state.receiver.stats().lost, 0);
        assert!(state.receiver.stats().adaptation_ppm > 0.0);
    }
    #[test]
    fn idle_drain_preserves_a_cold_start_onset_even_after_packet_timeout() {
        struct DrainOutput {
            needed: usize,
            samples: [f32; 4800],
            len: usize,
        }
        impl Output for DrainOutput {
            fn needs_frame(&self) -> bool {
                self.needed > 0
            }
            fn frames_needed(&self) -> usize {
                self.needed
            }
            fn push(&mut self, samples: &[f32], _: Option<u64>) -> bool {
                self.samples[self.len..self.len + samples.len()].copy_from_slice(samples);
                self.len += samples.len();
                self.needed = self.needed.saturating_sub(samples.len() / 2);
                true
            }
        }
        let config = dsp::Config::low_latency_pcm();
        let now = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
        // The dormant owner received a finite beep, and a very slow open finished
        // after the thirty-second no-packet timeout. No ingress queue overflowed.
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        for sequence in 1..=8 {
            let mut payload = [0; dsp::MAX_PACKET_BYTES];
            let len = encoder
                .encode_into(&[if sequence > 6 { 0.25 } else { 0.0 }; 240], &mut payload)
                .unwrap();
            assert!(source.push_packet(Packet {
                burst: 2,
                sequence,
                payload,
                len,
                arrived: now - Duration::from_secs(31),
                network_read: None,
                source_emit: None
            }));
        }
        let registry = Registry::new();
        registry.slots[0].store(&mut *state, Ordering::Release);
        registry.draining.store(true, Ordering::Release);
        let mut output = DrainOutput {
            needed: 0,
            samples: [0.0; 4800],
            len: 0,
        };
        crate::allocation_audit::assert_no_heap(|| {
            for tick in 0..20 {
                output.needed = 120;
                registry.work(&mut output, now + Duration::from_micros(tick * 2500));
                if registry.drained.load(Ordering::Acquire) {
                    break;
                }
            }
        });
        assert!(registry.drained.load(Ordering::Acquire));
        assert_eq!(source.status.occupied.load(Ordering::Acquire), 0);
        assert_eq!(
            state.receiver.stats().lost,
            0,
            "drain must zero fill rather than invent PLC"
        );
        assert!(
            output.samples[..output.len]
                .iter()
                .filter(|sample| **sample > 0.24)
                .count()
                >= 476,
            "leading zero periods must not acknowledge shutdown before the beep"
        );
        assert!(output.samples[..1200].iter().all(|sample| *sample == 0.0));
    }
    #[test]
    fn new_burst_resets_decoder_jitter_and_sequence_even_if_first_packet_was_lost() {
        for config in [
            dsp::Config::low_latency_pcm(),
            dsp::Config::low_latency_opus(),
            dsp::Config::opus_10ms(),
        ] {
            let now = Instant::now();
            let (mut source, mut state) = SourceState::prepare(config, false).unwrap();
            let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
            let mut payload = [0; dsp::MAX_PACKET_BYTES];
            let len = encoder
                .encode_into(&[0.3; dsp::FRAME_SAMPLES][..config.frame_samples()], &mut payload)
                .unwrap();
            assert!(source.push_packet(Packet {
                burst: 1,
                sequence: 5000,
                payload,
                len,
                arrived: now,
                network_read: None,
                source_emit: None
            }));
            state.receive(now);
            let mut pcm = [0.0; MIX_SAMPLES];
            for _ in 0..20 {
                state.render(now, 120, &mut pcm);
            }
            encoder.reset().unwrap();
            let len = encoder
                .encode_into(&[0.1; dsp::FRAME_SAMPLES][..config.frame_samples()], &mut payload)
                .unwrap();
            for sequence in 2..=7 {
                assert!(source.push_packet(Packet {
                    burst: 2,
                    sequence,
                    payload,
                    len,
                    arrived: now + Duration::from_secs(40),
                    network_read: None,
                    source_emit: None
                }));
            }
            crate::allocation_audit::assert_no_heap(|| state.receive(now + Duration::from_secs(40)));
            assert_eq!(state.burst, 2);
            assert_eq!(state.receiver.stats().received, 6);
            assert_eq!(state.receiver.stats().rejected, 0);
            assert_eq!(state.receiver.stats().late, 0);
            // Compare a freshly created receiver fed the same surviving new burst.
            let mut fresh = dsp::Receiver::new_with_config(config).unwrap();
            fresh.reset_burst();
            for sequence in 2..=7 {
                assert!(fresh.push_slice_at(sequence, &payload[..len], now + Duration::from_secs(40), None));
            }
            for tick in 0..4 {
                let at = now + Duration::from_secs(40) + Duration::from_millis(tick * 10);
                let mut a = [0.0; dsp::FRAME_SAMPLES];
                let mut b = [0.0; dsp::FRAME_SAMPLES];
                state
                    .receiver
                    .render_into(at, config.frame_size(), &mut a[..config.frame_samples()], None);
                fresh.render_into(at, config.frame_size(), &mut b[..config.frame_samples()], None);
                assert_eq!(a, b, "old decoder/phase/jitter leaked into the new burst");
            }
        }
    }
}

/// Executes the production callback core without opening any native device.
/// The ordinary owner creates/publishes sources; every fill owns the full DSP
/// transaction. Intended for integration audits and synthetic deadline evidence.
#[cfg(feature = "test-support")]
pub struct Simulator {
    registry: Arc<Registry>,
    states: Vec<(usize, Box<SourceState>)>,
    render: crate::playback::Render,
}
#[cfg(feature = "test-support")]
impl Simulator {
    pub fn new(trace: bool) -> Self {
        let registry = Arc::new(Registry::new());
        let render = crate::playback::Render::simulated(Box::new(PullRender::new(registry.clone())), trace);
        Self {
            registry,
            states: Vec::with_capacity(MAX_SOURCES),
            render,
        }
    }
    pub fn source(&mut self, config: dsp::Config) -> Result<Source, String> {
        self.states
            .retain(|(index, _)| !self.registry.slots[*index].load(Ordering::Acquire).is_null());
        let index = self
            .registry
            .slots
            .iter()
            .position(|slot| slot.load(Ordering::Acquire).is_null())
            .ok_or("source slots awaiting retirement")?;
        let (source, mut state) = SourceState::prepare(config, false)?;
        let ptr = &mut *state as *mut SourceState;
        self.states.push((index, state));
        self.registry.slots[index].store(ptr, Ordering::Release);
        Ok(source)
    }
    pub fn fill(&mut self, output: &mut [f32], rate: u32, channels: u16, period: u32, at: Instant, gap: Option<u64>) {
        self.render.simulate(output, rate, channels, period, at, gap);
    }
    pub fn metrics(&self) -> PlaybackMetrics {
        self.render.observations()
    }
    pub fn set_output_quality_counters(&self, counters: Arc<DropoutCounters>) {
        self.render.simulated_quality(counters);
    }
    pub fn begin_idle_drain(&self) {
        self.registry.draining.store(true, Ordering::Release);
    }
    pub fn cancel_idle_drain(&self) {
        self.registry.draining.store(false, Ordering::Release);
    }
    pub fn invalidate(&mut self) {
        // &mut self proves no fill is running. Equivalent to native quiescence.
        self.registry.stop.store(true, Ordering::Release);
        for slot in &self.registry.slots {
            slot.store(std::ptr::null_mut(), Ordering::Release);
        }
        self.states.clear();
    }
}

#[cfg(feature = "test-support")]
impl std::fmt::Debug for Simulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Simulator")
            .field("sources", &self.states.len())
            .finish_non_exhaustive()
    }
}
