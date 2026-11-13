//! Full-system echo reference (desktop only).
//!
//! The reference is the whole mix of the physical output that Oink plays to:
//! macOS records it with a process tap of that output device that includes
//! every process (Oink too), Windows with WASAPI loopback of the output
//! endpoint. Oink's forwarded audio and every other local program are both in
//! it, so one AEC removes both. It replaces the Oink render reference while it
//! runs; the two are never added (Oink's part would be subtracted twice).
//!
//! The capture thread only converts timestamps and copies PCM into the fixed
//! reference handoff (`RenderTap`); AEC runs on the microphone DSP owner as for
//! the Oink reference. When the capture cannot start or stops, `Control`
//! selects the Oink render reference again and records why (`SystemStatus`).
use super::{
    CHUNK, Control, SystemStatus, Time, TimeQuality, estimated_host_ns,
    processor::cpu_ns,
    reference::{RenderTap, reference_bytes},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Stamp jitter accepted without a native position to confirm continuity.
const STAMP_TOLERANCE_NS: f64 = 4_000_000.0;
/// With a continuous native position, only a stamp jump this large is a
/// delivery gap (an endpoint that stopped rendering keeps its position).
const POSITIONED_GAP_NS: f64 = 20_000_000.0;
/// Larger gaps start a new reference timeline instead of being filled.
const MAX_FILL_NS: f64 = 100_000_000.0;

/// One delivered native packet of the output mix, interleaved stereo.
#[derive(Clone, Copy, Debug)]
pub struct SystemPacket<'a> {
    pub samples: &'a [f32],
    /// First frame on the process host clock (mach / QPC mapped to `Instant`).
    /// An estimate of when the mix is handed to the device, not a DAC time.
    /// None when the platform flagged a timestamp error.
    pub stamp: Option<Instant>,
    /// Native frame position of the first frame, when the platform has one.
    pub position: Option<u64>,
    pub discontinuity: bool,
    pub arrived: Instant,
}

/// Producer side of the system reference: maps native stamps to the shared
/// host clock that also stamps microphone blocks, keeps one continuous native
/// frame line for the reference clock fit, and turns a healthy endpoint that
/// delivers nothing into known silence. Runs on the capture thread with fixed
/// storage only.
pub struct SystemFeed {
    tap: RenderTap,
    rate: u32,
    gaps_are_silence: bool,
    idle_margin_ns: f64,
    next_frame: u64,
    end_ns: Option<f64>,
    expected_position: Option<u64>,
    last_arrival_ns: f64,
    idle: bool,
    zeros: [f32; CHUNK * 2],
    cpu_mark: (Instant, u64),
    /// Delivered packets that overlapped audio already declared silent.
    pub overlaps: u64,
}
impl std::fmt::Debug for SystemFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemFeed")
            .field("rate", &self.rate)
            .field("idle", &self.idle)
            .finish_non_exhaustive()
    }
}
impl SystemFeed {
    /// Registers the system reference with its native rate and reports it
    /// running. `gaps_are_silence`: the platform stops delivering while the
    /// output renders nothing (WASAPI loopback) rather than delivering zeros
    /// (macOS tap). `idle_margin` must exceed the platform's delivery lag, so
    /// silence is never declared for a span whose packet is still on its way.
    /// `native_bytes` is the platform's own fixed capture storage.
    pub fn new(
        control: Arc<Control>,
        rate: u32,
        gaps_are_silence: bool,
        idle_margin: Duration,
        native_bytes: usize,
    ) -> Self {
        let now = Instant::now();
        control.system_bytes.store(
            (native_bytes + reference_bytes() + std::mem::size_of::<Self>()) as u64,
            Ordering::Relaxed,
        );
        let tap = RenderTap::system(control.clone(), rate);
        control.set_system_status(SystemStatus::Running, None, now);
        Self {
            tap,
            rate,
            gaps_are_silence,
            idle_margin_ns: idle_margin.as_nanos() as f64,
            next_frame: 0,
            end_ns: None,
            expected_position: None,
            last_arrival_ns: 0.0,
            idle: false,
            zeros: [0.0; CHUNK * 2],
            cpu_mark: (now, cpu_ns()),
            overlaps: 0,
        }
    }

