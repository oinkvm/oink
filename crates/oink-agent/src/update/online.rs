//! macOS online updates without Sparkle: the signed v2 catalog from the
//! compiled sources, and the DMG it names. A downloaded DMG must match the
//! catalog's length, SHA-256 and Ed25519 signature, then goes through the same
//! staging (Developer ID + notarization) and install path as a dropped one.
use super::*;
use oink_update::{
    catalog::{self, Check, Expected, Kind, OsVersion, Outcome, Plan, Platform, Release, Selection},
    feed::{Channel, Severity, Trust, Watermark},
    fetch,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Weak, mpsc},
};

const AFTER_SUCCESS: Duration = Duration::from_secs(24 * 60 * 60);
const AFTER_FAILURE: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Debug, Default)]
pub(super) struct Online {
    /// The signed release behind the offered feed candidate.
    release: Option<Release>,
    /// Each check or download runs in a generation; a superseded result is dropped.
    pub(super) generation: u64,
    abort: Option<Arc<AtomicBool>>,
    /// Next automatic check. `None` while the feed is not configured.
    due: Option<Instant>,
    wake: Option<mpsc::SyncSender<()>>,
}
#[derive(Debug)]
pub(super) struct Download {
    release: Release,
    generation: u64,
    abort: Arc<AtomicBool>,
}

