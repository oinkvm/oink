use super::{
    BLOCK, Control, ENGINE_DELAY, Reason, ReferenceSource, State, SystemStatus, Time, TimeQuality, WorkerControl,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Transition {
    pub state: State,
    pub reason: Reason,
    pub at_us: u64,
}
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Summary {
    pub state: State,
    pub reason: Reason,
    pub epoch: u64,
    pub output_epoch: u64,
    /// Which reference is in use, and why the full-system one is (not).
    pub reference_source: ReferenceSource,
    pub system_status: SystemStatus,
    /// The read delay behind the capture stamp, its growth steps, and the
    /// latest reference arrival after its mapped time in this window.
    pub reference_read_delay_us: u64,
    pub reference_read_delay_steps: u64,
    pub reference_late_max_us: u64,
    /// Blocks bypassed because the reference arrives too late to be causal.
    pub reference_late_blocks: u64,
    /// Silence filled for an idle (non-delivering) loopback endpoint.
    pub reference_silence_frames: u64,
    /// System reference capture worker: thread CPU in this window and its
    /// fixed allocation (native ring, handoff queue, reference history).
    pub system_cpu_us: u64,
    pub system_bytes: u64,
    pub window_ms: u64,
    pub blocks: u64,
    pub processed_blocks: u64,
    pub bypass_blocks: u64,
    pub reference_valid_frames: u64,
    pub reference_invalid_frames: u64,
    pub reference_dropped_frames: u64,
    pub capture_quality: TimeQuality,
    pub reference_quality: TimeQuality,
    pub capture_ppm: f64,
    pub render_ppm: f64,
    pub alignment_error_us: f64,
    pub asrc_ratio: f64,
    /// Qualified linear ERL/ERLE: only in conservative far-end-only windows
    /// (aligned, unclipped, quiet near end). The engine's own latest metrics
    /// are `aec3_erl_db`/`aec3_erle_db`.
    pub erl_db: Option<f64>,
    pub erle_db: Option<f64>,
    /// AEC3's echo-path delay estimate (its internal delay estimator).
    pub internal_delay_ms: Option<i32>,
    /// AEC3's own latest ERL/ERLE metrics on a processed block, unqualified.
    pub aec3_erl_db: Option<f64>,
    pub aec3_erle_db: Option<f64>,
    /// Sums of squares and peaks for the window's levels: the microphone as
    /// captured (`capture`), the reference blocks given to AEC3 (`render`,
    /// mean of both channels, over `reference_blocks`) and what was sent
    /// (`output`). Levels in dBFS are derived by the log, not here.
    pub capture_energy: f64,
    pub capture_peak: f32,
    pub render_energy: f64,
    pub render_peak: f32,
    pub reference_blocks: u64,
    pub output_energy: f64,
    pub output_peak: f32,
    /// Processed blocks whose reference is active (above -70 dBFS), and the
    /// capture/output energy of exactly those blocks: the attenuation AEC
    /// actually applies while the speaker plays, independent of AEC3's own
    /// estimates (near-end talk in these blocks lowers it).
    pub render_active_blocks: u64,
    pub active_capture_energy: f64,
    pub active_output_energy: f64,
    /// Processed blocks whose linear-filter error exceeded the capture by
    /// 3 dB (a diverged or mismatched linear filter).
    pub diverged_blocks: u64,
    /// Capture discontinuities (lost or refused frames) handled without a new
    /// engine: the read position re-anchors, the adapted filter stays.
    pub capture_gaps: u64,
    /// Why the last engine epoch in this window replaced the previous one.
    pub reset_reason: Reason,
    /// Engines the control has built so far (a steady configuration: one).
    pub engines_built: u64,
    pub cpu_us: u64,
    pub block_cpu_max_us: u64,
    pub block_work_max_us: u64,
    pub block_work_p99_us: u64,
    pub deadline_misses: u64,
    pub resets: u64,
    pub clipped_samples: u64,
    pub nonfinite_samples: u64,
    pub wait_max_us: u64,
    pub transition_count: usize,
    pub transitions: [Transition; 16],
    pub transitions_dropped: u64,
}
impl Default for Summary {
    fn default() -> Self {
        Self {
            state: State::Armed,
            reason: Reason::None,
            epoch: 0,
            output_epoch: 0,
            reference_source: ReferenceSource::None,
            system_status: SystemStatus::NotRequested,
            reference_read_delay_us: 0,
            reference_read_delay_steps: 0,
            reference_late_max_us: 0,
            reference_late_blocks: 0,
            reference_silence_frames: 0,
            system_cpu_us: 0,
            system_bytes: 0,
            window_ms: 0,
            blocks: 0,
            processed_blocks: 0,
            bypass_blocks: 0,
            reference_valid_frames: 0,
            reference_invalid_frames: 0,
            reference_dropped_frames: 0,
            capture_quality: TimeQuality::Unknown,
            reference_quality: TimeQuality::Unknown,
            capture_ppm: 0.0,
            render_ppm: 0.0,
            alignment_error_us: 0.0,
            asrc_ratio: 1.0,
            erl_db: None,
            erle_db: None,
            internal_delay_ms: None,
            aec3_erl_db: None,
            aec3_erle_db: None,
            capture_energy: 0.0,
            capture_peak: 0.0,
            render_energy: 0.0,
            render_peak: 0.0,
            reference_blocks: 0,
            output_energy: 0.0,
            output_peak: 0.0,
            render_active_blocks: 0,
            active_capture_energy: 0.0,
            active_output_energy: 0.0,
            diverged_blocks: 0,
            capture_gaps: 0,
            reset_reason: Reason::None,
            engines_built: 0,
            cpu_us: 0,
            block_cpu_max_us: 0,
            block_work_max_us: 0,
            block_work_p99_us: 0,
            deadline_misses: 0,
            resets: 0,
            clipped_samples: 0,
            nonfinite_samples: 0,
            wait_max_us: 0,
            transition_count: 0,
            transitions: [Transition::default(); 16],
            transitions_dropped: 0,
        }
    }
}
impl Summary {
    /// Fold a later window into this one (the owner's log aggregates several
    /// processor windows into one line): counters add, maxima stay maximal,
    /// the newest state and estimates win.
    pub fn merge(&mut self, next: &Summary) {
        let transitions = self.transitions;
        let count = self.transition_count;
        let dropped = self.transitions_dropped;
        let mut merged = *next;
        merged.window_ms += self.window_ms;
        merged.blocks += self.blocks;
        merged.processed_blocks += self.processed_blocks;
        merged.bypass_blocks += self.bypass_blocks;
        merged.reference_valid_frames += self.reference_valid_frames;
        merged.reference_invalid_frames += self.reference_invalid_frames;
        merged.reference_dropped_frames += self.reference_dropped_frames;
        merged.reference_late_max_us = merged.reference_late_max_us.max(self.reference_late_max_us);
        merged.reference_late_blocks += self.reference_late_blocks;
        merged.reference_silence_frames += self.reference_silence_frames;
        merged.system_cpu_us += self.system_cpu_us;
        merged.erl_db = next.erl_db.or(self.erl_db);
        merged.erle_db = next.erle_db.or(self.erle_db);
        merged.internal_delay_ms = next.internal_delay_ms.or(self.internal_delay_ms);
        merged.aec3_erl_db = next.aec3_erl_db.or(self.aec3_erl_db);
        merged.aec3_erle_db = next.aec3_erle_db.or(self.aec3_erle_db);
        merged.capture_energy += self.capture_energy;
        merged.capture_peak = merged.capture_peak.max(self.capture_peak);
        merged.render_energy += self.render_energy;
        merged.render_peak = merged.render_peak.max(self.render_peak);
        merged.reference_blocks += self.reference_blocks;
        merged.output_energy += self.output_energy;
        merged.output_peak = merged.output_peak.max(self.output_peak);
        merged.render_active_blocks += self.render_active_blocks;
        merged.active_capture_energy += self.active_capture_energy;
        merged.active_output_energy += self.active_output_energy;
        merged.diverged_blocks += self.diverged_blocks;
        merged.capture_gaps += self.capture_gaps;
        if next.resets == 0 {
            merged.reset_reason = self.reset_reason;
        }
        merged.cpu_us += self.cpu_us;
        merged.block_cpu_max_us = merged.block_cpu_max_us.max(self.block_cpu_max_us);
        merged.block_work_max_us = merged.block_work_max_us.max(self.block_work_max_us);
        merged.block_work_p99_us = merged.block_work_p99_us.max(self.block_work_p99_us);
        merged.deadline_misses += self.deadline_misses;
        merged.resets += self.resets;
        merged.clipped_samples += self.clipped_samples;
        merged.nonfinite_samples += self.nonfinite_samples;
        merged.wait_max_us = merged.wait_max_us.max(self.wait_max_us);
        // Older transitions first, then the newer window's.
        merged.transitions = transitions;
        merged.transition_count = count;
        merged.transitions_dropped = dropped + next.transitions_dropped;
        for event in &next.transitions[..next.transition_count] {
            if merged.transition_count < merged.transitions.len() {
                merged.transitions[merged.transition_count] = *event;
                merged.transition_count += 1;
            } else {
                merged.transitions_dropped += 1;
            }
        }
        *self = merged;
    }
}
/// Single-owner, fixed-buffer AEC adapter. It neither creates microphone demand
/// nor owns a transport. Its output feeds the existing one encoder/fanout.
///
/// Whenever the engine is engaged (eligible, an engine and a valid reference
/// block) the processed AEC3 output is sent, from the first block on: AEC3's
/// residual echo suppressor acts before its linear filter has converged
/// (it then assumes a strong echo path wherever the reference is active, at
/// the delay its render delay controller finds within roughly the first
/// 100 ms of render activity). Raw microphone audio is sent only for blocks
/// that cannot be processed (`Degraded`) and when AEC is not applicable
/// (`Off`/`Armed`).
pub struct Processor {
    control: Option<WorkerControl>,
    raw_history: [f32; ENGINE_DELAY],
    raw_cursor: usize,
    latency_mix: f32,
    wet_mix: f32,
    last_output: f32,
    splice: Option<f32>,
    state: State,
    reason: Reason,
    /// The active engine needs replacement; a reset was requested once.
    untrusted: bool,
    /// The active engine has adapted on at least one block since adoption. A
    /// fault before that cannot have trained a wrong model, so it bypasses the
    /// block without asking for another engine (no rebuild loop while the
    /// reference stays invalid).
    engine_dirty: bool,
    /// This block must be bypassed (a fault was observed in this cycle).
    skip_block: bool,
    far_blocks: u32,
    confident_blocks: u32,
    /// The linear filter of the current engine has shown sustained coupling
    /// (diagnostic `Active`; the output is the same as in `Converging`).
    converged: bool,
    last_erle: Option<f64>,
    summary: Summary,
    window: Instant,
    origin: Instant,
    work_histogram: [u64; 8],
    #[cfg(any(test, feature = "test-support"))]
    pub(super) test_work_budget: Option<Duration>,
}
impl std::fmt::Debug for Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoProcessor")
            .field("state", &self.state)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl Processor {
    pub fn new(control: Option<Arc<Control>>, now: Instant) -> Result<Self, String> {
        Ok(Self {
            control: control.map(WorkerControl::new).transpose()?,
            raw_history: [0.0; ENGINE_DELAY],
            raw_cursor: 0,
            latency_mix: 0.0,
            wet_mix: 0.0,
            last_output: 0.0,
            splice: None,
            state: State::Armed,
            reason: Reason::None,
            untrusted: false,
            engine_dirty: false,
            skip_block: false,
            far_blocks: 0,
            confident_blocks: 0,
            converged: false,
            last_erle: None,
            summary: Summary::default(),
            window: now,
            origin: now,
            work_histogram: [0; 8],
            #[cfg(any(test, feature = "test-support"))]
            test_work_budget: None,
        })
    }
    fn transition(&mut self, state: State, reason: Reason, now: Instant) {
        if self.state == state && self.reason == reason {
            return;
        }
        self.state = state;
        self.reason = reason;
        let event = Transition {
            state,
            reason,
            at_us: now.saturating_duration_since(self.origin).as_micros() as u64,
        };
        if self.summary.transition_count < self.summary.transitions.len() {
            self.summary.transitions[self.summary.transition_count] = event;
            self.summary.transition_count += 1;
        } else {
            self.summary.transitions_dropped += 1;
        }
    }
    /// Drain/reference conversion and update adoption are bounded. Reset/create
    /// is requested via atomics and performed by Control::update, never here.
    pub fn prepare(&mut self, now: Instant) {
        let cpu = cpu_ns();
        let mut fault = None;
        let mut adopted = false;
        if let Some(control) = &mut self.control {
            if control.apply() {
                adopted = true;
                self.untrusted = false;
                self.engine_dirty = false;
                self.far_blocks = 0;
                self.confident_blocks = 0;
                self.converged = false;
                self.last_erle = None;
                self.summary.erl_db = None;
                self.summary.erle_db = None;
                self.summary.internal_delay_ms = None;
                self.summary.resets += 1;
                self.summary.reset_reason = control.active.as_ref().map_or(Reason::None, |a| a.cause);
            }
            if let Some(active) = &mut control.active {
                self.summary.epoch = active.epoch;
                if let Some(reference) = &mut active.reference {
                    fault = reference.ingest();
                    self.summary.output_epoch = reference.generation;
                }
            }
        }
        if adopted {
            // A fresh filter has no coupling evidence, even when the previous
            // epoch was active. Its output is still sent (see `process`).
            self.transition(State::Converging, Reason::None, now);
        }
        if let Some(reason) = fault {
            self.invalidate(reason, now);
        }
        self.summary.cpu_us += cpu_ns().saturating_sub(cpu) / 1000;
    }
    pub fn enabled(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|c| c.control.mode() == 2 && c.control.output() != 0)
    }
    pub fn invalidate(&mut self, reason: Reason, now: Instant) {
        if self.engine_dirty && !self.untrusted {
            if let Some(control) = &self.control {
                control.control.request_reset(reason);
            }
            self.untrusted = true;
        }
        self.skip_block = true;
        self.confident_blocks = 0;
        self.last_erle = None;
        self.summary.erl_db = None;
        self.summary.erle_db = None;
        self.transition(State::Degraded, reason, now);
    }
    fn reference_mut(&mut self) -> Option<&mut super::reference::Reference> {
        self.control.as_mut()?.active.as_mut()?.reference.as_mut()
    }
    #[allow(dead_code)]
    pub(crate) fn capture_nonfinite(&mut self, count: u64, now: Instant) {
        self.summary.nonfinite_samples += count;
        self.invalidate(Reason::Nonfinite, now);
    }
    /// Always writes `out`: the processed block whenever the engine is engaged,
    /// or the delay-matched raw block when this block cannot be processed.
    /// Product capture owners cannot hold a block, so a late reference never
    /// becomes silence.
    pub fn process(
        &mut self,
        mic: &[f32; BLOCK],
        time: Time,
        capture_step_ns: f64,
        out: &mut [f32; BLOCK],
        now: Instant,
    ) {
        let began = Instant::now();
        let cpu = cpu_ns();
        let mut processed = [0.0; BLOCK];
        let mut use_processed = false;
        let mut render_active = false;
        let eligible = self.enabled();
        self.summary.capture_quality = time.quality;
        self.summary.capture_ppm = (1e9 / 48_000.0 / capture_step_ns - 1.0) * 1e6;
        let age = now.saturating_duration_since(time.delivered_at);
        self.summary.wait_max_us = self.summary.wait_max_us.max(age.as_micros() as u64);
        if time.discontinuity {
            // Lost capture frames (a refused/overflowing handoff, a native
            // gap). The echo path and the adapted filter are unchanged and
            // this block is stamped where it was really captured, so only the
            // reference read position re-anchors to it; a phase servo that
            // moves 0.1 frame per block would otherwise lag by the gap. No new
            // engine: a restarted stream gets its own processor and epoch.
            self.summary.capture_gaps += 1;
            if let Some(reference) = self.reference_mut() {
                reference.reset_timeline();
            }
        }
        let clipped = mic.iter().filter(|s| s.abs() >= 0.999).count() as u64;
        let nonfinite = mic.iter().filter(|s| !s.is_finite()).count() as u64;
        if nonfinite != 0 {
            self.invalidate(Reason::Nonfinite, now);
        }
        if eligible && (time.host_ns.is_none() || time.quality == TimeQuality::Unknown) {
            self.invalidate(Reason::TimestampUnknown, now);
        }
        if eligible && age >= Duration::from_millis(20) {
            self.summary.deadline_misses += 1;
            self.invalidate(Reason::Deadline, now);
        }
        if eligible && !self.untrusted && !self.skip_block {
            let output_epoch = self.control.as_ref().map_or(0, |c| c.control.output());
            let ready = self
                .control
                .as_mut()
                .and_then(|c| c.active.as_mut())
                .filter(|a| a.reference.as_ref().is_some_and(|r| r.generation == output_epoch))
                .and_then(|active| Some((active.engine.as_mut()?, active.reference.as_mut()?)));
            if let Some((engine, reference)) = ready {
                let mut reverse = [[0.0; BLOCK]; 2];
                let reference_result = reference.block(time.host_ns.unwrap(), capture_step_ns, &mut reverse);
                match reference_result {
                    // Not a fault of the engine or of the timeline: bypass
                    // this one block, the render timeline already skipped it.
                    Err(reason @ (Reason::ReferenceMissing | Reason::ReferenceIdle)) => {
                        self.transition(State::Degraded, reason, now);
                    }
                    // Recorded, not a fault: the timeline continues and the
                    // engine keeps its model for when arrival is causal again.
                    Err(Reason::ReferenceLate) => {
                        self.summary.reference_late_blocks += 1;
                        self.transition(State::Degraded, Reason::ReferenceLate, now);
                    }
                    Err(reason) => self.invalidate(reason, now),
                    Ok(()) => {
                        let far_power =
                            reverse.iter().flatten().map(|s| f64::from(s * s)).sum::<f64>() / (BLOCK * 2) as f64;
                        let far_peak = reverse.iter().flatten().fold(0.0f32, |peak, s| peak.max(s.abs()));
                        let mut linear = [0.0; 160];
                        engine.process(&reverse, mic, &mut processed, &mut linear, false);
                        self.engine_dirty = true;
                        let (erl, erle, delay) = engine.metrics();
                        let (capture_power, error_power) = engine.linear_powers();
                        self.summary.processed_blocks += 1;
                        self.summary.reference_blocks += 1;
                        self.summary.render_energy += far_power * BLOCK as f64;
                        self.summary.render_peak = self.summary.render_peak.max(far_peak);
                        render_active = far_power > 1e-7;
                        if render_active {
                            self.far_blocks = self.far_blocks.saturating_add(1);
                        }
                        // Six dB of sustained linear reduction is coupling evidence, not
                        // an advertised ERLE target. Long recorded room tails and
                        // double-talk can remain useful below a 10 dB gate. It only
                        // labels the state; the processed output is sent either way.
                        let coupled = erle.is_finite() && erle >= 6.0 && erl.is_finite() && erl > -25.0;
                        if coupled {
                            self.confident_blocks = self.confident_blocks.saturating_add(1);
                        } else {
                            self.confident_blocks = 0;
                        }
                        self.summary.internal_delay_ms = Some(delay);
                        self.summary.aec3_erl_db = erl.is_finite().then_some(erl);
                        self.summary.aec3_erle_db = erle.is_finite().then_some(erle);
                        if capture_power > 1e-10 && error_power > 2.0 * capture_power {
                            self.summary.diverged_blocks += 1;
                        }
                        // ERLE is an engine estimate, only reported in conservative
                        // far-end-only windows: same-rate aligned linear reduction,
                        // valid excitation and no clipping. Silence is never infinity.
                        let quiet_near =
                            capture_power > 1e-10 && error_power > 1e-15 && capture_power / error_power > 10.0;
                        if coupled && render_active && clipped == 0 && quiet_near {
                            self.summary.erl_db = Some(erl);
                            self.summary.erle_db = Some(erle);
                            self.last_erle = Some(erle);
                        } else {
                            self.summary.erl_db = None;
                            self.summary.erle_db = None;
                        }
                        self.summary.reference_quality = reference.clock.quality;
                        self.summary.render_ppm = reference.clock.ppm();
                        self.summary.alignment_error_us = reference.alignment_error_frames * 1e6 / 48_000.0;
                        self.summary.asrc_ratio = reference.ratio();
                        if self.confident_blocks >= 30 && self.far_blocks >= 50 {
                            self.converged = true;
                        }
                        use_processed = true;
                        self.transition(
                            if self.converged {
                                State::Active
                            } else {
                                State::Converging
                            },
                            Reason::None,
                            now,
                        );
                        if processed.iter().any(|s| !s.is_finite()) {
                            self.summary.nonfinite_samples += 1;
                            use_processed = false;
                            self.invalidate(Reason::Nonfinite, now);
                        }
                    }
                }
            } else {
                self.transition(State::Degraded, Reason::Rebuilding, now);
            }
        }
        let budget = Duration::from_millis(2);
        // Virtual-time numerical fixtures inject a budget independently from
        // host test-runner contention. Product always enforces the 2 ms limit.
        #[cfg(any(test, feature = "test-support"))]
        let budget = self.test_work_budget.unwrap_or(budget);
        if began.elapsed() > budget {
            self.summary.deadline_misses += 1;
            use_processed = false;
            self.invalidate(Reason::Overload, now);
        }
        if !eligible {
            self.policy_state(now);
        }
        self.mix(mic, if use_processed { Some(&processed) } else { None }, out, eligible);
        let (capture_energy, capture_peak) = levels(mic);
        let (output_energy, output_peak) = levels(out);
        self.summary.capture_energy += capture_energy;
        self.summary.capture_peak = self.summary.capture_peak.max(capture_peak);
        self.summary.output_energy += output_energy;
        self.summary.output_peak = self.summary.output_peak.max(output_peak);
        if use_processed && render_active {
            // The output lags the microphone by the engine delay (430 of 480
            // frames); over a window of blocks the energies still compare.
            self.summary.render_active_blocks += 1;
            self.summary.active_capture_energy += capture_energy;
            self.summary.active_output_energy += output_energy;
        }
        self.summary.clipped_samples += clipped;
        self.summary.nonfinite_samples += nonfinite;
        self.summary.blocks += 1;
        if !use_processed {
            self.summary.bypass_blocks += 1;
        }
        let elapsed = began.elapsed().as_micros() as u64;
        self.summary.block_work_max_us = self.summary.block_work_max_us.max(elapsed);
        let bucket = [25, 50, 100, 200, 500, 1000, 2000].partition_point(|v| elapsed > *v);
        self.work_histogram[bucket] += 1;
        let cpu = cpu_ns().saturating_sub(cpu) / 1000;
        self.summary.cpu_us += cpu;
        self.summary.block_cpu_max_us = self.summary.block_cpu_max_us.max(cpu);
        self.skip_block = false;
    }
    /// Device-free product-path fixtures inject a virtual work budget so test
    /// runner preemption does not retire filters. Never used by product code.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_test_work_budget(&mut self, budget: Duration) {
        self.test_work_budget = Some(budget);
    }
    fn policy_state(&mut self, now: Instant) {
        self.summary.erl_db = None;
        self.summary.erle_db = None;
        self.summary.internal_delay_ms = None;
        let off = self.control.as_ref().is_none_or(|c| c.control.mode() == 0);
        self.transition(
            if off { State::Off } else { State::Armed },
            if off { Reason::SwitchOff } else { Reason::NoLocalRoute },
            now,
        );
    }
    /// Off uses the existing 5 ms encoder cadence. Retain only a 430-frame raw
    /// history for a future smooth transition; no AEC history is built here.
    pub fn raw(&mut self, mic: &[f32], out: &mut [f32], now: Instant) {
        self.policy_state(now);
        self.skip_block = false;
        self.mix(mic, None, out, false);
        let (capture_energy, capture_peak) = levels(mic);
        self.summary.capture_energy += capture_energy;
        self.summary.capture_peak = self.summary.capture_peak.max(capture_peak);
        self.summary.output_energy += capture_energy;
        self.summary.output_peak = self.summary.output_peak.max(capture_peak);
        self.summary.blocks += 1;
        self.summary.bypass_blocks += 1;
    }
    fn mix(&mut self, mic: &[f32], processed: Option<&[f32]>, out: &mut [f32], latency: bool) {
        let wet = processed.is_some();
        // On reference loss there is no valid processed block to crossfade with.
        // A bounded DC splice at the last output boundary avoids a click without
        // replaying old speech or running AEC on a fabricated reference.
        if !wet && self.wet_mix > 0.0 {
            self.splice = Some(self.last_output);
            self.wet_mix = 0.0;
        }
        let mut splice_offset = None;
        for (i, (&sample, output)) in mic.iter().zip(out.iter_mut()).enumerate() {
            let raw = if sample.is_finite() { sample } else { 0.0 };
            let delayed = self.raw_history[self.raw_cursor];
            self.raw_history[self.raw_cursor] = raw;
            self.raw_cursor = (self.raw_cursor + 1) % ENGINE_DELAY;
            self.latency_mix = (self.latency_mix
                + if latency {
                    1.0 / BLOCK as f32
                } else {
                    -1.0 / BLOCK as f32
                })
            .clamp(0.0, 1.0);
            self.wet_mix = (self.wet_mix + if wet { 1.0 / BLOCK as f32 } else { -1.0 / BLOCK as f32 }).clamp(0.0, 1.0);
            let bypass = raw + (delayed - raw) * self.latency_mix;
            let mut value = processed.map_or(bypass, |p| bypass + (p[i] - bypass) * self.wet_mix);
            if let Some(last) = self.splice.take() {
                splice_offset = Some(last - value);
            }
            if let Some(offset) = splice_offset {
                value += offset * (1.0 - (i + 1) as f32 / mic.len() as f32);
            }
            *output = value;
            self.last_output = value;
        }
    }
    /// One window per second, or sooner (at most four per second) when the
    /// state changed, so the owner can log a transition close to when it
    /// happened. Fixed values only; the owner formats them.
    pub fn summary(&mut self, now: Instant) -> Option<Summary> {
        let elapsed = now.saturating_duration_since(self.window);
        let changed = self.summary.transition_count != 0 || self.summary.transitions_dropped != 0;
        if elapsed < Duration::from_secs(1) && !(changed && elapsed >= Duration::from_millis(250)) {
            return None;
        }
        let mut result = std::mem::take(&mut self.summary);
        result.state = self.state;
        result.reason = self.reason;
        result.window_ms = elapsed.as_millis() as u64;
        if let Some(control) = &mut self.control {
            result.engines_built = control.control.engines_built();
            result.reference_dropped_frames = control
                .control
                .reference_dropped
                .swap(0, std::sync::atomic::Ordering::Relaxed);
            result.reference_source = control.control.reference_source();
            result.system_status = control.control.system_status();
            result.system_cpu_us = control
                .control
                .system_cpu_us
                .swap(0, std::sync::atomic::Ordering::Relaxed);
            result.system_bytes = control.control.system_bytes.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(reference) = control.active.as_mut().and_then(|a| a.reference.as_mut()) {
                result.reference_valid_frames = std::mem::take(&mut reference.valid_frames);
                result.reference_invalid_frames = std::mem::take(&mut reference.invalid_frames);
                result.reference_read_delay_us = (reference.read_delay_ns / 1000.0) as u64;
                result.reference_read_delay_steps = reference.read_delay_steps;
                result.reference_late_max_us = std::mem::take(&mut reference.late_max_us) as u64;
                result.reference_silence_frames = std::mem::take(&mut reference.synthetic_frames);
            }
        }
        let target = self.work_histogram.iter().sum::<u64>().saturating_mul(99).div_ceil(100);
        let mut count = 0;
        for (i, n) in self.work_histogram.iter().enumerate() {
            count += n;
            if count >= target {
                result.block_work_p99_us = [25, 50, 100, 200, 500, 1000, 2000, result.block_work_max_us][i];
                break;
            }
        }
        self.work_histogram.fill(0);
        self.window = now;
        Some(result)
    }
}

