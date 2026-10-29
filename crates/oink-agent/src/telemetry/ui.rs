//! Native UI capture owns local numeric records only; the authenticated backend
//! owns admission, consent generations, quotas and all HTTPS delivery.
use super::*;

#[derive(Debug)]
pub struct UiCapture {
    dir: PathBuf,
    manifest: Option<Manifest>,
    build: BuildContext,
    generation: u64,
    observed_generation: u64,
    #[cfg(windows)]
    local_instance: Option<[u8; 12]>,
    #[cfg(windows)]
    local_generation: u64,
    capture: Option<Arc<oink_crash::capsule::Capture>>,
    #[cfg(windows)]
    collector: Option<oink_crash::collector::Collector>,
}
impl UiCapture {
    pub fn new(data_dir: PathBuf) -> Self {
        let (version, manifest) = packaged_manifest();
        Self {
            dir: data_dir.join("reports"),
            manifest,
            build: BuildContext {
                app_version: version,
                os_version_class: projections::os_class(),
            },
            generation: 0,
            observed_generation: 0,
            #[cfg(windows)]
            local_instance: None,
            #[cfg(windows)]
            local_generation: 0,
            capture: None,
            #[cfg(windows)]
            collector: None,
        }
    }
    /// Call only with an applied status received from the authenticated backend,
    /// before forwarding the status to WebViews. Submit returned scrubbed records
    /// through submit_crash_capsule with this status's generation and original time.
    pub fn apply(&mut self, status: &ReportStatus) -> Result<Vec<(SafeCrash, u64)>> {
        if status.consent.generation < self.observed_generation {
            return Ok(Vec::new());
        }
        self.observed_generation = status.consent.generation;
        #[cfg(windows)]
        let applied = {
            let mut applied = *status;
            if applied.local_diagnostics_generation < self.local_generation {
                // An initial RPC can complete after diagnostic disable. Its
                // old local grant must not re-arm capture in the UI.
                applied.local_diagnostics_instance = self.local_instance;
            } else {
                self.local_generation = applied.local_diagnostics_generation;
            }
            applied
        };
        #[cfg(windows)]
        let status = &applied;

        if !status.configured || !status.consent.granted() || self.manifest.is_none() {
            self.shutdown();
            let _ = std::fs::remove_file(self.dir.join("capsules/ui.bin"));
            let _ = std::fs::remove_file(self.dir.join("capsules/ui-lifetime.json"));
            let _ = std::fs::remove_dir_all(self.dir.join("raw/ui"));
            #[cfg(windows)]
            self.start_local(status);
            return Ok(Vec::new());
        }
        if self.generation == status.consent.generation {
            #[cfg(windows)]
            if self.local_instance != status.local_diagnostics_instance {
                self.start_local(status);
            }
            return Ok(Vec::new());
        }
        self.shutdown();
        let manifest = self.manifest.as_ref().ok_or(Error::Disabled)?;
        let consent = status.consent;
        let capsule = self.dir.join("capsules/ui.bin");
        #[cfg(windows)]
        let mut recovered = oink_crash::staging::recover(&self.dir.join("raw/ui"), consent, manifest);
        #[cfg(not(windows))]
        let mut recovered = Vec::new();
        #[cfg(target_os = "macos")]
        {
            let path = self.dir.join("capsules/ui-lifetime.json");
            if let Ok(file) = std::fs::File::open(path) {
                use std::io::Read;
                let mut bytes = Vec::new();
                let _ = file.take(4097).read_to_end(&mut bytes);
                if bytes.len() <= 4096
                    && let Ok(lifetime) = serde_json::from_slice::<oink_crash::macos::Lifetime>(&bytes)
                    && lifetime.component == Component::UiRust
                    && lifetime.image == ImageSlot::UiRust
                    && let Some(home) = std::env::var_os("HOME")
                    && let Some(crash) = oink_crash::macos::recover(
                        &PathBuf::from(home).join("Library/Logs/DiagnosticReports"),
                        lifetime,
                        consent,
                        manifest,
                        now(),
                    )
                {
                    recovered.push(crash);
                }
            }
        }
        if recovered.is_empty()
            && let Some(crash) =
                oink_crash::capsule::recover(&capsule, consent, manifest, self.build.os_version_class, now())
            && crash.0.component == Component::UiRust
        {
            recovered.push(crash);
        }
        let capture = oink_crash::capsule::Capture::install();
        capture.arm(&capsule, consent.generation, Component::UiRust, self.build)?;
        self.capture = Some(capture);
        self.generation = consent.generation;
        #[cfg(windows)]
        self.start_local(status);
        #[cfg(target_os = "macos")]
        if let Ok(executable) = std::env::current_exe()
            && let Some(id) = oink_crash::macos::executable_uuid(&executable)
            && let Some(image) = manifest
                .images
                .iter()
                .find(|i| i.slot == ImageSlot::UiRust && i.debug_id == id)
            && let Some(launched_at) = oink_crash::macos::process_launch(std::process::id())
        {
            let lifetime = oink_crash::macos::Lifetime {
                schema: 1,
                pid: std::process::id(),
                launched_at,
                armed_at: now(),
                generation: consent.generation,
                component: Component::UiRust,
                image: ImageSlot::UiRust,
                debug_id: image.debug_id,
                build: self.build,
            };
            let bytes = serde_json::to_vec(&lifetime).map_err(|_| Error::Invalid)?;
            oink_telemetry::store::atomic_write(&self.dir.join("capsules/ui-lifetime.json"), &bytes)?;
        }
        Ok(recovered)
    }
    #[cfg(windows)]
    fn start_local(&mut self, status: &ReportStatus) {
        self.collector.take();
        self.local_instance = status.local_diagnostics_instance;
        let Some(manifest) = self.manifest.as_ref() else {
            return;
        };
        let Some(data) = self.dir.parent() else {
            return;
        };
        let current = oink_foundation::paths::StoragePaths::current();
        let paths = if data == current.data {
            current
        } else {
            oink_foundation::paths::StoragePaths::isolated(data.into())
        };
        let local = self.local_instance.as_ref().map(|instance| oink_crash::local::Target {
            root: paths.logs.join("Diagnostics/crash-ui"),
            instance: oink_crash::local::instance_hex(*instance),
        });
        if let Ok(executable) = std::env::current_exe()
            && let Some(parent) = executable.parent()
        {
            oink_crash::capsule::Capture::install();
            self.collector = oink_crash::collector::Collector::start_with_local(
                &parent.join("oink-crash-collector.exe"),
                &self.dir.join("raw/ui"),
                super::native_consent(status.consent, status.configured),
                self.build,
                manifest,
                Component::UiRust,
                local,
            )
            .ok();
        }
    }
    pub fn shutdown(&mut self) {
        let owned = self.generation != 0 || self.capture.is_some();
        self.generation = 0;
        #[cfg(windows)]
        self.collector.take();
        if let Some(capture) = self.capture.take() {
            capture.disarm();
        }
        if owned {
            let _ = std::fs::remove_file(self.dir.join("capsules/ui-lifetime.json"));
        }
    }
}
impl Drop for UiCapture {
    fn drop(&mut self) {
        // The process entry boundary commits an escaping Rust panic after stack
        // unwinding. Keep its armed numeric file until that terminal decision.
        if !std::thread::panicking() {
            self.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path) -> UiCapture {
        let version = AppVersion {
            major: 0,
            minor: 1,
            patch: 0,
            build: 1,
        };
        UiCapture {
            dir: root.join("reports"),
            manifest: Some(Manifest {
                version,
                images: vec![oink_telemetry::capture::Image {
                    slot: ImageSlot::UiRust,
                    debug_id: [7; 16],
                    debug_age: 0,
                    code_id: None,
                    text_size: 4096,
                }],
            }),
            build: BuildContext {
                app_version: version,
                os_version_class: OsVersionClass::Unknown,
            },
            generation: 0,
            observed_generation: 0,
            #[cfg(windows)]
            local_instance: None,
            #[cfg(windows)]
            local_generation: 0,
            capture: None,
            #[cfg(windows)]
            collector: None,
        }
    }
    fn granted() -> ReportStatus {
        ReportStatus {
            consent: ReportConsent {
                choice: Choice::Granted,
                policy_version: POLICY_VERSION,
                generation: 1,
            },
            configured: true,
            region: None,
            queued_performance: 0,
            queued_crashes: 0,
            delivery_disabled: false,
            rust_crash: Coverage::NumericCapsule,
            native_crash: Coverage::Unavailable,
            local_diagnostics_instance: None,
            local_diagnostics_generation: 0,
            file_provider: Coverage::Unavailable,
        }
    }
    #[cfg(windows)]
    #[test]
    fn late_status_cannot_restore_disabled_local_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let mut capture = fixture(root.path());
        let mut enabled = granted();
        enabled.consent.choice = Choice::Denied;
        enabled.local_diagnostics_instance = Some([1; 12]);
        enabled.local_diagnostics_generation = 1;
        capture.apply(&enabled).unwrap();
        assert_eq!(capture.local_instance, Some([1; 12]));
        let disabled = ReportStatus {
            local_diagnostics_instance: None,
            local_diagnostics_generation: 2,
            ..enabled
        };
        capture.apply(&disabled).unwrap();
        capture.apply(&enabled).unwrap();
        assert!(capture.local_instance.is_none());
        assert_eq!(capture.local_generation, 2);
    }
    #[test]
    fn ui_fatal_child_writes_only_scrubbed_numeric_capsule() {
        const CHILD: &str = "OINK_UI_FATAL_UNIT_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            fatal_boundary(|| {
                let mut capture = fixture(Path::new(&root));
                capture.apply(&granted()).unwrap();
                panic!("PRIVATE UI PANIC /PRIVATE/PATH");
            });
            unreachable!();
        }
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "telemetry::ui::tests::ui_fatal_child_writes_only_scrubbed_numeric_capsule",
                "--nocapture",
            ])
            .env(CHILD, directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let capture = fixture(directory.path());
        let path = capture.dir.join("capsules/ui.bin");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 128);
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE"));
        let (crash, _) = oink_crash::capsule::recover(
            &path,
            granted().consent,
            capture.manifest.as_ref().unwrap(),
            OsVersionClass::Unknown,
            now(),
        )
        .unwrap();
        assert_eq!(crash.component, Component::UiRust);
        assert_eq!(crash.exception_class, ExceptionClass::Panic);
    }
    #[test]
    fn ui_opt_out_is_generation_fenced_and_cleanup_stays_in_its_class() {
        let directory = tempfile::tempdir().unwrap();
        let mut capture = fixture(directory.path());
        capture.apply(&granted()).unwrap();
        assert!(capture.dir.join("capsules/ui.bin").exists());
        let protected = [
            "reports/capsules/agent.bin",
            "Logs/oink.2026-01-01.log",
            "Logs/Diagnostics/manual.zip",
            "Logs/Diagnostics/emergency.zip",
            "Logs/Diagnostics/buffer/segment.dseg",
        ];
        for name in protected {
            let path = directory.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"kept").unwrap();
        }
        std::fs::create_dir_all(capture.dir.join("raw/ui")).unwrap();
        std::fs::write(capture.dir.join("raw/ui/private.dmp"), b"private dump").unwrap();
        let mut denied = granted();
        denied.consent.choice = Choice::Denied;
        denied.consent.generation = 2;
        capture.apply(&denied).unwrap();
        assert!(!capture.dir.join("capsules/ui.bin").exists());
        assert!(!capture.dir.join("raw/ui").exists());
        capture.apply(&granted()).unwrap();
        assert!(capture.capture.is_none());
        assert!(!capture.dir.join("capsules/ui.bin").exists());
        for name in protected {
            assert_eq!(std::fs::read(directory.path().join(name)).unwrap(), b"kept");
        }
    }
}
