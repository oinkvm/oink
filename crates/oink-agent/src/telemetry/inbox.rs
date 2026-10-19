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
pub(super) struct Inbox {
    dir: PathBuf,
    grant: Grant,
    _watcher: notify::RecommendedWatcher,
}
impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").finish_non_exhaustive()
    }
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
fn recovered(dir: &Path, grant: &Grant, consent: ReportConsent, manifest: &Manifest) -> Option<(SafeCrash, u64)> {
    if grant.schema != 1
        || grant.generation != consent.generation
        || grant.policy_version != 3
        || grant.build.app_version != manifest.version
    {
        return None;
    }
    let capsule = oink_crash::capsule::recover_with_capability(
        &dir.join("capsule.bin"),
        consent,
        manifest,
        now(),
        grant.capability,
    );
    let registration: Option<Registration> = read(&dir.join("lifetime.json"));
    let mut native = None;
    {
        let registration = registration?;
        let lifetime = registration.lifetime;
        if registration.schema == 1
            && lifetime.schema == 1
            && registration.capability == grant.capability
            && lifetime.generation == grant.generation
            && lifetime.build == grant.build
            && lifetime.component == Component::FileProvider
            && lifetime.image == ImageSlot::FileProvider
            && lifetime.debug_id == grant.debug_id
            && lifetime.armed_at >= grant.armed_at
        {
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
    native.or(capsule)
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
        if let Some(old) = read::<Grant>(&dir.join("grant.json"))
            && let Some((crash, at)) = recovered(&dir, &old, consent, manifest)
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
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                wake.wake.notify_one();
            }
        })
        .map_err(|_| Error::Io)?;
        watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .map_err(|_| Error::Io)?;
        oink_telemetry::store::atomic_write(
            &dir.join("grant.json"),
            &serde_json::to_vec(&grant).map_err(|_| Error::Invalid)?,
        )?;
        Ok(Self {
            dir,
            grant,
            _watcher: watcher,
        })
    }
    pub(super) fn recover(&mut self, consent: ReportConsent, manifest: &Manifest, store: &mut Store) {
        if let Some((crash, at)) = recovered(&self.dir, &self.grant, consent, manifest) {
            let _ = store.observe(consent.generation, Report::SafeCrash(crash), at, now());
            // Consumed once even if quotas reject it; no second native/capsule event.
            let _ = std::fs::remove_file(self.dir.join("lifetime.json"));
            let _ = std::fs::remove_file(self.dir.join("capsule.bin"));
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
