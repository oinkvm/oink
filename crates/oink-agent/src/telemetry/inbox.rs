//! OS-authenticated App Group inbox, separate from catalogs and content bridge.
use super::*;
use notify::Watcher;
use oink_crash::macos::Lifetime;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    schema: u16,
    policy_version: u16,
    generation: u64,
    capability: [u8; 32],
    agent_pid: u32,
    agent_launched_at: u64,
    armed_at: u64,
    debug_id: [u8; 16],
    build: BuildContext,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    schema: u16,
    capability: [u8; 32],
    lifetime: Lifetime,
}
impl Grant {
    fn accepts(&self, registration: &Registration) -> bool {
        let lifetime = &registration.lifetime;
        registration.schema == 1
            && lifetime.schema == 1
            && registration.capability == self.capability
            && lifetime.generation == self.generation
            && lifetime.build == self.build
            && lifetime.component == Component::FileProvider
            && lifetime.image == ImageSlot::FileProvider
            && lifetime.debug_id == self.debug_id
            && lifetime.armed_at >= self.armed_at
    }
}

pub(super) struct Inbox {
    dir: PathBuf,
    grant: Grant,
    _watcher: notify::RecommendedWatcher,
    changed: Arc<std::sync::atomic::AtomicBool>,
    watch_failed: Arc<std::sync::atomic::AtomicBool>,
    native_watched: bool,
    registration: Option<Registration>,
    gate: RecoveryGate,
}
#[derive(Default)]
struct RecoveryGate {
    alive: Option<bool>,
}
impl RecoveryGate {
    fn scan(&mut self, changed: bool, alive: bool, native_watched: bool) -> bool {
        let needed = changed || self.alive != Some(alive) || (!alive && !native_watched);
        self.alive = Some(alive);
        needed
    }
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").finish_non_exhaustive()
    }
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

