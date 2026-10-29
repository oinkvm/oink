//! Numeric copies at statistics owners, before tracing. No second destructive drain.
use super::Ingress;
use oink_telemetry::*;
use std::time::Instant;
pub fn os_class() -> OsVersionClass {
    #[cfg(target_os = "macos")]
    {
        let mut bytes = [0u8; 64];
        let mut length = bytes.len();
        // SAFETY: fixed NUL-terminated read-only sysctl and bounded output buffer.
        let ok = unsafe {
            libc::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0
            && length <= bytes.len()
            && let Ok(s) = std::str::from_utf8(&bytes[..length])
            && let Some(major) = s.split('.').next().and_then(|s| s.parse::<u8>().ok())
        {
            return OsVersionClass::MacOSMajor(major);
        }
    }
    #[cfg(windows)]
    {
        oink_crash::windows::os_class()
    }
    #[cfg(not(windows))]
    OsVersionClass::Unknown
}
pub fn platform(os: crate::api::Os) -> Platform {
    match os {
        crate::api::Os::Macos => Platform::Macos,
        crate::api::Os::Windows => Platform::Windows,
        _ => Platform::Unknown,
    }
}
pub fn input_window(ingress: &Ingress, sample: &oink_input::timing::Sample) {
    let Some(generation) = ingress.generation() else { return };
    if !ingress.window_eligible(sample.window_us) {
        return;
    }
    if sample.inject_completed > 0 || sample.capture_callbacks > 0 || sample.pending > 0 {
        ingress.mark_busy();
    }
    for (metric, value, nonempty) in [
        (
            Metric::InputQueueWaitWindowMax,
            sample.inject_queue_wait_max_us,
            sample.inject_completed > 0,
        ),
        (
            Metric::InputCaptureCallbackWindowMax,
            sample.capture_callback_max_us,
            sample.capture_callbacks > 0,
        ),
        (Metric::InputOldestPending, sample.oldest_pending_us, sample.pending > 0),
    ] {
        if nonempty {
            distribution_value(ingress, generation, metric, value, Some(sample.window_us));
        }
    }
    for (transport, path) in [
        (Transport::Reliable, &sample.jitter.reliable),
        (Transport::Lossy, &sample.jitter.lossy),
    ] {
        for (metric, d) in [
            (Metric::InputIngressVariation, &path.ingress),
            (Metric::InputDispatchVariation, &path.dispatch),
        ] {
            if d.count == 0 {
                continue;
            }
            let started = Instant::now();
            let Ok((bins, count, saturated)) = sampling::jitter_bins(d.count, d.late) else {
                continue;
            };
            let mut context = ingress.context(metric);
            context.transport = Some(transport);
            ingress.submit(
                generation,
                now(),
                Report::PerformanceDistribution(PerformanceDistribution {
                    context,
                    histogram: HistogramSchema::Jitter6,
                    bins,
                    sample_count: count,
                    missing: 0,
                    saturated,
                    window_us: Some(sample.window_us),
                    seal_us: started.elapsed().as_micros() as u64,
                }),
            );
        }
    }
}
pub fn distribution_value(ingress: &Ingress, generation: u64, metric: Metric, value_us: u64, window_us: Option<u64>) {
    let started = Instant::now();
    let bins = sampling::singleton(value_us);
    ingress.submit(
        generation,
        now(),
        Report::PerformanceDistribution(PerformanceDistribution {
            context: ingress.context(metric),
            histogram: HistogramSchema::Duration14,
            bins,
            sample_count: 1,
            missing: 0,
            saturated: false,
            window_us,
            seal_us: started.elapsed().as_micros() as u64,
        }),
    );
}
pub fn audio_histogram(metric: Metric, buckets: [u64; 8], window_us: u64) {
    let source = super::producer();
    let Some(ingress) = source.as_ref() else { return };
    let Some(generation) = ingress.generation() else { return };
    if !ingress.window_eligible(window_us) {
        return;
    }
    let start = Instant::now();
    let mut bins = [0; 14];
    let mut saturated = false;
    for (to, from) in bins.iter_mut().zip(buckets) {
        saturated |= from > u32::MAX as u64;
        *to = from.min(u32::MAX as u64) as u32;
    }
    let count = bins.iter().fold(0u32, |a, b| a.saturating_add(*b));
    if count == 0 {
        return;
    }
    ingress.mark_busy();
    let context = ingress.context(metric);
    ingress.submit(
        generation,
        now(),
        Report::PerformanceDistribution(PerformanceDistribution {
            context,
            histogram: HistogramSchema::Audio8,
            bins,
            sample_count: count,
            missing: 0,
            saturated,
            window_us: Some(window_us),
            seal_us: start.elapsed().as_micros() as u64,
        }),
    );
}

pub fn audio_presentation(pushed_at: Instant, playback_at: Instant) {
    if let Some(ingress) = super::producer().as_ref()
        && let Some(stamp) = ingress.stamp_at(pushed_at)
    {
        ingress.interval(stamp, Metric::AudioPushToPresentationEstimate, playback_at);
    }
}

pub fn link_interval(measurement: oink_network::link::timing::Measurement, start: Instant, end: Instant) {
    use oink_network::link::timing::{Measurement, Result as Outcome, Stage};
    let Some(p) = super::producer() else { return };
    let Some(stamp) = p.stamp_at(start) else { return };
    let metric = match measurement {
        Measurement::Setup(result) => Metric::LinkSetup {
            result: match result {
                Outcome::Ready => SetupResult::Ready,
                Outcome::Failed => SetupResult::Failed,
                Outcome::Cancelled => SetupResult::Cancelled,
                Outcome::Timeout => SetupResult::Timeout,
            },
        },
        Measurement::Stage(stage) => {
            let stage = match stage {
                Stage::Dial => SetupStage::Dial,
                Stage::InstanceAuth => SetupStage::InstanceAuth,
                Stage::Selection => SetupStage::Selection,
                Stage::PathInstall => SetupStage::PathInstall,
                Stage::ConfigReady => SetupStage::ConfigReady,
                Stage::Migration => SetupStage::Migration,
                Stage::LowTcp => SetupStage::LowTcp,
                Stage::LowTls => SetupStage::LowTls,
                Stage::LowBind => SetupStage::LowBind,
                Stage::LowReady => SetupStage::LowReady,
            };
            if matches!(
                stage,
                SetupStage::LowTcp | SetupStage::LowTls | SetupStage::LowBind | SetupStage::LowReady
            ) {
                Metric::LowSetup { stage }
            } else {
                Metric::LinkSetupStage { stage }
            }
        }
    };
    p.interval(stamp, metric, end);
}

/// Called at the native boundary: no owned closure, allocation or reclamation.
pub fn drag_native_begin(generation: u64, at: Instant, end: Instant) {
    if let Some(p) = super::producer() {
        let boundary = if cfg!(target_os = "macos") {
            StartBoundary::AppkitWillBegin
        } else {
            StartBoundary::OleLoopEnter
        };
        p.interval(
            super::Stamp {
                generation,
                at,
                transport: None,
            },
            Metric::DragStart { boundary },
            end,
        );
    }
}

pub fn drag_preparation(stage: oink_clipboard::dnd::PreparationStage, start: Instant, end: Instant) {
    use oink_clipboard::dnd::PreparationStage;
    if let Some(elapsed) = end.checked_duration_since(start) {
        tracing::debug!(
            dnd_phase = 30_u64,
            ?stage,
            elapsed_us = elapsed.as_micros() as u64,
            "drag preparation stage finished"
        );
    }
    if let Some(p) = super::producer()
        && let Some(stamp) = p.stamp_at(start)
    {
        let stage = match stage {
            PreparationStage::SourceRead => PrepareStage::SourceRead,
            PreparationStage::Normalization => PrepareStage::Normalization,
            PreparationStage::NativePreparation => PrepareStage::NativePreparation,
            PreparationStage::Metadata => PrepareStage::Metadata,
            PreparationStage::PendingWindow => PrepareStage::PendingWindow,
            PreparationStage::SourcePress => PrepareStage::SourcePress,
            PreparationStage::NativeAcknowledgement => PrepareStage::NativeAcknowledgement,
            PreparationStage::WritersPreview => PrepareStage::WritersPreview,
            PreparationStage::NativeBegin => PrepareStage::NativeBegin,
            PreparationStage::Preview => PrepareStage::Preview,
            PreparationStage::Namespace => PrepareStage::Namespace,
            PreparationStage::Registration => PrepareStage::Registration,
        };
        p.interval(stamp, Metric::DragPrepareStage { stage }, end);
    }
}

/// Fixed local stage log shares the exact boundaries used for report admission.
pub fn drag_target_stage(stage: PrepareStage, start: Instant, end: Instant) {
    let Some(elapsed) = end.checked_duration_since(start) else {
        return;
    };
    tracing::debug!(
        dnd_phase = 30_u64,
        ?stage,
        elapsed_us = elapsed.as_micros() as u64,
        "target drag local stage finished"
    );
    if let Some(p) = super::producer()
        && let Some(stamp) = p.stamp_at(start)
    {
        p.interval(stamp, Metric::DragPrepareStage { stage }, end);
    }
}
