use super::{AudioPlatform, OutputDevice, OutputLatency};
use std::{
    sync::{Arc, mpsc},
    thread,
};

pub(super) struct DeviceSnapshot {
    pub service_generation: Option<u64>,
    pub outputs: Vec<OutputDevice>,
    pub latencies: Vec<OutputLatency>,
    pub capture: String,
}

pub(super) struct DeviceDiscovery {
    requests: mpsc::SyncSender<()>,
    results: mpsc::Receiver<Result<DeviceSnapshot, String>>,
    pending: bool,
    again: bool,
}

impl DeviceDiscovery {
    pub fn with_wake(platform: Arc<dyn AudioPlatform>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (requests, receive) = mpsc::sync_channel(1);
        let (send, results) = mpsc::sync_channel(1);
        // Metadata queries stay off the supply thread. On macOS 14.7.8/26.6.2,
        // CPAL description() created AudioUnits: moving it here removed worker
        // stalls but not HAL supply holes (111/137/1 send gaps per150s across
        // baseline/speaker-poll-off/all-poll-off). macOS now reads a shared HAL
        // metadata cache on notifications/explicit refresh, never a timer. The
        // 2026-10-07 user decision removes periodic enumeration on every OS;
        // Windows keeps its existing CPAL enumeration and stable ID format,
        // triggered by endpoint notifications/startup/explicit requests only.
        thread::Builder::new()
            .name("oink-audio-devices".into())
            .spawn(move || {
                while receive.recv().is_ok() {
                    let started = std::time::Instant::now();
                    let start_us = oink_audio::diagnostics::monotonic_us(started);
                    tracing::info!(start_us, "audio device query start");
                    let service_generation = platform.service_generation();
                    let snapshot = platform.outputs().map(|outputs| DeviceSnapshot {
                        service_generation,
                        latencies: platform.output_latencies(&outputs),
                        capture: platform.capture_capability(),
                        outputs,
                    });
                    tracing::info!(
                        start_us,
                        end_us = oink_audio::diagnostics::monotonic_us(std::time::Instant::now()),
                        elapsed_us = started.elapsed().as_micros() as u64,
                        success = snapshot.is_ok(),
                        error = ?snapshot.as_ref().err(),
                        "audio device query end"
                    );
                    if send.send(snapshot).is_err() {
                        break;
                    }
                    wake();
                }
            })
            .expect("audio device discovery thread creation");
        // Deliberately do not join on drop: a stalled OS query must not block
        // audio shutdown either. Closed channels retire it when the call returns.
        Self {
            requests,
            results,
            pending: false,
            again: false,
        }
    }

    pub fn request(&mut self) {
        // Coalesce connection and settings requests into one query.
        if !self.pending && self.requests.try_send(()).is_ok() {
            self.pending = true;
        }
    }
    pub fn changed(&mut self) {
        if self.pending {
            self.again = true;
        } else {
            self.request();
        }
    }

    pub fn poll(&mut self) -> Option<Result<DeviceSnapshot, String>> {
        let result = self.results.try_recv().ok()?;
        self.pending = false;
        if std::mem::take(&mut self.again) {
            self.request();
        }
        Some(result)
    }

    #[cfg(all(test, debug_assertions))]
    pub fn wait(&mut self) -> Option<Result<DeviceSnapshot, String>> {
        if !self.pending {
            return None;
        }
        let result = self.results.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        self.pending = false;
        if std::mem::take(&mut self.again) {
            self.request();
        }
        Some(result)
    }
}

// Permission-only probes must not enumerate devices or invalidate the HAL cache.
// Keep TCC off the audio owner just like other potentially blocking OS queries.
pub(super) struct PermissionObserver {
    requests: mpsc::SyncSender<()>,
    results: mpsc::Receiver<oink_audio::CapturePreflight>,
    pending: bool,
}
impl PermissionObserver {
    pub fn new(platform: Arc<dyn AudioPlatform>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (requests, receive) = mpsc::sync_channel(1);
        let (send, results) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("oink-audio-permission".into())
            .spawn(move || {
                while receive.recv().is_ok() {
                    if send.send(platform.capture_permission_preflight()).is_err() {
                        break;
                    }
                    wake();
                }
            })
            .expect("audio permission observer thread creation");
        Self {
            requests,
            results,
            pending: false,
        }
    }
    pub fn request(&mut self) {
        if !self.pending && self.requests.try_send(()).is_ok() {
            self.pending = true;
        }
    }
    pub fn poll(&mut self) -> Option<oink_audio::CapturePreflight> {
        let result = self.results.try_recv().ok()?;
        self.pending = false;
        Some(result)
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    struct Tcc(std::sync::Mutex<oink_audio::CapturePreflight>);
    impl AudioPlatform for Tcc {
        fn capture_permission_preflight(&self) -> oink_audio::CapturePreflight {
            self.0.lock().unwrap().clone()
        }
        fn outputs(&self) -> Result<Vec<OutputDevice>, String> {
            panic!("permission refresh enumerated audio devices")
        }
        fn capture_capability(&self) -> String {
            panic!("permission refresh queried audio capabilities")
        }
        fn playback(&self, _: &str) -> Result<Box<dyn super::super::PlaybackStream>, String> {
            panic!("permission refresh opened audio IO")
        }
    }
    #[test]
    fn permission_refresh_observes_fake_tcc_without_devices_or_capture() {
        use oink_audio::CapturePreflight;
        let tcc = Arc::new(Tcc(std::sync::Mutex::new(CapturePreflight::NotDetermined)));
        let (send, wake) = mpsc::channel();
        let mut observer = PermissionObserver::new(
            tcc.clone(),
            Arc::new(move || {
                let _ = send.send(());
            }),
        );
        for state in [
            CapturePreflight::NotDetermined,
            CapturePreflight::Denied,
            CapturePreflight::Authorized,
        ] {
            *tcc.0.lock().unwrap() = state.clone();
            observer.request();
            wake.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            assert_eq!(observer.poll(), Some(state));
        }
    }
}
