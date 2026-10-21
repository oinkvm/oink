//! Native-cadenced receiver/decoder/mixer. Session routing, media admission,
//! logs and device lifetime stay on the owning ordinary thread.
use crate::{
    dsp, handoff,
    playback::{Feed, Playback, PlaybackInfo, PlaybackMetrics, PlaybackOptions},
    realtime,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod dropouts;
pub(crate) mod quality;
pub(crate) use dropouts::Ledger as DropoutLedger;
pub use dropouts::{DropoutBucket, DropoutCounters};

const MAX_SOURCES: usize = 16;
const MIX_SAMPLES: usize = 240;
const INPUT_BATCH: usize = 8;
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
type PacketReader = Arc<dyn Fn() -> (bool, Option<Packet>) + Send + Sync>;
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
    pub at: Instant,
}
struct Status {
    reader: std::sync::OnceLock<PacketReader>,
    playing: AtomicBool,
    active: AtomicBool,
    // Packet handoff plus existing receiver occupancy share this exact budget.
    // There is no second jitter reserve in front of Receiver.
    occupied: AtomicUsize,
    accepted: AtomicU64,
    dropout_counters: std::sync::OnceLock<Arc<DropoutCounters>>,
    highest_accepted: AtomicU64,
    accepted_at_ns: AtomicU64,
    epoch: Instant,
    feedback_dropped: AtomicU64,
    rendered_dropped: AtomicU64,
}
/// Ordinary-thread endpoint; the underlying DSP exists only on the RT worker.
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
    wake: realtime::Signal,
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
    /// Read M1 directly at the render worker's native cadence. False means paused:
    /// queued packets are consumed without playback and the receiver is retained.
    pub fn set_packet_reader(&self, reader: Arc<dyn Fn() -> (bool, Option<Packet>) + Send + Sync>) {
        let _ = self.status.reader.set(reader);
        self.wake.notify();
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
        // Admit ready media promptly instead of spending another device period
        // in the handoff. Rendering is still gated by native consumption demand,
        // so arrival wakeups cannot advance the playback clock or add reserve.
        self.wake.notify();
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
        self.wake.notify();
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
        // Control-side retirement may wake an otherwise silent device waiter.
        self.wake.notify();
    }
}
struct SourceState {
    packets: handoff::Consumer<Packet>,
    outcomes: handoff::Producer<Option<Outcome>>,
    snapshots: handoff::Producer<Snapshot>,
    status: Arc<Status>,
    receiver: dsp::Receiver,
    rendered: Option<handoff::Producer<Option<Rendered>>>,
    pcm: [f32; dsp::FRAME_SAMPLES],
    offset: usize,
    remaining: usize,
    rejections: dsp::Rejections,
    burst: u64,
    quality: quality::Interruptions,
}
impl SourceState {
    fn prepare(config: dsp::Config, wake: realtime::Signal, observe: bool) -> Result<(Source, Box<Self>), String> {
        let receiver = dsp::Receiver::new_with_config(config)?;
        let capacity = receiver.packet_capacity();
        let status = Arc::new(Status {
            reader: Default::default(),
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
            rendered,
            packets,
            outcomes,
            snapshots,
            status: status.clone(),
            receiver,
            pcm: [0.0; dsp::FRAME_SAMPLES],
            offset: 0,
            remaining: 0,
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
            wake,
        };
        Ok((source, state))
    }

    fn receive(&mut self, now: Instant) {
        self.receiver.set_dropout_counters(self.status.dropout_counters.get());
        for _ in 0..INPUT_BATCH {
            let packet = if let Some(packet) = self.packets.pop() {
                Some(packet)
            } else if let Some(read) = self.status.reader.get() {
                let (playing, packet) = read();
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
                self.offset = 0;
                self.remaining = 0;
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
    }
    fn render(&mut self, now: Instant, demand: usize, out: &mut [f32; MIX_SAMPLES]) {
        if self.remaining < MIX_SAMPLES {
            // All negotiated codec frames are multiples of the fixed mix unit.
            let samples = self.receiver.config().frame_samples();
            let before = self.receiver.buffered_frames();
            self.receiver.render_into(now, demand, &mut self.pcm[..samples], None);
            let after = self.receiver.buffered_frames();
            if after >= before {
                self.status.occupied.fetch_add(after - before, Ordering::Release);
            } else {
                self.status.occupied.fetch_sub(before - after, Ordering::AcqRel);
            }
            self.offset = 0;
            self.remaining = samples;
        }
        out.copy_from_slice(&self.pcm[self.offset..self.offset + MIX_SAMPLES]);
        if let Some(counters) = self.status.dropout_counters.get() {
            self.quality.block(
                (crate::diagnostics::monotonic_us(now).max(0) / 1000) as u64,
                &self.receiver.output_kinds()[self.offset / 2..(self.offset + MIX_SAMPLES) / 2],
                out,
                counters,
            );
        }
        if let Some(rendered) = &mut self.rendered
            && rendered.push(Some(Rendered { samples: *out, at: now })).is_err()
        {
            self.status.rendered_dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.offset += MIX_SAMPLES;
        self.remaining -= MIX_SAMPLES;
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
trait Output {
    fn needs_frame(&self) -> bool;
    fn frames_needed(&self) -> usize;
    fn push(&mut self, samples: &[f32], sequence: Option<u64>) -> bool;
}
impl Output for Feed {
    fn needs_frame(&self) -> bool {
        self.needs_frame()
    }
    fn frames_needed(&self) -> usize {
        self.frames_needed()
    }
    fn push(&mut self, samples: &[f32], sequence: Option<u64>) -> bool {
        self.push(samples, sequence)
    }
}
struct Registry {
    slots: [AtomicPtr<SourceState>; MAX_SOURCES],
    stop: AtomicBool,
    draining: AtomicBool,
    drained: AtomicBool,
    started: AtomicUsize,
    error: AtomicI32,
    metrics: realtime::Counters,
}
// SAFETY: slots publish exclusively-owned SourceStates. Only the single RT
// thread dereferences them; ordinary owner never reads/mutates a live state.
// RT clears a slot with Release after its last access; ordinary owner reclaims
// only after observing that null with Acquire (or after joining the worker).
impl Registry {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            stop: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            drained: AtomicBool::new(false),
            started: AtomicUsize::new(0),
            error: AtomicI32::new(0),
            metrics: realtime::Counters::default(),
        }
    }
    fn work(&self, feed: &mut impl Output, now: Instant) {
        for slot in &self.slots {
            let ptr = slot.load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            // SAFETY: one RT reader, owner retains allocation until slot clear.
            let state = unsafe { &mut *ptr };
            if !state.status.active.load(Ordering::Acquire) {
                slot.store(std::ptr::null_mut(), Ordering::Release);
                continue;
            }
            state.receive(now);
        }
        let draining = self.draining.load(Ordering::Acquire);
        for _ in 0..8 {
            if draining {
                let pending = self.slots.iter().any(|slot| {
                    let ptr = slot.load(Ordering::Acquire);
                    if ptr.is_null() {
                        return false;
                    }
                    // SAFETY: this is the same sole RT owner as the render loop.
                    let state = unsafe { &*ptr };
                    state.remaining != 0 || state.status.occupied.load(Ordering::Acquire) != 0
                });
                if !pending {
                    // Stop producing too: the native queue must become empty before
                    // its zero-period acknowledgement is allowed to close the stream.
                    self.drained.store(true, Ordering::Release);
                    break;
                }
            }
            if !feed.needs_frame() {
                break;
            }
            let mut inputs = [[0.0; MIX_SAMPLES]; MAX_SOURCES];
            let mut present = 0;
            for slot in &self.slots {
                let ptr = slot.load(Ordering::Acquire);
                if ptr.is_null() {
                    continue;
                }
                // SAFETY: as above; retirement is completed only by this loop.
                let state = unsafe { &mut *ptr };
                if state.status.active.load(Ordering::Acquire)
                    && state.status.playing.load(Ordering::Acquire)
                    && (!draining || state.remaining != 0 || state.status.occupied.load(Ordering::Acquire) != 0)
                {
                    state.receiver.set_draining(draining);
                    state.render(
                        now,
                        feed.frames_needed().saturating_sub(state.remaining / 2),
                        &mut inputs[present],
                    );
                    present += 1;
                }
            }
            let refs: [&[f32]; MAX_SOURCES] = std::array::from_fn(|i| inputs[i].as_slice());
            let mut mixed = [0.0; MIX_SAMPLES];
            dsp::mix(&refs[..present], &mut mixed);
            feed.push(&mixed, None);
        }
        for slot in &self.slots {
            let ptr = slot.load(Ordering::Acquire);
            if !ptr.is_null() {
                // SAFETY: same exclusive RT ownership, no control dereference.
                unsafe { &mut *ptr }.snapshot();
            }
        }
    }
}

pub struct Sink {
    playback: Playback,
    registry: Arc<Registry>,
    // Boxing is intentional: published addresses must not move with this Vec.
    states: Vec<(usize, Box<SourceState>)>,
    wake: realtime::Signal,
    worker: Option<JoinHandle<(Feed, realtime::Schedule)>>,
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
        let mut playback = Playback::start_with_options(id, options)?;
        let (mut feed, mut schedule, wake) = playback.take_realtime_feed()?;
        let registry = Arc::new(Registry::new());
        let shared = registry.clone();
        let worker_wake = wake.clone();
        let worker = thread::Builder::new()
            .name("oink-audio-render".into())
            .spawn(move || {
                {
                    match schedule.enter() {
                        Ok(guard) => {
                            shared.started.store(1, Ordering::Release);
                            let mut previous: Option<Instant> = None;
                            while !shared.stop.load(Ordering::Acquire) {
                                if let Err(error) = worker_wake.wait() {
                                    shared.error.store(error, Ordering::Release);
                                    break;
                                }
                                if shared.stop.load(Ordering::Acquire) {
                                    break;
                                }
                                let now = Instant::now();
                                shared.work(&mut feed, now);
                                shared.metrics.observe(
                                    previous.map(|before| now.saturating_duration_since(before)),
                                    now.elapsed(),
                                );
                                previous = Some(now);
                            }
                            drop(guard);
                        }
                        Err(error) => {
                            shared.error.store(error, Ordering::Release);
                            shared.started.store(2, Ordering::Release);
                        }
                    }
                }
                (feed, schedule)
            })
            .map_err(|e| format!("start audio render worker: {e}"))?;
        let mut sink = Self {
            playback,
            registry,
            states: Vec::with_capacity(MAX_SOURCES),
            wake,
            worker: Some(worker),
        };
        let started = Instant::now();
        while sink.registry.started.load(Ordering::Acquire) == 0 && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(1));
        }
        if sink.registry.started.load(Ordering::Acquire) != 1 {
            sink.stop();
            return Err(format!(
                "audio render scheduling failed: {}",
                sink.registry.error.load(Ordering::Acquire)
            ));
        }
        sink.wake.notify();
        Ok(sink)
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
        let (source, mut state) = SourceState::prepare(config, self.wake.clone(), observe)?;
        let ptr = &mut *state as *mut SourceState;
        self.states.push((index, state));
        self.registry.slots[index].store(ptr, Ordering::Release);
        Ok(source)
    }

    fn stop(&mut self) {
        self.registry.stop.store(true, Ordering::Release);
        self.wake.notify();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        for (_, state) in &self.states {
            state.status.active.store(false, Ordering::Release);
        }
        for slot in &self.registry.slots {
            slot.store(std::ptr::null_mut(), Ordering::Release);
        }
        self.states.clear();
    }
    pub fn error(&self) -> Option<String> {
        let error = self.registry.error.load(Ordering::Acquire);
        if error != 0 {
            Some(format!("audio render worker failed: {error}"))
        } else if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            Some("audio render worker stopped".into())
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
        self.wake.notify();
    }
    pub fn cancel_idle_drain(&self) {
        self.playback.cancel_idle_drain();
        self.registry.draining.store(false, Ordering::Release);
        self.registry.drained.store(false, Ordering::Release);
        self.wake.notify();
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
        let mut metrics = self.playback.take_metrics();
        metrics.worker = Some(self.registry.metrics.take());
        metrics
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
    fn short_opus_concealment_is_measured_when_rendered_and_late_packets_are_separate() {
        let mut config = dsp::Config::low_latency_opus();
        config.jitter.adaptive = false;
        let origin = Instant::now();
        let (mut source, mut state) = SourceState::prepare(config, realtime::Signal::new().unwrap(), false).unwrap();
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
        let (mut source, mut state) = SourceState::prepare(config, realtime::Signal::new().unwrap(), false).unwrap();
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
        let (mut source, mut state) = SourceState::prepare(config, realtime::Signal::new().unwrap(), false).unwrap();
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
        assert!(output.peak > 0.1, "restored packets render in the same source/burst");
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
        let (source, mut state) = SourceState::prepare(config, realtime::Signal::new().unwrap(), true).unwrap();
        let sequence = Arc::new(AtomicU64::new(0));
        let enabled = Arc::new(AtomicBool::new(true));
        let cursor = sequence.clone();
        let playing = enabled.clone();
        let mut encoder = dsp::Encoder::new_with_config(config).unwrap();
        let mut payload = [0; dsp::MAX_PACKET_BYTES];
        let len = encoder.encode_into(&[0.1; 240], &mut payload).unwrap();
        source.set_packet_reader(Arc::new(move || {
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
            let signal = realtime::Signal::new().unwrap();
            let registry = Registry::new();
            let (mut source, mut state) = SourceState::prepare(config, signal, true).unwrap();
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
            registry.work(&mut out, Instant::now());
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
        let wake = realtime::Signal::new().unwrap();
        for _ in 0..1000 {
            let (mut source, mut state) =
                SourceState::prepare(dsp::Config::low_latency_pcm(), wake.clone(), false).unwrap();
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
        let (mut source, mut state) = SourceState::prepare(config, realtime::Signal::new().unwrap(), false).unwrap();
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
        let (mut source, mut state) = SourceState::prepare(config, realtime::Signal::new().unwrap(), false).unwrap();
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
            let (mut source, mut state) =
                SourceState::prepare(config, realtime::Signal::new().unwrap(), false).unwrap();
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
