//! Observation only: no readiness decisions, content reads, or waits on AppKit.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

// Local diagnostic codes, independent of protocol enums and file_stage.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum Event {
    PerformEnter = 1,
    PerformReturn = 2,
    ReceiveEnter = 3,
    ReceiveReturn = 4,
    MainHeartbeat = 5,
    Reader = 6,
    FileFirstObserved = 7,
    FileChanged = 8,
    QueueSentinel = 9,
    ObserverStopped = 10,
    ObserverError = 11,
    FileRemoved = 12,
    ObserverReady = 14,
}

#[derive(Clone)]
pub(super) struct Trace {
    request: u64,
    created: Instant,
    native_sequence: i64,
    dispatch: tracing::Dispatch,
}
impl Trace {
    pub(super) fn new(request: u64, created: Instant) -> Self {
        Self {
            request,
            created,
            native_sequence: 0,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
        }
    }
    pub(super) fn event(&self, event: Event, index: usize, delay: Duration, status: i64) {
        self.record(event, index, delay, status, None);
    }
    fn record(&self, event: Event, index: usize, delay: Duration, status: i64, metadata: Option<&fs::Metadata>) {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        let at = ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64;
        tracing::dispatcher::with_default(&self.dispatch, || {
            tracing::info!(target: "oink_file_diagnostics", file_promise_event = event as u8,
                file_request = self.request, promise_native_sequence = self.native_sequence,
                file_index = index, promise_at_us = at,
                elapsed_us = self.created.elapsed().as_micros() as u64,
                promise_delay_us = delay.as_micros() as u64, file_status = status,
                file_size = metadata.map(fs::Metadata::len),
                file_mtime_ns = metadata.map(|m| m.mtime().saturating_mul(1_000_000_000).saturating_add(m.mtime_nsec())),
                file_kind = metadata.map(|m| if m.is_file() { 1_u8 } else if m.is_dir() { 2 } else { 4 }),
                "native promise timing observation");
        });
    }
    pub(super) fn sentinel(&self, queue: &NSOperationQueue, index: usize) {
        if !tracing::enabled!(target: "oink_file_diagnostics", tracing::Level::INFO) {
            return;
        }
        let trace = self.clone();
        let queued = Instant::now();
        let block = block2::RcBlock::new(move || trace.event(Event::QueueSentinel, index, queued.elapsed(), 0));
        // SAFETY: Foundation copies the block; it captures only owned diagnostics.
        unsafe {
            queue.addOperationWithBlock(&block);
        }
    }
}

pub(in crate::macos::dnd) struct Perform {
    trace: Option<Trace>,
    entered: Instant,
}
pub(in crate::macos::dnd) fn perform(native_sequence: i64) -> Option<Perform> {
    if !tracing::enabled!(target: "oink_file_diagnostics", tracing::Level::INFO) {
        return None;
    }
    let entered = Instant::now();
    let mut trace = Trace::new(0, entered);
    trace.native_sequence = native_sequence;
    trace.event(Event::PerformEnter, 0, Duration::ZERO, 0);
    Some(Perform {
        trace: Some(trace),
        entered,
    })
}
impl Perform {
    pub(in crate::macos::dnd) fn bind(&mut self, request: u64) {
        if let Some(trace) = &mut self.trace {
            trace.request = request;
        }
    }
    pub(super) fn finish(&mut self, enqueue: impl FnOnce(Box<dyn FnOnce() + Send>)) {
        let Some(trace) = self.trace.take() else {
            return;
        };
        trace.event(Event::PerformReturn, 0, self.entered.elapsed(), 0);
        let queued = Instant::now();
        enqueue(Box::new(move || {
            trace.event(Event::MainHeartbeat, 0, queued.elapsed(), 0)
        }));
    }
}
impl Drop for Perform {
    fn drop(&mut self) {
        // Always defer, even on AppKit's thread. on_main would run inline and
        // measure no queue turn. This is a diagnostic, not a drag-loop workaround;
        // neither it nor the directory observer completes/cancels the export.
        self.finish(|job| super::super::queue_main(move |_| job()));
    }
}