/// Catalog and package transfers; HTTPS in production.
pub(super) trait Transport {
    fn catalog(&mut self, url: &str) -> Result<Vec<u8>, Error>;
    /// Stops early (with an error) when `progress` returns false.
    fn package(&mut self, url: &str, length: u64, progress: &mut dyn FnMut(u64, u64) -> bool)
    -> Result<Vec<u8>, Error>;
}
pub(super) struct Feed<'a> {
    pub sources: &'a [String],
    pub trust: Trust<'a>,
    pub architecture: &'a str,
    pub os: OsVersion,
}
pub(super) struct Query<'a> {
    pub channel: Channel,
    pub seen: &'a Watermark,
    pub now: i64,
    pub installed_build: u64,
    pub skipped_build: Option<u64>,
    pub check: Check,
}
/// The first source whose catalog verifies, and what it offers this installation.
pub(super) fn evaluate(
    feed: &Feed<'_>,
    query: &Query<'_>,
    transport: &mut impl Transport,
) -> Result<(Watermark, Result<Plan, Outcome>), Error> {
    let path = catalog::path(Platform::Mac, feed.trust.epoch, feed.architecture, query.channel);
    let expected = Expected {
        platform: Platform::Mac,
        architecture: feed.architecture,
        channel: query.channel,
    };
    let verified = fetch::first_verified(
        feed.sources,
        &path,
        |url| transport.catalog(url),
        |bytes| catalog::verify(bytes, &feed.trust, &expected, query.seen, query.now),
    )?;
    // A macOS install either completes or leaves the old bundle running; there
    // is no failed-install quarantine, and the installed build is the successful one.
    let quarantine = BTreeSet::new();
    let plan = verified.plan(&Selection {
        installed_build: query.installed_build,
        successful_build: query.installed_build,
        quarantine: &quarantine,
        channel: query.channel,
        os: feed.os,
        skipped_build: query.skipped_build,
        check: query.check,
    });
    Ok((verified.watermark().clone(), plan))
}
/// The package bytes, only once they match the signed release exactly.
pub(super) fn fetch_package(
    release: &Release,
    trust: &Trust<'_>,
    transport: &mut impl Transport,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, Error> {
    let bytes = transport.package(&release.package.url, release.package.length, progress)?;
    oink_update::feed::verify_package(&bytes, &release.package, trust)?;
    Ok(bytes)
}
fn candidate(plan: &Plan) -> Candidate {
    Candidate {
        local_package: false,
        build: plan.release.build,
        version: plan.release.version.clone(),
        size: plan.release.package.length,
        published_at: None,
        severity: if plan.kind == Kind::Critical {
            Severity::Critical
        } else {
            Severity::Normal
        },
        notes: Some(plan.release.notes.clone()),
        kind: plan.kind,
        notice: plan.notice.clone(),
    }
}

impl Controller {
    pub(super) fn finish_check(
        &mut self,
        generation: u64,
        result: Result<Result<Plan, Outcome>, Error>,
        now: i64,
        at: Instant,
    ) -> bool {
        if generation != self.online.generation || self.local_mode || self.status.phase != Phase::Checking {
            return false;
        }
        let success = result.is_ok();
        match result {
            Ok(Ok(plan)) => {
                if self.status.failed_build != Some(plan.release.build) {
                    self.status.failed_build = None;
                }
                self.status.candidate = Some(candidate(&plan));
                self.online.release = Some(plan.release);
                self.status.phase = Phase::Available;
            }
            Ok(Err(outcome)) => {
                self.status.candidate = None;
                self.online.release = None;
                self.status.failed_build = None;
                (self.status.phase, self.status.error) = match outcome {
                    Outcome::UpToDate => (Phase::Idle, None),
                    Outcome::OsUnsupported => (Phase::Failed, Some(Error::OsUnsupported)),
                };
            }
            // The offered candidate (and a download failure for it) stays.
            Err(error) => {
                self.status.phase = Phase::Failed;
                self.status.error = Some(error);
            }
        }
        if success {
            self.status.checked_at = Some(now);
        }
        self.online.due = Some(at + if success { AFTER_SUCCESS } else { AFTER_FAILURE });
        self.publish_schedule();
        true
    }
    /// `None` joins a download that is already running or finished.
    pub(super) fn begin_download(&mut self) -> Result<Option<Download>, Error> {
        let candidate = self.status.candidate.as_ref().ok_or(Error::Policy)?;
        // A manual update is installed by the user from its release page.
        if candidate.local_package || candidate.kind == Kind::Manual {
            return Err(Error::Policy);
        }
        match self.status.phase {
            Phase::Downloading | Phase::Verifying | Phase::Ready => return Ok(None),
            Phase::Available | Phase::Failed | Phase::Checking => {}
            _ => return Err(Error::Policy),
        }
        let release = self.online.release.clone().ok_or(Error::Policy)?;
        self.online.generation += 1;
        let abort = Arc::new(AtomicBool::new(false));
        self.online.abort = Some(abort.clone());
        self.status.phase = Phase::Downloading;
        self.status.progress = Some(0);
        self.status.error = None;
        self.status.blockers.clear();
        Ok(Some(Download {
            release,
            generation: self.online.generation,
            abort,
        }))
    }
    fn download_progress(&mut self, generation: u64, percent: u8) -> bool {
        if generation != self.online.generation || self.status.phase != Phase::Downloading {
            return false;
        }
        self.status.progress = Some(percent);
        true
    }
    fn download_verified(&mut self, generation: u64) -> bool {
        if generation != self.online.generation || self.status.phase != Phase::Downloading {
            return false;
        }
        self.status.phase = Phase::Verifying;
        self.status.progress = None;
        true
    }
    /// Returns whether the result was applied, and a superseded staged package to drop.
    pub(super) fn finish_download(
        &mut self,
        generation: u64,
        build: u64,
        result: Result<local::Staged, Error>,
    ) -> (bool, Option<local::Staged>) {
        if generation != self.online.generation || self.local_mode {
            return (false, result.ok());
        }
        self.online.abort = None;
        self.status.progress = None;
        match result {
            Ok(staged) => {
                self.local = Some(staged);
                self.status.phase = Phase::Ready;
                self.status.error = None;
                self.status.failed_build = None;
            }
            Err(error) => {
                self.status.phase = Phase::Failed;
                self.status.error = Some(error);
                self.status.failed_build = Some(build);
            }
        }
        (true, None)
    }
    /// Cancel during download or verification, or discard a downloaded package.
    pub(super) fn cancel_download(&mut self) {
        if !self.local_mode && matches!(self.status.phase, Phase::Downloading | Phase::Verifying | Phase::Ready) {
            self.stop_download();
            self.status.phase = Phase::Available;
            self.status.error = None;
        }
    }
    fn stop_download(&mut self) {
        self.online.generation += 1;
        if let Some(abort) = self.online.abort.take() {
            abort.store(true, Ordering::Release);
        }
        if let Some(staged) = self.local.take() {
            // Removing a staged app tree is slow; never under the update lock.
            std::thread::spawn(move || drop(staged));
        }
        self.status.progress = None;
    }
    /// Drop the feed candidate and anything running for it (channel change,
    /// skip, or a dropped package taking over).
    pub(super) fn supersede(&mut self) {
        self.stop_download();
        self.online.release = None;
        self.status.candidate = None;
        self.status.failed_build = None;
        if matches!(
            self.status.phase,
            Phase::Checking | Phase::Available | Phase::Downloading | Phase::Verifying | Phase::Ready | Phase::Failed
        ) {
            self.status.phase = Phase::Idle;
        }
    }
    /// Run the next automatic check now (checks were just turned on).
    pub(super) fn schedule_now(&mut self) {
        if self.online.due.is_some() {
            self.online.due = Some(Instant::now());
            if let Some(wake) = &self.online.wake {
                let _ = wake.try_send(());
            }
        }
    }
    pub(super) fn publish_schedule(&mut self) {
        self.status.next_check = self
            .automatic_wait(Instant::now())
            .map(|wait| now().saturating_add(wait.as_secs() as i64));
    }
    /// Time until the next automatic check; `None` when there is none.
    fn automatic_wait(&self, now: Instant) -> Option<Duration> {
        let due = self.online.due.filter(|_| self.status.checks_enabled)?;
        Some(due.saturating_duration_since(now))
    }
}

pub(super) fn attach(node: &Node) {
    let configured = cfg!(target_os = "macos")
        && oink_update::trust::mac_enabled()
        && oink_update::trust::sources().is_ok()
        && oink_update::trust::mac_compiled().is_ok();
    if !configured {
        return;
    }
    let (wake, receive) = mpsc::sync_channel(1);
    let mut state = lock(&node.inner.update);
    if !state.restored_result {
        state.status.error = None;
        state.status.phase = Phase::Idle;
    }
    state.status.checks_enabled = state.policy.preferences.checks_enabled;
    state.online.wake = Some(wake);
    // One check at every launch.
    state.online.due = Some(Instant::now());
    state.publish_schedule();
    let status = state.status.clone();
    drop(state);
    node.emit(ApiEvent::UpdateStatusChanged(status));
    let weak = Arc::downgrade(&node.inner);
    std::thread::spawn(move || schedule(weak, receive));
}
/// The only timer: wait for the next automatic check or a preference change.
fn schedule(weak: Weak<crate::node::Inner>, wake: mpsc::Receiver<()>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let node = Node { inner };
        let wait = lock(&node.inner.update).automatic_wait(Instant::now());
        if wait == Some(Duration::ZERO) {
            automatic(&node);
            continue;
        }
        drop(node);
        let disconnected = match wait {
            Some(wait) => wake.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Disconnected),
            None => wake.recv().is_err(),
        };
        if disconnected {
            return;
        }
    }
}
fn automatic(node: &Node) {
    let mut state = lock(&node.inner.update);
    let Ok(Some(generation)) = state.begin_check() else {
        // A check, download, install or dropped package owns the dialog now.
        state.online.due = Some(Instant::now() + AFTER_SUCCESS);
        state.publish_schedule();
        return;
    };
    let status = state.status.clone();
    drop(state);
    node.emit(ApiEvent::UpdateStatusChanged(status));
    run_check(node, generation, Check::Automatic);
}
pub(super) fn spawn_check(node: &Node, generation: u64, check: Check) {
    let node = node.clone();
    std::thread::spawn(move || run_check(&node, generation, check));
}
fn run_check(node: &Node, generation: u64, check: Check) {
    let state = lock(&node.inner.update);
    let channel = state.policy.preferences.channel();
    let seen = state
        .policy
        .watermarks
        .get(channel.as_str())
        .cloned()
        .unwrap_or_default();
    let (installed_build, skipped_build) = (state.status.current_build, state.policy.skipped_build);
    drop(state);
    let result = (|| {
        let roots = oink_update::trust::mac_compiled()?;
        let sources = oink_update::trust::sources()?;
        let feed = Feed {
            sources: &sources,
            trust: roots.feed(),
            architecture: architecture(),
            os: os_version()?,
        };
        let query = Query {
            channel,
            seen: &seen,
            now: now(),
            installed_build,
            skipped_build,
            check,
        };
        evaluate(&feed, &query, &mut Https)
    })();
    tracing::info!(
        ?check,
        channel = channel.as_str(),
        offered = ?result.as_ref().map(|(_, plan)| plan.as_ref().map(|p| (p.release.build, p.kind))),
        "update check finished"
    );
    let mut state = lock(&node.inner.update);
    if state.online.generation != generation {
        return;
    }
    let result = result.and_then(|(watermark, plan)| {
        let previous = state.policy.clone();
        state.policy.watermarks.insert(channel.as_str().into(), watermark);
        if let Err(error) = state.save(node.data_dir()) {
            state.policy = previous;
            return Err(error);
        }
        Ok(plan)
    });
    if state.finish_check(generation, result, now(), Instant::now()) {
        let status = state.status.clone();
        drop(state);
        node.emit(ApiEvent::UpdateStatusChanged(status));
    }
}
pub(super) fn spawn_download(node: &Node, download: Download) {
    let node = node.clone();
    std::thread::spawn(move || run_download(&node, download));
}
fn run_download(node: &Node, download: Download) {
    let Download {
        release,
        generation,
        abort,
    } = download;
    tracing::info!(build = release.build, "update download started");
    let emit = |state: std::sync::MutexGuard<'_, Controller>| {
        let status = state.status.clone();
        drop(state);
        node.emit(ApiEvent::UpdateStatusChanged(status));
    };
    let result = (|| {
        let roots = oink_update::trust::mac_compiled()?;
        let mut shown = 0;
        let bytes = fetch_package(&release, &roots.feed(), &mut Https, &mut |received, expected| {
            if abort.load(Ordering::Acquire) {
                return false;
            }
            let percent = (received.saturating_mul(100) / expected.max(1)).min(100) as u8;
            if percent != shown {
                shown = percent;
                let mut state = lock(&node.inner.update);
                if state.download_progress(generation, percent) {
                    emit(state);
                }
            }
            true
        })?;
        let mut state = lock(&node.inner.update);
        if !state.download_verified(generation) {
            return Err(Error::Busy);
        }
        let (version, current_build) = (state.status.version.clone(), state.status.current_build);
        emit(state);
        stage(&bytes, &release, &version, current_build)
    })();
    tracing::info!(build = release.build, error = ?result.as_ref().err(), "update download finished");
    let mut state = lock(&node.inner.update);
    let (applied, superseded) = state.finish_download(generation, release.build, result);
    if applied {
        emit(state);
    } else {
        drop(state);
    }
    drop(superseded);
}

