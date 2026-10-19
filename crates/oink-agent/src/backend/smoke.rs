//! Private launch checks keep the real AppKit/host lifecycle and native trail
//! appearance, with desktop I/O disabled even if this executable has TCC grants.
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

use oink_foundation::Point;
use oink_input::{
    CaptureMode, EventHandler, FeelProfile, InjectEvent, InputBackend, InputError, InputPermissions, PermissionState,
};

pub(super) fn network() -> anyhow::Result<oink_network::NetworkConfig> {
    let home = std::env::var_os("OINK_HOME").ok_or_else(|| anyhow::anyhow!("launch smoke requires OINK_HOME"))?;
    let home = Path::new(&home);
    anyhow::ensure!(
        home.is_absolute() && home.is_dir(),
        "launch smoke requires an absolute, empty OINK_HOME directory"
    );
    anyhow::ensure!(
        std::fs::read_dir(home)?.next().is_none(),
        "launch smoke requires an empty OINK_HOME directory"
    );
    Ok(oink_network::NetworkConfig {
        data_dir: home.to_path_buf(),
        device_name: "Launch smoke".into(),
        bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        bulk_port: 0,
        mdns: oink_network::MdnsMode::Off,
        beacon: false,
        beacon_ports: Vec::new(),
        ..Default::default()
    })
}

pub(crate) fn options(net: oink_network::NetworkConfig) -> anyhow::Result<crate::node::NodeOptions> {
    let display = oink_display::platform_backend()?;
    Ok(crate::node::NodeOptions {
        net,
        input: Arc::new(NoDesktopInput::default()),
        displays: display.displays()?,
        display_backend: Some(Arc::from(display)),
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        cursor_trail: true,
    })
}

#[derive(Default)]
struct NoDesktopInput {
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl InputBackend for NoDesktopInput {
    fn start(&self, _: EventHandler) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    fn start_loop(&self, mut owner: Box<dyn oink_input::capture_loop::CaptureLoop>) -> Result<(), InputError> {
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("oink-smoke-input".into())
            .spawn(move || {
                let thread = std::thread::current();
                owner.set_wake(Arc::new(move || thread.unpark()));
                owner.capture_available(false);
                let _ = ready.send(());
                loop {
                    let due = owner.poll(Instant::now());
                    if owner.stopped() {
                        break;
                    }
                    match due {
                        Some(due) => std::thread::park_timeout(due.saturating_duration_since(Instant::now())),
                        None => std::thread::park(),
                    }
                }
            })
            .map_err(|error| InputError::Platform(error.to_string()))?;
        *self.worker.lock().unwrap() = Some(worker);
        // Match the native capture startup contract: InputThread may read its
        // wake source immediately after start_loop returns.
        started
            .recv()
            .map_err(|_| InputError::Platform("smoke input worker failed during startup".into()))?;
        Ok(())
    }
    fn stop(&self) {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            // InputThread sends Stop and waits for its owner before stopping us.
            let _ = worker.join();
        }
    }
    fn capture_active(&self) -> Option<bool> {
        Some(false)
    }
    fn set_mode(&self, _: CaptureMode) -> Result<(), InputError> {
        Ok(())
    }
    fn inject(&self, _: &InjectEvent) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    fn cursor_position(&self) -> Result<Point, InputError> {
        Ok(Point::new(0.0, 0.0))
    }
    fn feel_profile(&self) -> FeelProfile {
        FeelProfile::default()
    }
    fn permissions(&self) -> InputPermissions {
        InputPermissions {
            accessibility: PermissionState::NotGranted,
            input_monitoring: PermissionState::NotGranted,
            secure_input_active: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    struct DelayedWake {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        installed: mpsc::Sender<oink_input::capture_loop::Wake>,
        stopped: Arc<AtomicBool>,
    }
    impl oink_input::capture_loop::CaptureLoop for DelayedWake {
        fn set_wake(&mut self, wake: oink_input::capture_loop::Wake) {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            self.installed.send(wake).unwrap();
        }
        fn event(&mut self, _: &oink_input::CapturedEvent) -> oink_input::Disposition {
            oink_input::Disposition::Pass
        }
        fn poll(&mut self, _: Instant) -> Option<Instant> {
            None
        }
        fn stopped(&self) -> bool {
            self.stopped.load(Ordering::Acquire)
        }
    }

    #[test]
    fn start_loop_waits_for_wake_registration() {
        let backend = Arc::new(NoDesktopInput::default());
        let stopped = Arc::new(AtomicBool::new(false));
        let (entered, entering) = mpsc::channel();
        let (release, releasing) = mpsc::channel();
        let (installed, wake) = mpsc::channel();
        let (returned, returning) = mpsc::channel();
        let owner = DelayedWake {
            entered,
            release: releasing,
            installed,
            stopped: stopped.clone(),
        };
        let starting_backend = backend.clone();
        let starter = std::thread::spawn(move || {
            returned.send(starting_backend.start_loop(Box::new(owner))).unwrap();
        });
        entering.recv_timeout(Duration::from_secs(2)).unwrap();
        // The worker is deliberately held inside set_wake, so startup must not
        // return and allow InputThread to access an uninitialized wake source.
        let premature = returning.recv_timeout(Duration::from_millis(100));
        let returned_early = premature.is_ok();
        release.send(()).unwrap();
        let result = premature.unwrap_or_else(|_| returning.recv_timeout(Duration::from_secs(2)).unwrap());
        let wake = wake.recv_timeout(Duration::from_secs(2)).unwrap();
        stopped.store(true, Ordering::Release);
        wake();
        backend.stop();
        starter.join().unwrap();
        result.unwrap();
        assert!(!returned_early, "startup returned before capture wake registration");
    }
}