fn recovered(
    dir: &Path,
    grant: &Grant,
    consent: ReportConsent,
    manifest: &Manifest,
    registration: Option<&Registration>,
) -> Option<(SafeCrash, u64)> {
    if grant.schema != 1
        || grant.generation != consent.generation
        || grant.policy_version != 3
        || grant.build.app_version != manifest.version
    {
        return None;
    }
    let mut native = None;
    {
        let registration = registration?;
        let lifetime = registration.lifetime;
        if grant.accepts(registration) {
            // An active process is not a fatal incident. Do not upload a caught
            // panic/old capsule or scan unrelated OS records while it is alive.
            if oink_crash::macos::process_launch(lifetime.pid) == Some(lifetime.launched_at) {
                return None;
            }
            if let Some(home) = std::env::var_os("HOME") {
                native = oink_crash::macos::recover(
                    &PathBuf::from(home).join("Library/Logs/DiagnosticReports"),
                    lifetime,
                    consent,
                    manifest,
                    now(),
                );
            }
        } else {
            return None;
        }
    }
    native.or_else(|| {
        oink_crash::capsule::recover_with_capability(
            &dir.join("capsule.bin"),
            consent,
            manifest,
            now(),
            grant.capability,
        )
    })
}
impl Inbox {
    pub(super) fn open(
        root: &Path,
        consent: ReportConsent,
        manifest: &Manifest,
        ingress: Arc<Ingress>,
        store: &mut Store,
    ) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let image = manifest
            .images
            .iter()
            .find(|i| i.slot == ImageSlot::FileProvider)
            .ok_or(Error::Invalid)?;
        let dir = root.join("Reports");
        std::fs::create_dir_all(&dir).map_err(|_| Error::Io)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|_| Error::Io)?;
        if let Ok(Some(old)) = read::<Grant>(&dir.join("grant.json"))
            && let Some((crash, at)) = recovered(
                &dir,
                &old,
                consent,
                manifest,
                read::<Registration>(&dir.join("lifetime.json")).ok().flatten().as_ref(),
            )
        {
            let _ = store.observe(consent.generation, Report::SafeCrash(crash), at, now());
        }
        for file in ["grant.json", "lifetime.json", "capsule.bin"] {
            let _ = std::fs::remove_file(dir.join(file));
        }
        let mut capability = [0; 32];
        getrandom::fill(&mut capability).map_err(|_| Error::Entropy)?;
        let grant = Grant {
            schema: 1,
            policy_version: 3,
            generation: consent.generation,
            capability,
            agent_pid: std::process::id(),
            agent_launched_at: oink_crash::macos::process_launch(std::process::id()).ok_or(Error::Invalid)?,
            armed_at: now(),
            debug_id: image.debug_id,
            build: ingress.build,
        };
        let wake = ingress.clone();
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let watch_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dirty = changed.clone();
        let failed = watch_failed.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.as_ref().is_ok_and(|event| event.kind.is_access()) {
                return;
            }
            if event.is_err() || event.as_ref().is_ok_and(|event| event.need_rescan()) {
                failed.store(true, Ordering::Release);
            }
            dirty.store(true, Ordering::Release);
            wake.wake.notify_one();
        })
        .map_err(|_| Error::Io)?;
        watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .map_err(|_| Error::Io)?;
        // Apple may publish the .ips after process death. Watch its directory as
        // well as the capsule inbox; keep polling recovery if this watch fails.
        let native_watched = std::env::var_os("HOME").is_some_and(|home| {
            watcher
                .watch(
                    &PathBuf::from(home).join("Library/Logs/DiagnosticReports"),
                    notify::RecursiveMode::NonRecursive,
                )
                .is_ok()
        });
        oink_telemetry::store::atomic_write(
            &dir.join("grant.json"),
            &serde_json::to_vec(&grant).map_err(|_| Error::Invalid)?,
        )?;
        Ok(Self {
            dir,
            grant,
            _watcher: watcher,
            changed,
            watch_failed,
            native_watched,
            registration: None,
            gate: RecoveryGate::default(),
        })
    }
    pub(super) fn recover(&mut self, consent: ReportConsent, manifest: &Manifest, store: &mut Store) {
        let failed = self.watch_failed.load(Ordering::Acquire);
        let changed = self.changed.swap(false, Ordering::AcqRel) || failed;
        if changed {
            match read(&self.dir.join("lifetime.json")) {
                Ok(registration) => self.registration = registration,
                Err(_) => {
                    // A transient I/O failure is not a stable absent binding.
                    // Retry on the next ordinary wake even without a new event.
                    self.changed.store(true, Ordering::Release);
                    return;
                }
            }
        }
        let alive = self
            .registration
            .as_ref()
            .filter(|r| self.grant.accepts(r))
            .is_some_and(|r| oink_crash::macos::process_launch(r.lifetime.pid) == Some(r.lifetime.launched_at));
        // Liveness remains observed: a death need not modify the inbox. Ordinary
        // performance reports, however, are not reasons to rescan crash files.
        if !self.gate.scan(changed, alive, self.native_watched && !failed) {
            return;
        }
        if let Some((crash, at)) = recovered(&self.dir, &self.grant, consent, manifest, self.registration.as_ref()) {
            let _ = store.observe(consent.generation, Report::SafeCrash(crash), at, now());
            // Consumed once even if quotas reject it; no second native/capsule event.
            let _ = std::fs::remove_file(self.dir.join("lifetime.json"));
            let _ = std::fs::remove_file(self.dir.join("capsule.bin"));
            self.registration = None;
        }
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        for file in ["grant.json", "lifetime.json", "capsule.bin"] {
            let _ = std::fs::remove_file(self.dir.join(file));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "slow: requires an injected watcher notification stream"]
    fn native_inbox_watch_observes_registration_creation_and_removal() {
        let directory = tempfile::tempdir().unwrap();
        let owner = Owner::new(directory.path(), ReportConsent::default());
        let consent = ReportConsent {
            choice: Choice::Granted,
            policy_version: POLICY_VERSION,
            generation: 1,
        };
        let manifest = Manifest {
            version: owner.ingress.build.app_version,
            images: vec![oink_telemetry::capture::Image {
                slot: ImageSlot::FileProvider,
                debug_id: [7; 16],
                debug_age: 0,
                code_id: None,
                text_size: 4096,
            }],
        };
        let mut store = Store::open(&directory.path().join("test-store"), consent, now()).unwrap();
        let mut inbox = Inbox::open(directory.path(), consent, &manifest, owner.ingress.clone(), &mut store).unwrap();
        inbox.recover(consent, &manifest, &mut store);
        assert!(inbox.registration.is_none());
        let lifetime = Lifetime {
            schema: 1,
            pid: std::process::id(),
            launched_at: oink_crash::macos::process_launch(std::process::id()).unwrap(),
            armed_at: now(),
            generation: 1,
            component: Component::FileProvider,
            image: ImageSlot::FileProvider,
            debug_id: [7; 16],
            build: owner.ingress.build,
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema":1, "capability":inbox.grant.capability, "lifetime":lifetime,
        }))
        .unwrap();
        oink_telemetry::store::atomic_write(&inbox.dir.join("lifetime.json"), &bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while inbox.registration.is_none() {
            assert!(Instant::now() < deadline, "native watcher missed registration creation");
            std::thread::sleep(Duration::from_millis(20));
            inbox.recover(consent, &manifest, &mut store);
        }
        assert_eq!(inbox.gate.alive, Some(true));
        std::fs::remove_file(inbox.dir.join("lifetime.json")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while inbox.registration.is_some() {
            assert!(Instant::now() < deadline, "native watcher missed registration removal");
            std::thread::sleep(Duration::from_millis(20));
            inbox.recover(consent, &manifest, &mut store);
        }
        // An unreadable replacement must not become a cached negative result.
        std::fs::create_dir(inbox.dir.join("lifetime.json")).unwrap();
        inbox.changed.store(true, Ordering::Release);
        inbox.recover(consent, &manifest, &mut store);
        assert!(inbox.changed.load(Ordering::Acquire));
        std::fs::remove_dir(inbox.dir.join("lifetime.json")).unwrap();
        inbox.recover(consent, &manifest, &mut store);
        assert!(inbox.registration.is_none());
    }

    #[test]
    fn recovery_waits_for_changes_but_detects_death_and_late_native_reports() {
        let mut gate = RecoveryGate::default();
        assert!(gate.scan(true, true, true));
        for _ in 0..60 {
            assert!(!gate.scan(false, true, true));
        }
        assert!(gate.scan(false, false, true), "death without an inbox write");
        for _ in 0..60 {
            assert!(!gate.scan(false, false, true));
        }
        assert!(gate.scan(true, false, true), "late Apple report");
        assert!(gate.scan(true, true, true), "replacement lifetime");
        assert!(gate.scan(false, false, false));
        assert!(gate.scan(false, false, false), "failed watch retains recovery fallback");
    }
}