    /// The platform could not provide the system reference; Oink's render
    /// reference is used and `detail` is kept for the diagnostic log.
    pub fn unavailable(control: &Control, status: SystemStatus, detail: String) {
        control.set_system_status(status, Some(detail), Instant::now());
    }

    /// The running capture stopped: unregister and record why.
    pub fn fail(self, status: SystemStatus, detail: String) {
        let control = self.tap.control().clone();
        drop(self);
        control.set_system_status(status, Some(detail), Instant::now());
    }

    fn rate_f(&self) -> f64 {
        f64::from(self.rate)
    }
    fn supply(&mut self, samples: &[f32], host_ns: Option<f64>, discontinuity: bool, synthetic: bool, at: Instant) {
        let frames = samples.len() / 2;
        self.tap.supply_packets(
            samples,
            2,
            Time {
                host_ns: host_ns.map(|ns| ns.max(1.0) as u64),
                device_clock: None,
                native_frame: Some(self.next_frame as f64),
                rate: self.rate,
                // Tap/loopback stamps are pre-output estimates; the residual
                // delay to the speaker is left to AEC3's delay estimator.
                quality: if host_ns.is_some() {
                    TimeQuality::Estimated
                } else {
                    TimeQuality::Unknown
                },
                discontinuity,
                delivered_at: at,
            },
            synthetic,
        );
        self.next_frame += frames as u64;
    }
    /// Known silence from the current end of the reference to `to_ns`.
    fn fill_silence(&mut self, to_ns: f64, at: Instant) {
        let Some(mut end) = self.end_ns else {
            return;
        };
        let mut frames = ((to_ns - end) * self.rate_f() / 1e9).floor().max(0.0) as usize;
        while frames > 0 {
            let count = frames.min(CHUNK);
            let zeros = self.zeros;
            self.supply(&zeros[..count * 2], Some(end), false, true, at);
            end += count as f64 * 1e9 / self.rate_f();
            frames -= count;
        }
        self.end_ns = Some(end);
    }

    /// One delivered packet. Copies only; never blocks or allocates.
    pub fn packet(&mut self, packet: SystemPacket<'_>) {
        let frames = packet.samples.len() / 2;
        if frames == 0 {
            return;
        }
        self.last_arrival_ns = estimated_host_ns(packet.arrived) as f64;
        let Some(stamp) = packet.stamp else {
            // No usable time: the reference rejects it (timestamp unknown) and
            // the next stamped packet starts a new timeline.
            self.supply(packet.samples, None, true, false, packet.arrived);
            self.end_ns = None;
            self.expected_position = None;
            self.idle = false;
            return;
        };
        let stamp_ns = estimated_host_ns(stamp) as f64;
        let mut discontinuity = packet.discontinuity;
        if let Some(end) = self.end_ns {
            let gap_ns = stamp_ns - end;
            // After an idle stretch the silence was filled by time; whether
            // the endpoint position advanced meanwhile is driver behaviour,
            // so only the stamp decides there.
            let jump = packet
                .position
                .zip(self.expected_position)
                .filter(|_| !self.idle)
                .map(|(position, expected)| position as i128 - expected as i128);
            let threshold = if jump == Some(0) {
                POSITIONED_GAP_NS
            } else {
                STAMP_TOLERANCE_NS
            };
            if gap_ns < -threshold || jump.is_some_and(|j| j < 0) {
                // Audio for a span already declared silent (or a rewound
                // device clock): never overwrite history, start a new epoch.
                self.overlaps += 1;
                discontinuity = true;
            } else if gap_ns > threshold || jump.is_some_and(|j| j > 0) {
                let gap_ns = if gap_ns > threshold {
                    gap_ns
                } else {
                    jump.unwrap_or(0) as f64 * 1e9 / self.rate_f()
                };
                // WASAPI loopback delivers nothing while the endpoint renders
                // nothing and may flag the first packet after that pause as a
                // discontinuity. After an observed idle stretch, a gap that
                // starts at our end is that silence, not lost audio; without
                // the idle observation a flagged packet stays a new epoch.
                let silent_gap = self.gaps_are_silence && (!packet.discontinuity || self.idle);
                if silent_gap && gap_ns <= MAX_FILL_NS {
                    self.fill_silence(stamp_ns, packet.arrived);
                    discontinuity = false;
                } else {
                    discontinuity = true;
                }
            } else if packet.discontinuity && self.idle && self.gaps_are_silence {
                discontinuity = false;
            }
        }
        self.supply(packet.samples, Some(stamp_ns), discontinuity, false, packet.arrived);
        self.end_ns = Some(stamp_ns + frames as f64 * 1e9 / self.rate_f());
        self.expected_position = packet.position.map(|p| p + frames as u64);
        self.idle = false;
    }