/// The DMG as downloaded and verified, staged next to the installed bundle.
#[cfg(target_os = "macos")]
fn stage(bytes: &[u8], release: &Release, version: &str, current_build: u64) -> Result<local::Staged, Error> {
    stage_into(&super::local_mac::bundle()?, bytes, release, version, current_build)
}
#[cfg(target_os = "macos")]
fn stage_into(
    destination: &Path,
    bytes: &[u8],
    release: &Release,
    version: &str,
    current_build: u64,
) -> Result<local::Staged, Error> {
    let staged = super::local_mac::stage_image(destination, |image| {
        std::fs::write(image, bytes).map_err(|_| Error::Storage)?;
        Ok(bytes.len() as u64)
    })?;
    staged
        .package
        .validate_codesigned_dmg(version, current_build, architecture())?;
    // The signed catalog and the authenticated bundle must name the same build.
    if staged.package.build != release.build {
        return Err(Error::CorruptPackage);
    }
    Ok(staged)
}
#[cfg(not(target_os = "macos"))]
fn stage(_: &[u8], _: &Release, _: &str, _: u64) -> Result<local::Staged, Error> {
    Err(Error::OsUnsupported)
}
#[cfg(target_os = "macos")]
use super::local_mac::architecture;
#[cfg(not(target_os = "macos"))]
fn architecture() -> &'static str {
    "arm64"
}
#[cfg(target_os = "macos")]
fn os_version() -> Result<OsVersion, Error> {
    let mut buffer = [0u8; 64];
    let mut length = buffer.len();
    // SAFETY: a read-only sysctl into a bounded local buffer; `length` carries its size.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(Error::OsUnsupported);
    }
    let text = std::ffi::CStr::from_bytes_until_nul(&buffer[..length.min(buffer.len())])
        .ok()
        .and_then(|text| text.to_str().ok())
        .ok_or(Error::OsUnsupported)?;
    OsVersion::parse(text)
}
#[cfg(not(target_os = "macos"))]
fn os_version() -> Result<OsVersion, Error> {
    Err(Error::OsUnsupported)
}
struct Https;
impl Transport for Https {
    #[cfg(target_os = "macos")]
    fn catalog(&mut self, url: &str) -> Result<Vec<u8>, Error> {
        fetch::get(url, oink_update::feed::MAX_FEED as u64, false, &mut |_, _| {})
    }
    #[cfg(target_os = "macos")]
    fn package(
        &mut self,
        url: &str,
        length: u64,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Vec<u8>, Error> {
        fetch::get_until(url, length, true, progress)
    }
    #[cfg(not(target_os = "macos"))]
    fn catalog(&mut self, _: &str) -> Result<Vec<u8>, Error> {
        Err(Error::OsUnsupported)
    }
    #[cfg(not(target_os = "macos"))]
    fn package(&mut self, _: &str, _: u64, _: &mut dyn FnMut(u64, u64) -> bool) -> Result<Vec<u8>, Error> {
        Err(Error::OsUnsupported)
    }
}

#[cfg(test)]
mod tests;
