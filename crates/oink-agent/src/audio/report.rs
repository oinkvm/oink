//! Keep diagnostic formatting and subscriber/file I/O off the supply worker.
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, sync_channel},
};
type Job = Box<dyn FnOnce() + Send>;
struct Reporter {
    tx: SyncSender<Job>,
    dropped: Arc<AtomicU64>,
}
impl Reporter {
    fn new(capacity: usize) -> Self {
        let (tx, rx) = sync_channel::<Job>(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        std::thread::Builder::new()
            .name("oink-audio-report".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                    let count = lost.swap(0, Ordering::Relaxed);
                    if count != 0 {
                        tracing::warn!(count, "audio diagnostic reports dropped: reporter queue full");
                    }
                }
            })
            .expect("audio diagnostic reporter creation");
        Self { tx, dropped }
    }
    fn submit(&self, job: Job) {
        // A stuck subscriber may lose diagnostics, never backpressure audio.
        // Inputs are metadata snapshots, never PCM; memory is bounded by queue.
        if self.tx.try_send(job).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
fn reporter() -> &'static Reporter {
    static REPORTER: OnceLock<Reporter> = OnceLock::new();
    REPORTER.get_or_init(|| Reporter::new(128))
}
pub(super) fn submit(job: impl FnOnce() + Send + 'static) {
    reporter().submit(Box::new(job));
}

/// Ordinary reporter only. Each native sink retains at most 16 exceptional
/// callbacks between polls; queue overflow is counted instead of backpressure.
pub(crate) fn callback_incidents(output: &oink_audio::playback::PlaybackMetrics) {
    for incident in &output.callback_incidents {
        let [ingress, pull, output, publish, adapter] = incident.phase_us();
        tracing::debug!(target: "oink_audio_diagnostics", kind="audio_callback_incident",
            sample_mono_us=incident.at_us, callback_sequence=incident.sequence,
            callback_frames=incident.frames, callback_rate=incident.rate,
            callback_budget_us=incident.budget_ns / 1000,
            callback_work_us=incident.elapsed_ns / 1000,
            callback_gap_us=incident.gap_us, native_gap_frames=incident.native_gap_frames,
            callback_ingress_us=ingress, callback_pull_us=pull,
            callback_output_us=output, callback_publish_us=publish, callback_adapter_us=adapter,
            callback_sources=incident.work.sources, callback_scan_steps=incident.work.scan_steps,
            callback_codec_calls=incident.work.codec_calls, callback_plc_frames=incident.work.plc_frames,
            callback_decode_errors=incident.work.decode_errors,
            callback_work_limited=incident.work.work_limited,
            callback_missing_frames=incident.missing_frames, callback_fault=incident.fault,
            "exceptional native audio callback; phase times include preemption");
    }
}

pub(super) fn capture_levels(source: oink_foundation::DeviceId, levels: oink_audio::CaptureLevels) {
    submit(move || {
        let pre = levels.before_gain;
        let post = levels.after_gain;
        // Option scalars omit unavailable dB values; explicit silence/measured
        // booleans preserve -infinity and verification in the ordinary store.
        tracing::debug!(
            ?source,
            audio_level_window_sequence = levels.window_sequence,
            audio_level_window_ms = levels.window_ms,
            pre_peak_dbfs = pre.peak_dbfs,
            pre_rms_dbfs = pre.rms_dbfs,
            pre_zero_frame_fraction = pre.zero_frame_fraction,
            pre_frames = pre.frames,
            pre_nonfinite_samples = pre.nonfinite_samples,
            pre_digital_silence = pre.peak_dbfs.is_none() && pre.zero_frame_fraction == 1.0,
            post_measured = post.is_some(),
            post_peak_dbfs = post.and_then(|p| p.peak_dbfs),
            post_rms_dbfs = post.and_then(|p| p.rms_dbfs),
            post_zero_frame_fraction = post.map(|p| p.zero_frame_fraction),
            post_frames = post.map(|p| p.frames),
            post_nonfinite_samples = post.map(|p| p.nonfinite_samples),
            post_digital_silence = post.map(|p| p.peak_dbfs.is_none() && p.zero_frame_fraction == 1.0),
            wasapi_buffers = levels.wasapi_buffers,
            wasapi_silent_buffers = levels.wasapi_silent_buffers,
            "audio capture levels"
        );
    });
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn blocked_log_sink_cannot_block_supply_and_overflow_is_counted() {
        let reporter = Reporter::new(1);
        let (entered, ready) = sync_channel(1);
        let (release, wait) = sync_channel(1);
        let caller = std::thread::current().id();
        reporter.submit(Box::new(move || {
            assert_ne!(caller, std::thread::current().id());
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
        }));
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        reporter.submit(Box::new(|| {}));
        reporter.submit(Box::new(|| panic!("overflow job must not execute")));
        assert_eq!(reporter.dropped.load(Ordering::Relaxed), 1);
        release.send(()).unwrap();
    }
}