    /// The capture is healthy but nothing arrived. On a platform whose
    /// endpoint stops delivering while silent, everything older than the
    /// delivery margin is known silence: keep the reference line continuous
    /// instead of treating the pause as a capture failure.
    pub fn idle(&mut self, now: Instant) {
        if !self.gaps_are_silence || self.end_ns.is_none() {
            return;
        }
        let now_ns = estimated_host_ns(now) as f64;
        if now_ns - self.last_arrival_ns < self.idle_margin_ns {
            return;
        }
        self.idle = true;
        self.fill_silence(now_ns - self.idle_margin_ns, now);
    }

    /// Accounts this thread's CPU time once per second for the diagnostics.
    pub fn account(&mut self, now: Instant) {
        if now.saturating_duration_since(self.cpu_mark.0) < Duration::from_secs(1) {
            return;
        }
        let cpu = cpu_ns();
        self.tap
            .control()
            .system_cpu_us
            .fetch_add(cpu.saturating_sub(self.cpu_mark.1) / 1000, Ordering::Relaxed);
        self.cpu_mark = (now, cpu);
    }
    /// Whether the processor currently wants this reference (diagnostics).
    pub fn selected(&self) -> bool {
        self.tap.wanted()
    }
}

/// Owner handle of the platform capture thread. Started by the microphone
/// owner only while this device is the audio exit, the shared microphone
/// source, and echo cancellation is on; dropping it stops the recording.
#[derive(Debug)]
pub struct SystemReference {
    control: Arc<Control>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl SystemReference {
    /// Never blocks the owner on the platform open: a worker opens the
    /// capture of `output_id` (the playback's native output id) and reports
    /// running or the reason it cannot through `Control`.
    pub fn start(control: Arc<Control>, output_id: &str) -> Self {
        control.set_system_status(SystemStatus::Starting, None, Instant::now());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_control = control.clone();
        let worker_stop = stop.clone();
        let id = output_id.to_owned();
        let worker = std::thread::Builder::new()
            .name("oink-echo-reference".into())
            .spawn(move || {
                if let Err((status, detail)) = run(&worker_control, &id, &worker_stop) {
                    tracing::warn!(?status, %detail, "full-system echo reference unavailable; using the Oink playback reference");
                    worker_control.set_system_status(status, Some(detail), Instant::now());
                }
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                SystemFeed::unavailable(
                    &control,
                    SystemStatus::StartFailed,
                    format!("spawn reference worker: {error}"),
                );
                None
            }
        };
        Self { control, stop, worker }
    }
}
impl Drop for SystemReference {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.control
            .set_system_status(SystemStatus::NotRequested, None, Instant::now());
    }
}

#[cfg(target_os = "macos")]
fn run(control: &Arc<Control>, output_id: &str, stop: &AtomicBool) -> Result<(), (SystemStatus, String)> {
    crate::macos::run_system_reference(control, output_id, stop)
}
#[cfg(windows)]
fn run(control: &Arc<Control>, output_id: &str, stop: &AtomicBool) -> Result<(), (SystemStatus, String)> {
    crate::windows::run_system_reference(control, output_id, stop)
}
#[cfg(not(any(target_os = "macos", windows)))]
fn run(_: &Arc<Control>, _: &str, _: &AtomicBool) -> Result<(), (SystemStatus, String)> {
    Err((
        SystemStatus::Unsupported,
        "no system output capture on this platform".into(),
    ))
}
