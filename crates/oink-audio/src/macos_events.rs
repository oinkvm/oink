//! Callback diagnostics drained off the realtime thread. No PCM is recorded.
use super::{HostClock, log_thread_scheduling, oink_audio_capture_events};
use crate::diagnostics::{Histogram, History, monotonic_us};
use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NativeEvent {
    pub callback_ns: u64,
    pub host_ns: u64,
    pub sequence: u64,
    pub gap_ns: u64,
    pub frames: u64,
    pub queued_frames: u64,
    pub dropped_frames: u64,
}

pub(super) struct Events {
    history: History<NativeEvent, 8>,
    // One second of ordinary 128-frame callbacks fits without a 1 ms poll.
    buffer: [NativeEvent; 1024],
    previous_nonempty_ns: Option<u64>,
    last_report: Instant,
    previous_drain: Instant,
    previous_age_us: Option<i64>,
    scheduled: bool,
    callbacks: u64,
    frames: u64,
    gaps: Histogram,
    max_age_us: Option<i64>,
    max_depth: u64,
    pub channel_dropped: u64,
    pub health: Histogram,
}
impl Events {
    pub fn new(muted: bool, clock: &HostClock) -> Self {
        log_thread_scheduling(std::ptr::null_mut(), "capture delivery");
        tracing::info!(
            source_muted = muted,
            host_anchor_ns = clock.host_ns,
            monotonic_anchor_us = monotonic_us(clock.instant),
            anchor_bracket_us = clock.bracket_us,
            "audio capture diagnostic epoch (native callback_ns uses host clock)"
        );
        Self {
            history: History::default(),
            buffer: [NativeEvent::default(); 1024],
            previous_nonempty_ns: None,
            last_report: Instant::now(),
            previous_drain: Instant::now(),
            previous_age_us: None,
            scheduled: false,
            callbacks: 0,
            frames: 0,
            gaps: Histogram::default(),
            max_age_us: None,
            max_depth: 0,
            channel_dropped: 0,
            health: Histogram::default(),
        }
    }
    pub fn drain(&mut self, raw: *mut c_void, clock: &HostClock) {
        let now = Instant::now();
        let delivery_gap = now.saturating_duration_since(self.previous_drain);
        self.previous_drain = now;
        let (mut lost, mut trimmed, mut audio_dropped) = (0, 0, 0);
        // SAFETY: Live handle on its sole consumer; POD layout matches C and
        // destination is writable for exactly the advertised number of events.
        let count = unsafe {
            oink_audio_capture_events(
                raw,
                self.buffer.as_mut_ptr(),
                self.buffer.len(),
                &mut lost,
                &mut trimmed,
                &mut audio_dropped,
            )
        };
        for event in self.buffer[..count].iter().copied() {
            self.callbacks += 1;
            self.frames += event.frames;
            self.gaps.observe(Duration::from_nanos(event.gap_ns));
            let age_us = (event.host_ns != 0)
                .then(|| ((i128::from(event.callback_ns) - i128::from(event.host_ns)) / 1000) as i64);
            if let Some(age) = age_us {
                self.max_age_us = Some(self.max_age_us.map_or(age, |old| old.max(age)));
            }
            self.max_depth = self.max_depth.max(event.queued_frames);
            let age_jump_us = age_us.zip(self.previous_age_us).map(|(now, before)| now - before);
            self.previous_age_us = age_us;
            let audio_gap_us = if event.frames > 0 {
                self.previous_nonempty_ns
                    .replace(event.callback_ns)
                    .map(|previous| event.callback_ns.saturating_sub(previous) / 1000)
            } else {
                None
            };
            self.history.push(event);
            if event.gap_ns >= 20_000_000
                || event.dropped_frames > 0
                || age_jump_us.is_some_and(|age| age >= 20_000)
                || audio_gap_us.is_some_and(|gap| gap >= 20_000)
            {
                tracing::info!(callback_us = clock.instant(event.callback_ns).map(monotonic_us),
                    gap_us = event.gap_ns / 1000, audio_timestamp_age_us = age_us, age_jump_us, audio_gap_us,
                    collected_us = monotonic_us(Instant::now()), diagnostic_events_lost = lost,
                    native_trimmed_frames = trimmed, channel_dropped = self.channel_dropped,
                    context = ?self.history, "audio capture callback gap");
            }
        }
        if count > 0 && !self.scheduled {
            log_thread_scheduling(raw, "capture callback");
            self.scheduled = true;
        }
        // This is supervisor drain latency, not PCM delivery. The encoder is
        // callback-driven; a normal one-second bookkeeping interval is healthy.
        if delivery_gap >= Duration::from_secs(2) {
            tracing::info!(start_us = monotonic_us(now), gap_us = delivery_gap.as_micros() as u64,
                context = ?self.history, native_trimmed_frames = trimmed, channel_dropped = self.channel_dropped,
                diagnostic_events_lost = lost, "audio capture delivery gap");
        }
        if self.last_report.elapsed() >= Duration::from_secs(1) {
            tracing::debug!(monotonic_us = monotonic_us(Instant::now()), callbacks = self.callbacks,
                frames = self.frames, gaps = ?self.gaps.take(), max_audio_timestamp_age_us = self.max_age_us,
                max_queued_frames = self.max_depth, audio_dropped_frames = audio_dropped,
                diagnostic_events_lost = lost, native_trimmed_frames = trimmed,
                channel_dropped = self.channel_dropped, health_check = ?self.health.take(),
                "audio native capture timing (drops and trims cumulative)");
            self.callbacks = 0;
            self.frames = 0;
            self.max_age_us = None;
            self.max_depth = 0;
            self.last_report = Instant::now();
        }
    }
}