struct Entry {
    file: fs::File,
    index: usize,
    size: u64,
    mtime: (i64, i64),
}
fn register(queue: &OwnedFd, file: &fs::File) -> io::Result<()> {
    let change = libc::kevent {
        ident: file.as_raw_fd() as usize,
        filter: libc::EVFILT_VNODE,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_WRITE
            | libc::NOTE_EXTEND
            | libc::NOTE_ATTRIB
            | libc::NOTE_DELETE
            | libc::NOTE_RENAME
            | libc::NOTE_REVOKE,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: one valid input event; no output buffer or timeout is supplied.
    if unsafe { libc::kevent(queue.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn open(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EVTONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

pub(super) fn observe(export: &Arc<Export>, trace: Trace) {
    if !tracing::enabled!(target: "oink_file_diagnostics", tracing::Level::INFO) {
        return;
    }
    let path = export.batch.path().join("incoming");
    let export = Arc::downgrade(export);
    // Startup is asynchronous: first observed is not kernel creation time. A
    // bounded metadata-only kqueue observer never gates receipt, holds a cache
    // lease, or treats a quiet file as complete. Events may be coalesced by OS.
    let failed_trace = trace.clone();
    let spawned = std::thread::Builder::new()
        .name("promise-observer".into())
        .spawn(move || {
            let result = observe_inner(
                &path,
                &trace,
                || {
                    export
                        .upgrade()
                        .is_none_or(|export| export.retired.load(Ordering::Acquire))
                },
                Some(Duration::from_secs(30)),
            );
            if let Err(ref error) = result {
                trace.event(
                    Event::ObserverError,
                    0,
                    Duration::ZERO,
                    i64::from(error.raw_os_error().unwrap_or(0)),
                );
            }
            trace.event(Event::ObserverStopped, 0, Duration::ZERO, i64::from(result.is_err()));
        });
    if let Err(error) = spawned {
        failed_trace.event(
            Event::ObserverError,
            0,
            Duration::ZERO,
            i64::from(error.raw_os_error().unwrap_or(0)),
        );
    }
}
// The production caller bounds diagnostics to 30 seconds. Tests drive the same
// kqueue loop with explicit cancellation, without a scheduling deadline.
pub(super) fn observe_inner(
    path: &Path,
    trace: &Trace,
    stop: impl Fn() -> bool,
    lifetime: Option<Duration>,
) -> io::Result<()> {
    // SAFETY: kqueue creates a fresh descriptor; immediately wrap its ownership.
    let fd = unsafe { libc::kqueue() };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a newly allocated, uniquely owned kqueue descriptor.
    let queue = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl sets only this observer's close-on-exec descriptor flag.
    if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = open(path)?;
    register(&queue, &directory)?;
    trace.event(Event::ObserverReady, 0, Duration::ZERO, 0);
    let until = lifetime.map(|lifetime| Instant::now() + lifetime);
    let mut entries: BTreeMap<(u64, u64), Entry> = BTreeMap::new();
    let mut next = 0;
    let mut budget = 256_usize;
    loop {
        let mut seen = BTreeSet::new();
        for child in fs::read_dir(path)?.take(129) {
            let child = child?;
            let metadata = match fs::symlink_metadata(child.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            // Never follow/open symlinks or special files merely for diagnostics.
            if !metadata.is_file() && !metadata.is_dir() {
                continue;
            }
            let identity = (metadata.dev(), metadata.ino());
            seen.insert(identity);
            if let std::collections::btree_map::Entry::Vacant(entry) = entries.entry(identity) {
                if next >= 128 {
                    return Err(io::Error::from_raw_os_error(libc::E2BIG));
                }
                let file = open(&child.path())?;
                register(&queue, &file)?;
                next += 1;
                let metadata = file.metadata()?;
                trace.record(Event::FileFirstObserved, next, Duration::ZERO, 0, Some(&metadata));
                budget = budget.saturating_sub(1);
                entry.insert(Entry {
                    file,
                    index: next,
                    size: metadata.len(),
                    mtime: (metadata.mtime(), metadata.mtime_nsec()),
                });
            }
            let entry = entries.get_mut(&identity).unwrap();
            let metadata = entry.file.metadata()?;
            let mtime = (metadata.mtime(), metadata.mtime_nsec());
            if metadata.len() != entry.size || mtime != entry.mtime {
                trace.record(Event::FileChanged, entry.index, Duration::ZERO, 0, Some(&metadata));
                entry.size = metadata.len();
                entry.mtime = mtime;
                budget = budget.saturating_sub(1);
            }
            if budget == 0 {
                return Err(io::Error::from_raw_os_error(libc::E2BIG));
            }
        }
        entries.retain(|identity, entry| {
            if seen.contains(identity) {
                return true;
            }
            trace.event(Event::FileRemoved, entry.index, Duration::ZERO, 0);
            false
        });
        loop {
            if stop() || until.is_some_and(|until| Instant::now() >= until) {
                return Ok(());
            }
            let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            };
            // SAFETY: one writable event slot and a valid bounded timeout.
            let count =
                unsafe { libc::kevent(queue.as_raw_fd(), std::ptr::null(), 0, event.as_mut_ptr(), 1, &timeout) };
            if count > 0 {
                break;
            }
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            // Timeout checks cancellation only; enumeration is event-led.
        }
    }
}
