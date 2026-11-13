use super::{BLOCK, CHUNK, Control, Reason, Time, TimeQuality, clock::Clock};
use crate::{handoff, microphone::resample::Resampler};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

const HISTORY: usize = 48_000 * 2;
const TAPS: usize = 32;
const PHASES: usize = 256;
const QUEUE: usize = 192_000 * 40 / 1000 / CHUNK;
/// Arrival-lateness window (real packets) behind the read delay.
const LATE_WINDOW: usize = 64;
/// Raise the read delay only when this many packets of the window were later
/// than it: a single scheduling hiccup must not delay the whole reference.
const LATE_RANK: usize = 4;
const READ_MARGIN_NS: f64 = 1_000_000.0;
/// Upper bound of the read delay. A reference that arrives later than this
/// after its mapped time trails its own echo in the microphone (the echo
/// reaches the mic within the input latency plus the acoustic path, a few ms
/// to tens of ms), which AEC3 cannot cancel: those blocks bypass as
/// `ReferenceLate` instead of feeding a reference the filter cannot use.
pub(super) const MAX_READ_DELAY_NS: f64 = 40_000_000.0;
/// Which producer a reference comes from. Exactly one is selected; the system
/// mix already contains Oink's output, so the two are never added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Kind {
    Oink = 0,
    System = 1,
}
/// Fixed storage of one reference reader and its handoff queue.
pub(super) fn reference_bytes() -> usize {
    HISTORY * std::mem::size_of::<[f32; 2]>()
        + PHASES * TAPS * std::mem::size_of::<f32>()
        + QUEUE * std::mem::size_of::<Packet>()
}
#[derive(Clone, Copy)]
pub(super) struct Packet {
    pub samples: [[f32; CHUNK]; 2],
    pub frames: usize,
    pub time: Time,
    pub sequence: u64,
    /// Known silence filled in by the system feed (an idle loopback endpoint),
    /// not delivered audio: valid reference, but no arrival-lateness sample.
    pub synthetic: bool,
    /// The last chunk of one native delivery: availability (arrival
    /// lateness) is measured at the end of what that delivery made readable.
    pub delivery_end: bool,
}
pub(super) struct Incoming {
    pub kind: Kind,
    pub generation: u64,
    pub rate: Arc<AtomicU32>,
    pub consumer: handoff::Consumer<Packet>,
}
/// Created before native playback starts. Only the corresponding native callback
/// writes this endpoint; a new output never shares the old SPSC producer.
#[derive(Debug)]
pub struct RenderTap {
    control: Arc<Control>,
    generation: u64,
    rate: Arc<AtomicU32>,
    producer: handoff::Producer<Packet>,
    sequence: u64,
}
#[derive(Debug)]
pub struct OutputRegistration {
    control: Arc<Control>,
    generation: u64,
}
impl Drop for OutputRegistration {
    fn drop(&mut self) {
        self.control.output_stopped(self.generation);
    }
}
impl RenderTap {
    /// Separate ordinary owner: invalidates this output even if a native
    /// service reset forces conservative retention of the callback context.
    pub fn registration(&self) -> OutputRegistration {
        OutputRegistration {
            control: self.control.clone(),
            generation: self.generation,
        }
    }

