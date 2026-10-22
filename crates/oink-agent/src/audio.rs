//! Audio worker: platform calls never run on the input-routing task.
mod link;
pub(crate) use link::{Handle as LinkHandle, start_link_runtime};

#[derive(Clone, Debug)]
pub(crate) struct OwnerOptionApplied {
    pub current: dto::DevicePreference,
    pub outcome: dto::ControlOutcome,
    pub reason: dto::ControlFailure,
    pub persisted: bool,
    pub fact_sequence: Option<u64>,
}

use oink_audio::diagnostics::Cadence;
mod discovery;
mod local_output;
mod report;
mod timing;
use crate::node::lock;
use discovery::{DeviceDiscovery, DeviceSnapshot};
use oink_audio::{AudioPhase, Problem, ReceiverReport, StreamFormat};
use oink_audio::{
    Capture, CapturePermission, CapturePreflight, CaptureScope, DeviceAudioState, DeviceOutputs, DeviceState,
    OutputDevice, OutputLatency, Settings, Status, dsp,
};
use oink_foundation::DeviceId;
use oink_protocol::v1::{
    self, Message, RawValue, dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::Ordering,
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_SOURCES: usize = 16;
pub(crate) fn dsp_config(format: StreamFormat) -> dsp::Config {
    format.config()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Negotiated {
    format: StreamFormat,
    generation: u64,
}

// Private seams keep lifecycle tests entirely independent of OS audio and sockets.
trait CaptureStream {
    fn finish(&mut self) -> Option<String> {
        None
    }
    fn default_output_changed(&self) -> bool {
        false
    }
    fn local_controls_changed(&self) -> bool {
        false
    }
    fn set_forwarding_pause(&self, _: Option<oink_audio::media_pause::ForwardingPause>) {}
    fn restart_reason(&self) -> Option<String> {
        None
    }
    fn info(&self) -> Option<oink_audio::CaptureInfo> {
        None
    }
    fn source_muted(&self) -> bool {
        false
    }
    fn error(&self) -> Option<String>;
}
impl CaptureStream for Capture {
    fn finish(&mut self) -> Option<String> {
        #[cfg(any(windows, target_os = "macos"))]
        {
            Capture::finish(self)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            None
        }
    }
    fn default_output_changed(&self) -> bool {
        #[cfg(any(windows, target_os = "macos"))]
        {
            Capture::default_output_changed(self)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            false
        }
    }
    fn local_controls_changed(&self) -> bool {
        #[cfg(any(windows, target_os = "macos"))]
        {
            Capture::local_controls_changed(self)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            false
        }
    }
    fn set_forwarding_pause(&self, pause: Option<oink_audio::media_pause::ForwardingPause>) {
        Capture::set_forwarding_pause(self, pause);
    }
    fn restart_reason(&self) -> Option<String> {
        #[cfg(any(target_os = "macos", windows))]
        {
            Capture::restart_reason(self)
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            None
        }
    }
    fn info(&self) -> Option<oink_audio::CaptureInfo> {
        Some(Capture::info(self))
    }
    fn source_muted(&self) -> bool {
        Capture::source_muted(self)
    }
    fn error(&self) -> Option<String> {
        Capture::error(self)
    }
}
trait PlaybackStream: Send {
    fn set_output_quality_counters(&self, _: Arc<oink_audio::sink::DropoutCounters>) {}
    fn begin_idle_drain(&self) {}
    fn cancel_idle_drain(&self) {}
    fn idle_drained(&self) -> bool {
        true
    }
    fn realtime_source(&mut self, _: dsp::Config) -> Result<Option<oink_audio::sink::Source>, String> {
        Ok(None)
    }
    fn device_invalidated(&self) -> bool {
        false
    }
    fn take_metrics(&self) -> Option<oink_audio::playback::PlaybackMetrics> {
        None
    }
    fn info(&self) -> Option<oink_audio::playback::PlaybackInfo> {
        None
    }
    fn error(&self) -> Option<String>;
}
impl PlaybackStream for oink_audio::sink::Sink {
    fn set_output_quality_counters(&self, counters: Arc<oink_audio::sink::DropoutCounters>) {
        self.set_output_quality_counters(counters);
    }
    fn begin_idle_drain(&self) {
        self.begin_idle_drain();
    }
    fn idle_drained(&self) -> bool {
        self.idle_drained()
    }
    fn cancel_idle_drain(&self) {
        self.cancel_idle_drain();
    }
    fn realtime_source(&mut self, config: dsp::Config) -> Result<Option<oink_audio::sink::Source>, String> {
        self.source(config).map(Some)
    }
    fn device_invalidated(&self) -> bool {
        self.device_invalidated()
    }
    fn take_metrics(&self) -> Option<oink_audio::playback::PlaybackMetrics> {
        Some(self.take_metrics())
    }
    fn info(&self) -> Option<oink_audio::playback::PlaybackInfo> {
        Some(self.info())
    }
    fn error(&self) -> Option<String> {
        self.error()
    }
}
trait AudioPlatform: Send + Sync {
    fn volume_endpoint(
        &self,
        _: &str,
        _: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn oink_audio::volume::Endpoint>, oink_audio::volume::Fault> {
        Err(oink_audio::volume::Fault {
            status: dto::ExitVolumeStatus::Unsupported,
            native_code: None,
        })
    }
    fn capture_encoded(
        &self,
        _: oink_audio::source::Encoder,
        _: bool,
        _: CaptureScope,
    ) -> Result<Box<dyn CaptureStream>, String> {
        Err("real-time capture unsupported".into())
    }
    fn forwarding_pause(&self) -> oink_audio::media_pause::ForwardingPause {
        oink_audio::media_pause::ForwardingPause::default()
    }
    fn service_generation(&self) -> Option<u64> {
        None
    }
    /// Notification generation, sampled without making a device query. None
    /// supports startup/explicit refresh only; it never enables a polling timer.
    fn device_revision(&self) -> Option<u64> {
        None
    }
    fn invalidate_devices(&self) {}
    fn capture_permission_preflight(&self) -> CapturePreflight {
        CapturePreflight::NotDetermined
    }
    fn permission_requires_signal(&self) -> bool {
        false
    }
    fn output_latencies(&self, outputs: &[OutputDevice]) -> Vec<OutputLatency> {
        outputs
            .iter()
            .map(|o| OutputLatency {
                output: o.id.clone(),
                reported_latency_ms: None,
            })
            .collect()
    }
    fn capture_cleanup_failed(&self) -> bool {
        false
    }
    fn supports_default_following(&self) -> bool {
        false
    }
    fn default_output_id(&self) -> Option<String> {
        None
    }
    fn outputs(&self) -> Result<Vec<OutputDevice>, String>;
    fn capture_capability(&self) -> String;
    fn playback(&self, output: &str) -> Result<Box<dyn PlaybackStream>, String>;
}
#[cfg(any(test, debug_assertions))]
struct UnitTestAudio;
#[cfg(any(test, debug_assertions))]
impl AudioPlatform for UnitTestAudio {
    fn outputs(&self) -> Result<Vec<OutputDevice>, String> {
        Ok(Vec::new())
    }
    fn capture_capability(&self) -> String {
        "unit-test".into()
    }
    fn playback(&self, _: &str) -> Result<Box<dyn PlaybackStream>, String> {
        Err("unit tests have no native output".into())
    }
}
struct SystemAudio {
    #[cfg(any(windows, target_os = "macos"))]
    recovery_dir: std::path::PathBuf,
}
impl AudioPlatform for SystemAudio {
    fn volume_endpoint(
        &self,
        output: &str,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn oink_audio::volume::Endpoint>, oink_audio::volume::Fault> {
        oink_audio::volume::open(output, wake)
    }
    fn capture_encoded(
        &self,
        encoder: oink_audio::source::Encoder,
        mute_source: bool,
        scope: CaptureScope,
    ) -> Result<Box<dyn CaptureStream>, String> {
        #[cfg(windows)]
        let capture = {
            if scope == CaptureScope::Global {
                return Err("global capture scope is unsupported on Windows".into());
            }
            Capture::start_encoded(encoder, mute_source, self.recovery_dir.clone())
        };
        #[cfg(target_os = "macos")]
        let capture = {
            let mut options = match scope {
                CaptureScope::DefaultOutput => oink_audio::MacCaptureOptions::default(),
                CaptureScope::Global => oink_audio::MacCaptureOptions {
                    global_tap: true,
                    physical_master: false,
                    ..Default::default()
                },
            };
            options.recovery_dir = Some(self.recovery_dir.clone());
            Capture::start_encoded(encoder, mute_source, options)
        };
        #[cfg(not(any(windows, target_os = "macos")))]
        let capture: Result<Capture, String> = {
            let _ = (encoder, mute_source, scope);
            Err("system capture unsupported".into())
        };
        capture.map(|capture| Box::new(capture) as Box<dyn CaptureStream>)
    }
    fn service_generation(&self) -> Option<u64> {
        oink_audio::device_notifications::service_generation()
    }
    fn device_revision(&self) -> Option<u64> {
        Some(oink_audio::device_notifications::generation())
    }
    fn invalidate_devices(&self) {
        oink_audio::device_notifications::invalidate();
    }
    #[cfg(target_os = "macos")]
    fn capture_permission_preflight(&self) -> CapturePreflight {
        oink_audio::capture_permission_preflight()
    }
    fn permission_requires_signal(&self) -> bool {
        cfg!(target_os = "macos")
    }
    fn output_latencies(&self, outputs: &[OutputDevice]) -> Vec<OutputLatency> {
        oink_audio::playback::output_latencies(outputs)
    }
    fn capture_cleanup_failed(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            Capture::cleanup_failed()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    fn supports_default_following(&self) -> bool {
        cfg!(target_os = "macos")
    }
    fn default_output_id(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            Capture::current_default_output_id().ok()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }
    fn outputs(&self) -> Result<Vec<OutputDevice>, String> {
        oink_audio::playback::outputs()
    }
    fn capture_capability(&self) -> String {
        oink_audio::capture_capability().into()
    }
    fn playback(&self, output: &str) -> Result<Box<dyn PlaybackStream>, String> {
        oink_audio::sink::Sink::start(output).map(|playback| Box::new(playback) as Box<dyn PlaybackStream>)
    }
}
