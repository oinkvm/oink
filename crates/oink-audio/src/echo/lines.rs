//! The AEC log lines every platform writes (target `oink_echo`, INFO): the
//! same names on desktop, iOS (os_log) and Android (logcat), so one monitor
//! reads all of them. Behaviour-neutral: owners feed processor summaries in
//! and emit what this returns; nothing here changes audio.
//!
//! Rate limits: one `aec` + `aec_timing` pair every 2 s while the engine is
//! engaged or degraded (every 30 s while it is only `off`/`armed`), and at
//! most four `aec_transition` lines per second. Nothing is written while no
//! AEC-capable capture exists, because then no summaries arrive.
use super::{BLOCK, Reason, State, Summary, processor::Transition};
use std::time::{Duration, Instant};

pub const TARGET: &str = "oink_echo";
const PERIOD: Duration = Duration::from_secs(2);
const IDLE_PERIOD: Duration = Duration::from_secs(30);
const TRANSITIONS_PER_SECOND: u32 = 4;
const FLOOR_DB: f64 = -120.0;

/// What one owner pass should log. Plain values, so the owner can move it to
/// a reporter thread before formatting.
#[derive(Clone, Copy, Debug)]
pub struct LogLines {
    summary: Option<Summary>,
    transitions: [Transition; TRANSITIONS_PER_SECOND as usize],
    transition_count: usize,
    suppressed: u64,
}
impl LogLines {
    pub fn is_empty(&self) -> bool {
        self.summary.is_none() && self.transition_count == 0
    }
    /// A periodic summary is due now (owners add their own context lines).
    pub fn has_summary(&self) -> bool {
        self.summary.is_some()
    }
    /// Write the lines (ordinary threads only).
    pub fn emit(&self) {
        for transition in &self.transitions[..self.transition_count] {
            tracing::info!(target: TARGET, event = "aec_transition",
                state = transition.state.name(), reason = transition.reason.name(),
                at_ms = transition.at_us / 1000, suppressed = self.suppressed,
                "echo cancellation state");
        }
        if let Some(summary) = &self.summary {
            emit_summary(summary);
        }
    }
}

fn db(energy: f64, samples: u64) -> f64 {
    if samples == 0 || energy <= 0.0 {
        FLOOR_DB
    } else {
        (10.0 * (energy / samples as f64).log10()).max(FLOOR_DB)
    }
}
fn peak_db(peak: f32) -> f64 {
    if peak <= 0.0 {
        FLOOR_DB
    } else {
        (20.0 * f64::from(peak).log10()).max(FLOOR_DB)
    }
}

/// The summary in two lines (each within the diagnostics recorder's 24
/// numeric fields). Unknown values are NaN: kept as `NaN` in the local log,
/// dropped by the diagnostics recorder.
fn emit_summary(s: &Summary) {
    let samples = s.blocks * BLOCK as u64;
    // Attenuation actually applied while the speaker plays: microphone vs.
    // sent energy over processed blocks whose reference was active.
    let attenuation_db = if s.active_capture_energy > 0.0 && s.active_output_energy > 0.0 {
        10.0 * (s.active_capture_energy / s.active_output_energy).log10()
    } else {
        f64::NAN
    };
    tracing::info!(target: TARGET, event = "aec",
        state = s.state.name(), reason = s.reason.name(),
        reference_source = s.reference_source.name(), system_status = s.system_status.name(),
        reset_reason = s.reset_reason.name(),
        engaged = s.state.engaged(),
        window_ms = s.window_ms, blocks = s.blocks, processed_blocks = s.processed_blocks,
        bypass_blocks = s.bypass_blocks,
        capture_rms_dbfs = db(s.capture_energy, samples), capture_peak_dbfs = peak_db(s.capture_peak),
        render_rms_dbfs = db(s.render_energy, s.reference_blocks * BLOCK as u64),
        render_peak_dbfs = peak_db(s.render_peak),
        output_rms_dbfs = db(s.output_energy, samples), output_peak_dbfs = peak_db(s.output_peak),
        render_active_blocks = s.render_active_blocks, attenuation_db,
        aec3_erle_db = s.aec3_erle_db.unwrap_or(f64::NAN), aec3_erl_db = s.aec3_erl_db.unwrap_or(f64::NAN),
        aec3_delay_ms = s.internal_delay_ms.map_or(f64::NAN, f64::from),
        erle_db = s.erle_db.unwrap_or(f64::NAN),
        clipped_samples = s.clipped_samples, capture_gaps = s.capture_gaps,
        diverged_blocks = s.diverged_blocks, resets = s.resets, engines_built = s.engines_built,
        epoch = s.epoch,
        "echo cancellation");
    tracing::info!(target: TARGET, event = "aec_timing",
        capture_quality = s.capture_quality.name(), reference_quality = s.reference_quality.name(),
        window_ms = s.window_ms,
        reference_read_delay_us = s.reference_read_delay_us,
        reference_read_delay_steps = s.reference_read_delay_steps,
        reference_late_max_us = s.reference_late_max_us, reference_late_blocks = s.reference_late_blocks,
        reference_silence_frames = s.reference_silence_frames,
        reference_valid_frames = s.reference_valid_frames, reference_invalid_frames = s.reference_invalid_frames,
        reference_dropped_frames = s.reference_dropped_frames,
        alignment_error_us = s.alignment_error_us, render_ppm = s.render_ppm, capture_ppm = s.capture_ppm,
        asrc_ratio = s.asrc_ratio, cpu_us = s.cpu_us, block_cpu_max_us = s.block_cpu_max_us,
        block_work_max_us = s.block_work_max_us, block_work_p99_us = s.block_work_p99_us,
        deadline_misses = s.deadline_misses, wait_max_us = s.wait_max_us,
        system_cpu_us = s.system_cpu_us, nonfinite_samples = s.nonfinite_samples,
        transitions_dropped = s.transitions_dropped, output_epoch = s.output_epoch,
        "echo cancellation timing");
}