    /// The Oink render reference: final application-visible playback samples.
    pub fn new(control: Arc<Control>) -> Self {
        Self::with_kind(control, Kind::Oink, 0)
    }
    /// The full-system output mix of the same physical output (desktop
    /// process tap / endpoint loopback). Its native rate is known at open.
    pub(super) fn system(control: Arc<Control>, rate: u32) -> Self {
        Self::with_kind(control, Kind::System, rate)
    }
    fn with_kind(control: Arc<Control>, kind: Kind, rate: u32) -> Self {
        let time = Time {
            device_clock: None,
            host_ns: None,
            native_frame: None,
            rate: 48_000,
            quality: TimeQuality::Unknown,
            discontinuity: true,
            delivered_at: std::time::Instant::now(),
        };
        let (producer, consumer) = handoff::channel(
            QUEUE,
            Packet {
                samples: [[0.0; CHUNK]; 2],
                frames: 0,
                time,
                sequence: 0,
                synthetic: false,
                delivery_end: false,
            },
        );
        let rate = Arc::new(AtomicU32::new(rate));
        let generation = control.register(Incoming {
            kind,
            generation: 0,
            rate: rate.clone(),
            consumer,
        });
        Self {
            control,
            generation,
            rate,
            producer,
            sequence: 0,
        }
    }
    /// Final application-visible samples only; on Windows this runs after a
    /// successful ReleaseBuffer and uses the values actually quantized/written.
    pub fn supply(&mut self, samples: &[f32], channels: usize, time: Time) {
        self.supply_packets(samples, channels, time, false);
    }
    /// Whether `update` selected this producer (it only pushes then).
    pub(super) fn wanted(&self) -> bool {
        self.control.wanted(self.generation)
    }
    pub(super) fn control(&self) -> &Arc<Control> {
        &self.control
    }
    pub(super) fn supply_packets(&mut self, samples: &[f32], channels: usize, time: Time, synthetic: bool) {
        if channels == 0 || !(8_000..=192_000).contains(&time.rate) {
            return;
        }
        if self.rate.swap(time.rate, Ordering::AcqRel) != time.rate {
            self.control.format_changed.store(true, Ordering::Release);
        }
        if !self.control.wanted(self.generation) {
            return;
        }
        let frames = samples.len() / channels;
        for first in (0..frames).step_by(CHUNK) {
            self.sequence = self.sequence.wrapping_add(1);
            let count = (frames - first).min(CHUNK);
            let mut packet = Packet {
                samples: [[0.0; CHUNK]; 2],
                frames: count,
                time: time.offset(first),
                sequence: self.sequence,
                synthetic,
                delivery_end: first + count == frames,
            };
            for i in 0..count {
                packet.samples[0][i] = samples[(first + i) * channels];
                if channels > 1 {
                    packet.samples[1][i] = samples[(first + i) * channels + 1];
                }
            }
            if self.producer.push(packet).is_err() {
                self.control
                    .reference_dropped
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
        }
        self.control.wake.notify();
    }
}
impl Drop for RenderTap {
    fn drop(&mut self) {
        self.control.output_stopped(self.generation);
    }
}

pub(super) struct Reference {
    pub kind: Kind,
    pub generation: u64,
    consumer: handoff::Consumer<Packet>,
    rate: u32,
    converters: [Resampler; 2],
    history: Box<[[f32; 2]]>,
    coefficients: Box<[[f32; TAPS]]>,
    pub clock: Clock,
    device_clock: Clock,
    origin_native: f64,
    count: u64,
    first: u64,
    previous_native: Option<f64>,
    sequence: Option<u64>,
    pub valid_frames: u64,
    pub invalid_frames: u64,
    phase: Option<f64>,
    step: f64,
    pub alignment_error_frames: f64,
    pub last_fault: Reason,
    late_us: [f32; LATE_WINDOW],
    late_cursor: usize,
    late_count: usize,
    /// How far behind the capture stamp the reference is read so that it has
    /// arrived: the measured arrival lateness of the reference (0 for the
    /// Oink render reference, which is stamped before it is presented).
    pub read_delay_ns: f64,
    pub late_exceeded: bool,
    pub late_max_us: f64,
    pub read_delay_steps: u64,
    pub synthetic_frames: u64,
    /// The newest history is filled silence (an idle endpoint): reads past it
    /// wait for the next delivery and say nothing about arrival lateness.
    newest_synthetic: bool,
    window_blocks: u32,
    window_misses: u32,
    window_short_ns: f64,
    /// The system reference first measures where its reads are available
    /// (whole windows without a needed raise) before AEC uses it: a read
    /// delay step under an adapted filter costs re-convergence, a step before
    /// the first processed block costs nothing.
    settled: bool,
    windows: u32,
    quiet_windows: u32,
}
impl Reference {
    pub fn new(incoming: Incoming, rate: u32) -> Self {
        let mut coefficients = vec![[0.0; TAPS]; PHASES].into_boxed_slice();
        for (phase, row) in coefficients.iter_mut().enumerate() {
            let fraction = phase as f64 / PHASES as f64;
            let mut sum = 0.0;
            for (tap, c) in row.iter_mut().enumerate() {
                let x = tap as f64 - 15.0 - fraction;
                let sinc = if x.abs() < 1e-12 {
                    0.9
                } else {
                    (std::f64::consts::PI * 0.9 * x).sin() / (std::f64::consts::PI * x)
                };
                *c = (sinc * (0.5 + 0.5 * (std::f64::consts::PI * x / 16.0).cos())) as f32;
                sum += *c;
            }
            for c in row {
                *c /= sum;
            }
        }
        Self {
            kind: incoming.kind,
            generation: incoming.generation,
            consumer: incoming.consumer,
            rate,
            converters: [Resampler::new(rate), Resampler::new(rate)],
            history: vec![[0.0; 2]; HISTORY].into_boxed_slice(),
            coefficients,
            clock: Clock::new(rate),
            device_clock: Clock::new(rate),
            origin_native: 0.0,
            count: 0,
            first: 0,
            previous_native: None,
            sequence: None,
            valid_frames: 0,
            invalid_frames: 0,
            phase: None,
            step: 1.0,
            alignment_error_frames: 0.0,
            last_fault: Reason::ReferenceMissing,
            late_us: [0.0; LATE_WINDOW],
            late_cursor: 0,
            late_count: 0,
            read_delay_ns: 0.0,
            late_exceeded: false,
            late_max_us: 0.0,
            read_delay_steps: 0,
            synthetic_frames: 0,
            newest_synthetic: false,
            window_blocks: 0,
            window_misses: 0,
            window_short_ns: 0.0,
            settled: incoming.kind != Kind::System,
            windows: 0,
            quiet_windows: 0,
        }
    }
    /// One delivered packet arrived `late_ns` after the mapped time of its
    /// last frame. The read delay follows the `LATE_RANK`-th latest arrival of
    /// the window (plus a margin) and only grows within this reference epoch:
    /// each growth re-anchors the read position once (AEC3 re-estimates the
    /// shifted delay), never a continuous slide.
    fn observe_late(&mut self, late_ns: f64) {
        let late_us = (late_ns.max(0.0) / 1000.0) as f32;
        self.late_max_us = self.late_max_us.max(f64::from(late_us));
        self.late_us[self.late_cursor] = late_us;
        self.late_cursor = (self.late_cursor + 1) % LATE_WINDOW;
        self.late_count = (self.late_count + 1).min(LATE_WINDOW);
        let rank = if self.late_count < 2 * LATE_RANK { 1 } else { LATE_RANK };
        let mut top = [0.0f32; LATE_RANK];
        for &value in &self.late_us[..self.late_count] {
            if value > top[LATE_RANK - 1] {
                let mut i = LATE_RANK - 1;
                while i > 0 && top[i - 1] < value {
                    top[i] = top[i - 1];
                    i -= 1;
                }
                top[i] = value;
            }
        }
        // A reference stamped ahead of its arrival (the Oink render tap) needs
        // no delay at all; only a late one gets the margin on top.
        let late = f64::from(top[rank - 1]) * 1000.0;
        let target = if late > 0.0 { late + READ_MARGIN_NS } else { 0.0 };
        self.late_exceeded = target > MAX_READ_DELAY_NS;
        if !self.late_exceeded {
            self.raise_read_delay(target);
        }
    }
    /// Grow the read delay to `target` (bounded). An established read
    /// position moves back by the same amount in one step, so jitter already
    /// absorbed by the smooth read phase is not reintroduced by re-anchoring.
    fn raise_read_delay(&mut self, target: f64) -> bool {
        let target = target.min(MAX_READ_DELAY_NS);
        if target <= self.read_delay_ns + 250_000.0 {
            return false;
        }
        let delta = target - self.read_delay_ns;
        self.read_delay_ns = target;
        self.read_delay_steps += 1;
        if let Some(phase) = &mut self.phase {
            *phase -= delta / self.clock.step_48k();
        }
        true
    }
    fn reset(&mut self) {
        for c in &mut self.converters {
            c.reset();
        }
        self.clock = Clock::new(self.rate);
        self.device_clock = Clock::new(self.rate);
        self.count = 0;
        self.first = 0;
        self.previous_native = None;
        self.phase = None;
    }
    /// At most 40 ms of native reference per wake. Missing frames are never
    /// replaced with fabricated silence, and they invalidate the AEC epoch.
    pub fn ingest(&mut self) -> Option<Reason> {
        let mut fault = None;
        for _ in 0..QUEUE {
            let Some(packet) = self.consumer.pop() else {
                break;
            };
            let discontinuity = packet.time.discontinuity
                || self.sequence.is_some_and(|n| packet.sequence != n.wrapping_add(1))
                || self
                    .previous_native
                    .zip(packet.time.native_frame)
                    .is_some_and(|(a, b)| (a - b).abs() > 0.5);
            self.sequence = Some(packet.sequence);
            if discontinuity {
                self.reset();
                fault = Some(Reason::ReferenceGap);
            }
            let rate_hint = packet.time.device_clock.and_then(|point| {
                let stamp = Time {
                    device_clock: None,
                    host_ns: Some(point.host_ns),
                    native_frame: Some(point.frame),
                    quality: TimeQuality::HardwareBoundary,
                    ..packet.time
                };
                self.device_clock
                    .observe(stamp)
                    .ok()
                    .map(|()| self.device_clock.native_step())
            });
            if packet.time.rate != self.rate
                || (packet.time.device_clock.is_some() && rate_hint.is_none())
                || self.clock.observe_rate(packet.time, rate_hint).is_err()
            {
                self.invalid_frames += packet.frames as u64;
                self.reset();
                self.last_fault = if packet.time.quality == TimeQuality::Unknown || packet.time.host_ns.is_none() {
                    Reason::TimestampUnknown
                } else {
                    Reason::ClockUnstable
                };
                fault = Some(self.last_fault);
                continue;
            }
            if self.previous_native.is_none() {
                self.origin_native = packet.time.native_frame.unwrap();
            }
            self.newest_synthetic = packet.synthetic;
            if packet.synthetic {
                self.synthetic_frames += packet.frames as u64;
            } else if self.kind == Kind::System
                && packet.delivery_end
                && let Some(host_ns) = packet.time.host_ns
            {
                // Only the system feed stamps on the shared estimated host
                // epoch and arrives after its stamp; the Oink render tap is
                // stamped ahead of presentation (phones on their own native
                // epoch) and never needs a read delay.
                let end_ns = host_ns as f64 + packet.frames as f64 * 1e9 / f64::from(self.rate);
                self.observe_late(super::estimated_host_ns(packet.time.delivered_at) as f64 - end_ns);
            }
            self.previous_native = packet.time.native_frame.map(|f| f + packet.frames as f64);
            for i in 0..packet.frames {
                if packet.samples.iter().any(|s| !s[i].is_finite()) {
                    self.invalid_frames += (packet.frames - i) as u64;
                    self.reset();
                    fault = Some(Reason::Nonfinite);
                    break;
                }
                self.valid_frames += 1;
                // A 8 kHz input emits at most six 48 kHz samples. Both identical
                // phase converters emit the same count; fixed scratch is enough.
                let mut left = [0.0; 8];
                let mut len = 0;
                self.converters[0].push(packet.samples[0][i], |s| {
                    left[len] = s;
                    len += 1;
                });
                let mut right = [0.0; 8];
                let mut right_len = 0;
                self.converters[1].push(packet.samples[1][i], |s| {
                    right[right_len] = s;
                    right_len += 1;
                });
                debug_assert_eq!(len, right_len);
                for j in 0..len {
                    self.history[self.count as usize % HISTORY] = [left[j], right[j]];
                    self.count += 1;
                    self.first = self.count.saturating_sub(HISTORY as u64);
                }
            }
        }
        if let Some(reason) = fault {
            self.last_fault = reason;
        }
        fault
    }
    /// Capture is the master timebase. Drift changes only the reference read
    /// phase; the microphone converter never chases the output clock.
    pub fn block(&mut self, host_ns: u64, capture_step_ns: f64, out: &mut [[f32; BLOCK]; 2]) -> Result<(), Reason> {
        if !self.clock.known() || self.count < 64 {
            return Err(self.last_fault);
        }
        if self.window_blocks >= LATE_WINDOW as u32 {
            // Before this block's read position is derived, so the step moves
            // the established phase and the new target together.
            let raised = self.window_misses >= LATE_RANK as u32 && {
                let target = self.read_delay_ns + self.window_short_ns + READ_MARGIN_NS;
                self.raise_read_delay(target)
            };
            if !self.settled {
                self.windows += 1;
                self.quiet_windows = if raised { 0 } else { self.quiet_windows + 1 };
                // At most eight windows (about 5 s) of measuring first.
                self.settled = self.quiet_windows >= 1 && self.windows >= 2 || self.windows >= 8;
            }
            self.window_blocks = 0;
            self.window_misses = 0;
            self.window_short_ns = 0.0;
        }
        let read_ns = host_ns as f64 - self.read_delay_ns;
        let desired = (self.clock.frame(read_ns) - self.origin_native) * 48_000.0 / f64::from(self.rate);
        let ratio = capture_step_ns / self.clock.step_48k();
        if !(0.9975..=1.0025).contains(&ratio) {
            return Err(Reason::ClockUnstable);
        }
        let previous = self.phase.unwrap_or(desired);
        self.alignment_error_frames = desired - previous;
        if self.alignment_error_frames.abs() > 480.0 {
            self.phase = None;
            return Err(Reason::ClockUnstable);
        }
        let phase = if self.phase.is_none() {
            desired
        } else {
            previous + self.alignment_error_frames.clamp(-0.1, 0.1)
        };
        let step = self.step + (ratio - self.step).clamp(-0.00001, 0.00001);
        let end = phase + (BLOCK - 1) as f64 * step;
        if self.late_exceeded {
            // Out of the causal range (see MAX_READ_DELAY_NS): bypass this
            // block honestly and keep the timeline as for a missing block.
            self.phase = Some(phase + BLOCK as f64 * step);
            self.step = step;
            return Err(Reason::ReferenceLate);
        }
        let not_yet = phase.floor() - 15.0 >= self.first as f64 && end.floor() + 16.0 >= self.count as f64;
        let measured = self.kind == Kind::System && self.phase.is_some() && !self.newest_synthetic;
        if measured {
            // Reads that run past delivered system audio measure what the
            // producer-side lateness cannot see (capture stamp jitter, the
            // interpolation span): grow the read delay by the shortfall when
            // it recurs (`LATE_RANK` of 64 blocks), never for a single block.
            self.window_blocks += 1;
            if not_yet {
                self.window_misses += 1;
                let short = (end.floor() + 17.0 - self.count as f64) * self.clock.step_48k();
                self.window_short_ns = self.window_short_ns.max(short);
            }
        }
        if !self.settled {
            self.phase = Some(phase + BLOCK as f64 * step);
            self.step = step;
            return Err(Reason::ReferenceMissing);
        }
        if phase.floor() - 15.0 < self.first as f64 || end.floor() + 16.0 >= self.count as f64 {
            // Product capture owners cannot hold a block (desktop runs in the
            // native capture callback), so this capture block is bypassed.
            // Advance an established render timeline by the same block so the
            // engine keeps one consistent render/capture alignment afterwards.
            if self.phase.is_some() {
                self.phase = Some(phase + BLOCK as f64 * step);
                self.step = step;
            }
            // At the causal bound a late delivery is reported as such.
            return Err(if not_yet && self.kind == Kind::System && self.newest_synthetic {
                // Idle endpoint: silence is declared only behind the delivery
                // margin, so the newest span is not known yet. Nothing plays.
                Reason::ReferenceIdle
            } else if not_yet && measured && self.read_delay_ns >= MAX_READ_DELAY_NS - READ_MARGIN_NS {
                Reason::ReferenceLate
            } else {
                Reason::ReferenceMissing
            });
        }
        for i in 0..BLOCK {
            let at = phase + i as f64 * step;
            let center = at.floor() as u64;
            let coefficients = &self.coefficients[((at.fract() * PHASES as f64) as usize).min(PHASES - 1)];
            for (channel, destination) in out.iter_mut().enumerate() {
                destination[i] = coefficients
                    .iter()
                    .enumerate()
                    .map(|(tap, c)| self.history[(center - 15 + tap as u64) as usize % HISTORY][channel] * c)
                    .sum();
            }
        }
        self.phase = Some(phase + BLOCK as f64 * step);
        self.step = step;
        Ok(())
    }
    pub fn reset_timeline(&mut self) {
        self.phase = None;
    }
    pub fn ratio(&self) -> f64 {
        self.step
    }
}