/// Sum of squares and peak of the finite samples of one block.
fn levels(samples: &[f32]) -> (f64, f32) {
    samples
        .iter()
        .filter(|s| s.is_finite())
        .fold((0.0, 0.0f32), |(energy, peak), s| {
            (energy + f64::from(s * s), peak.max(s.abs()))
        })
}

pub(crate) fn cpu_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct Timespec {
            sec: i64,
            nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock: i32, result: *mut Timespec) -> i32;
        }
        let mut t = Timespec { sec: 0, nsec: 0 };
        // SAFETY: Darwin CLOCK_THREAD_CPUTIME_ID=16, fixed writable struct. This
        // queries the current thread only; no strings, locks or heap ownership.
        if unsafe { clock_gettime(16, &mut t) } == 0 {
            t.sec as u64 * 1_000_000_000 + t.nsec as u64
        } else {
            0
        }
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn GetThreadTimes(
                thread: *mut std::ffi::c_void,
                creation: *mut FileTime,
                exit: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
        }
        let (mut c, mut e, mut k, mut u) = (
            FileTime { low: 0, high: 0 },
            FileTime { low: 0, high: 0 },
            FileTime { low: 0, high: 0 },
            FileTime { low: 0, high: 0 },
        );
        // SAFETY: current-thread pseudo handle and four writable FILETIME values.
        if unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } != 0 {
            (((u64::from(k.high) << 32) | u64::from(k.low)) + ((u64::from(u.high) << 32) | u64::from(u.low))) * 100
        } else {
            0
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        0
    }
}