/// Aggregates processor summaries of one capture into rate-limited lines.
#[derive(Debug, Default)]
pub struct SummaryLog {
    pending: Option<Summary>,
    last_line: Option<Instant>,
    last_state: Option<(State, Reason)>,
    second: Option<Instant>,
    transition_lines: u32,
    suppressed: u64,
}
impl SummaryLog {
    pub fn new() -> Self {
        Self::default()
    }
    /// Take one processor summary; returns what to log now.
    pub fn record(&mut self, summary: Summary, now: Instant) -> LogLines {
        let mut lines = LogLines {
            summary: None,
            transitions: [Transition::default(); TRANSITIONS_PER_SECOND as usize],
            transition_count: 0,
            suppressed: 0,
        };
        if self
            .second
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
        {
            self.second = Some(now);
            self.transition_lines = 0;
        }
        self.suppressed += summary.transitions_dropped;
        for transition in &summary.transitions[..summary.transition_count] {
            if self.transition_lines < TRANSITIONS_PER_SECOND {
                lines.transitions[lines.transition_count] = *transition;
                lines.transition_count += 1;
                self.transition_lines += 1;
            } else {
                self.suppressed += 1;
            }
        }
        lines.suppressed = self.suppressed;
        match &mut self.pending {
            Some(pending) => pending.merge(&summary),
            None => self.pending = Some(summary),
        }
        let state = (summary.state, summary.reason);
        let period = if summary.state.engaged() || summary.state == State::Degraded {
            PERIOD
        } else {
            IDLE_PERIOD
        };
        let due = self
            .last_line
            .is_none_or(|at| now.saturating_duration_since(at) >= period)
            // A settled new state is summarized at once (its transitions were
            // just logged), not up to 30 s later.
            || self.last_state.is_some_and(|last| last.0 != state.0);
        if due {
            lines.summary = self.pending.take();
            self.last_line = Some(now);
            self.last_state = Some(state);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn summary(state: State, transitions: usize) -> Summary {
        let mut s = Summary {
            state,
            blocks: 100,
            processed_blocks: 100,
            window_ms: 1000,
            ..Summary::default()
        };
        for i in 0..transitions {
            s.transitions[i] = Transition {
                state,
                reason: Reason::None,
                at_us: i as u64,
            };
        }
        s.transition_count = transitions;
        s
    }
    #[test]
    fn engaged_summaries_merge_into_one_line_every_two_seconds_and_transitions_are_bounded() {
        let start = Instant::now();
        let mut log = SummaryLog::new();
        let first = log.record(summary(State::Converging, 1), start);
        assert!(first.summary.is_some(), "the first summary is logged at once");
        assert_eq!(first.transition_count, 1);
        let second = log.record(summary(State::Converging, 0), start + Duration::from_secs(1));
        assert!(second.is_empty());
        let third = log.record(summary(State::Converging, 0), start + Duration::from_secs(2));
        let merged = third.summary.unwrap();
        assert_eq!(merged.blocks, 200, "the two windows since the last line");
        assert_eq!(merged.window_ms, 2000);
        // A burst of transitions within one second: four lines, rest counted.
        let burst = log.record(summary(State::Active, 7), start + Duration::from_millis(3100));
        assert_eq!(burst.transition_count, 4);
        assert_eq!(burst.suppressed, 3);
        assert!(burst.summary.is_some(), "a new state is summarized at once");
    }
    #[test]
    fn armed_summaries_are_logged_rarely() {
        let start = Instant::now();
        let mut log = SummaryLog::new();
        assert!(log.record(summary(State::Armed, 0), start).summary.is_some());
        for second in 1..30 {
            let lines = log.record(summary(State::Armed, 0), start + Duration::from_secs(second));
            assert!(lines.summary.is_none(), "{second}");
        }
        assert!(
            log.record(summary(State::Armed, 0), start + Duration::from_secs(30))
                .summary
                .is_some()
        );
    }
}
