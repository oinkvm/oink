//! Failure-only, path-free target diagnostics. Never open contents or wait on
//! AppKit/input: metadata samples run on a disposable worker, with their lag and
//! overlap with native cleanup explicitly recorded (not an atomic OS snapshot).
use super::*;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::sync::atomic::{AtomicBool, AtomicU64};

const LIMIT: usize = 256;
pub(super) struct Session {
    pub operation: DragOperation,
    tx: mpsc::Sender<Message>,
    entered: AtomicBool,
    released: AtomicBool,
    ended: AtomicBool,
    cancelled: AtomicBool,
    masks: [AtomicU64; 3], // within, outside, contexts queried
    cleanup: Arc<AtomicBool>,
}
enum Message {
    Enter(Instant, u64),
    Release(Instant),
    End {
        at: Instant,
        operation: u64,
        masks: [u64; 3],
        cancelled: bool,
        over_source: bool,
    },
}
impl Session {
    pub fn new(spec: &DragSpec, provider: Option<&Registration>) -> Option<Arc<Self>> {
        let entries: Vec<_> = if let Some(provider) = provider {
            provider
                .entries()
                .iter()
                .filter(|e| files::is_top_level(e))
                .cloned()
                .collect()
        } else {
            spec.items
                .iter()
                .filter_map(|item| match item {
                    ClipData::Files(entries) => Some(entries),
                    _ => None,
                })
                .flatten()
                .filter(|e| files::is_top_level(e))
                .cloned()
                .collect()
        };
        let paths: Vec<_> = entries
            .into_iter()
            .filter_map(|e| e.local_path.map(|p| (p, if e.is_dir { 2 } else { 1 })))
            .collect();
        let published = provider.map(Registration::diagnostic_publication);
        let cleanup = Arc::new(AtomicBool::new(false));
        let observed_cleanup = cleanup.clone();
        let operation = spec.operation;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("oink-drop-trace".into())
            .spawn(move || {
                if let Some(report) = collect(rx, paths, published, observed_cleanup) {
                    report.emit(operation);
                }
            })
            .ok()?;
        Some(Arc::new(Self {
            operation,
            tx,
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            masks: std::array::from_fn(|_| AtomicU64::new(0)),
            cleanup,
        }))
    }
    pub fn enter(&self, trigger: u64) {
        if !self.entered.swap(true, Ordering::Relaxed) {
            let _ = self.tx.send(Message::Enter(Instant::now(), trigger));
        }
    }
    pub fn release(&self) {
        if !self.released.swap(true, Ordering::Relaxed) {
            let _ = self.tx.send(Message::Release(Instant::now()));
        }
    }
    pub fn mask(&self, context: NSDraggingContext, mask: NSDragOperation) {
        let index = if context == NSDraggingContext::WithinApplication {
            0
        } else {
            1
        };
        self.masks[index].store(mask.bits() as u64, Ordering::Relaxed);
        self.masks[2].fetch_or(1 << index, Ordering::Relaxed);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn cleanup(&self) {
        self.cleanup.store(true, Ordering::Release);
    }
    pub fn end(&self, operation: NSDragOperation, over_source: bool) {
        if !self.ended.swap(true, Ordering::Relaxed) {
            let _ = self.tx.send(Message::End {
                at: Instant::now(),
                operation: operation.bits() as u64,
                masks: std::array::from_fn(|i| self.masks[i].load(Ordering::Relaxed)),
                cancelled: self.cancelled.load(Ordering::Relaxed),
                over_source,
            });
        }
    }
}

#[derive(Clone, Default)]
struct Item {
    // kind (unknown/file/dir/symlink/other), existence (unknown/absent/present),
    // process R_OK (unknown/denied/allowed), dataless, hidden, stat errno, access errno.
    values: [u64; 7],
    identity: Option<(u64, u64)>,
}
fn inspect(path: &Path) -> Item {
    let mut item = Item::default();
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            item.values[0] = if m.is_file() {
                1
            } else if m.is_dir() {
                2
            } else if m.is_symlink() {
                3
            } else {
                4
            };
            item.values[1] = 2;
            item.values[3] = u64::from(m.st_flags() & 0x4000_0000 != 0); // SF_DATALESS
            item.values[4] = u64::from(m.st_flags() & 0x8000 != 0); // UF_HIDDEN
            item.identity = Some((m.dev(), m.ino()));
        }
        Err(e) => {
            item.values[1] = u64::from(matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)));
            item.values[5] = e.raw_os_error().unwrap_or(libc::EIO) as u64;
        }
    }
    if let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: a live NUL-terminated path. access checks this process's
        // permission only: no open/read, hydration or destination sandbox claim.
        let readable = unsafe { libc::access(path.as_ptr(), libc::R_OK) } == 0;
        item.values[2] = if readable { 2 } else { 1 };
        if !readable {
            item.values[6] = std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO) as u64;
        }
    }
    item
}
struct Sample {
    at: Instant,
    lag_us: u64,
    cleanup: bool,
    items: Vec<Item>,
}
fn sample(paths: &[(PathBuf, u64)], at: Instant, cleanup: &AtomicBool) -> Sample {
    let before = cleanup.load(Ordering::Acquire);
    let items = paths.iter().take(LIMIT).map(|(path, _)| inspect(path)).collect();
    Sample {
        at,
        // The upper bound for every item in this sample, including queue and stat/access time.
        lag_us: at.elapsed().as_micros() as u64,
        cleanup: before || cleanup.load(Ordering::Acquire),
        items,
    }
}
struct Report {
    total: usize,
    provider: bool,
    placeholder: bool,
    published: Option<Instant>,
    enter: Sample,
    release: Sample,
    enter_trigger: u64,
    release_observed: bool,
    end: Instant,
    masks: [u64; 3],
    cancelled: bool,
    over_source: bool,
    items: Vec<[u64; 17]>,
}
fn collect(
    rx: mpsc::Receiver<Message>,
    paths: Vec<(PathBuf, u64)>,
    published: Option<(Instant, bool)>,
    cleanup: Arc<AtomicBool>,
) -> Option<Report> {
    let mut enter = None;
    let mut release = None;
    for message in rx {
        match message {
            Message::Enter(at, trigger) => enter = Some((sample(&paths, at, &cleanup), trigger)),
            Message::Release(at) => release = Some(sample(&paths, at, &cleanup)),
            Message::End {
                at,
                operation,
                masks,
                cancelled,
                over_source,
            } => {
                if operation != 0 {
                    return None;
                }
                let (enter, enter_trigger) = enter.unwrap_or_else(|| (sample(&paths, at, &cleanup), 0));
                let release_observed = release.is_some();
                let release = release.unwrap_or_else(|| sample(&paths, at, &cleanup));
                let items = paths
                    .iter()
                    .zip(&enter.items)
                    .zip(&release.items)
                    .enumerate()
                    .map(|(index, (((_, kind), a), b))| {
                        let mut row = [0; 17];
                        row[0] = index as u64;
                        row[1] = *kind;
                        row[2..9].copy_from_slice(&a.values);
                        row[9..16].copy_from_slice(&b.values);
                        row[16] = match (a.identity, b.identity) {
                            (Some(a), Some(b)) => {
                                if a == b {
                                    1
                                } else {
                                    2
                                }
                            }
                            _ => 0,
                        };
                        row
                    })
                    .collect();
                return Some(Report {
                    total: paths.len(),
                    provider: published.is_some(),
                    placeholder: published.is_some_and(|(_, p)| p),
                    published: published.map(|(at, _)| at),
                    enter,
                    release,
                    enter_trigger,
                    release_observed,
                    end: at,
                    masks,
                    cancelled,
                    over_source,
                    items,
                });
            }
        }
    }
    None
}
impl Report {
    fn emit(self, operation: DragOperation) {
        let elapsed = |at: Instant| {
            self.published
                .map_or(0, |p| at.saturating_duration_since(p).as_micros() as u64)
        };
        let items = serde_json::to_string(&self.items).expect("numeric diagnostic rows");
        tracing::info!(target: "oink_clipboard::macos::dnd::drop_trace",
            dnd_hold_sequence = operation.sequence, dnd_drop_schema = 1_u64,
            dnd_drop_total = self.total as u64, dnd_drop_truncated = self.total > LIMIT,
            dnd_drop_provider = self.provider, dnd_drop_placeholder = self.placeholder,
            dnd_drop_offered_within = self.masks[0], dnd_drop_offered_outside = self.masks[1], dnd_drop_mask_contexts = self.masks[2],
            dnd_drop_hover_mask_known = false, dnd_drop_end_operation = 0_u64, dnd_drop_dropped = false,
            dnd_drop_publication_enter_us = elapsed(self.enter.at), dnd_drop_publication_release_us = elapsed(self.release.at),
            dnd_drop_publication_end_us = elapsed(self.end), dnd_drop_enter_trigger = self.enter_trigger,
            dnd_drop_enter_lag_us = self.enter.lag_us, dnd_drop_release_lag_us = self.release.lag_us,
            dnd_drop_enter_cleanup = self.enter.cleanup, dnd_drop_release_cleanup = self.release.cleanup,
            dnd_drop_release_observed = self.release_observed, dnd_drop_cancel_requested = self.cancelled,
            dnd_drop_over_source = self.over_source, dnd_drop_items = items.as_str(),
            "native target drag ended without drop");
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn report(paths: Vec<(PathBuf, u64)>, operation: u64, release: bool, cleanup: bool) -> Option<Report> {
        let (tx, rx) = mpsc::channel();
        let at = Instant::now();
        tx.send(Message::Enter(at, 1)).unwrap();
        if release {
            tx.send(Message::Release(at)).unwrap();
        }
        tx.send(Message::End {
            at,
            operation,
            masks: [1, 17, 3],
            cancelled: !release,
            over_source: !release,
        })
        .unwrap();
        collect(rx, paths, Some((at, true)), Arc::new(AtomicBool::new(cleanup)))
    }

    #[test]
    fn failure_record_preserves_item_order_without_names_and_success_is_silent() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("private-name.txt");
        std::fs::write(&file, b"private contents").unwrap();
        let missing = root.path().join("private-missing.txt");
        let paths = vec![(file.clone(), 1), (root.path().to_path_buf(), 2), (missing, 1)];
        assert!(report(paths.clone(), 1, true, false).is_none());
        let failed = report(paths, 0, true, false).unwrap();
        assert_eq!(failed.masks, [1, 17, 3]);
        assert!(failed.provider && failed.placeholder && failed.release_observed);
        assert!(!failed.cancelled && !failed.enter.cleanup && !failed.release.cleanup);
        assert_eq!(failed.items[0], [0, 1, 1, 2, 2, 0, 0, 0, 0, 1, 2, 2, 0, 0, 0, 0, 1]);
        assert_eq!(&failed.items[1][..5], &[1, 2, 2, 2, 2]);
        assert_eq!(&failed.items[2][..5], &[2, 1, 0, 1, 1]);
        assert_eq!(failed.items[2][7], libc::ENOENT as u64);
        assert_eq!(failed.items[2][14], libc::ENOENT as u64);
        let encoded = serde_json::to_string(&failed.items).unwrap();
        assert!(!encoded.contains("private"));
        assert_eq!(serde_json::from_str::<Vec<[u64; 17]>>(&encoded).unwrap(), failed.items);
        assert_eq!(std::fs::read(file).unwrap(), b"private contents");
    }

    #[test]
    fn cancellation_cleanup_overlap_and_bounded_items_are_explicit() {
        let root = tempfile::tempdir().unwrap();
        let failed = report(vec![(root.path().to_path_buf(), 2); LIMIT + 1], 0, false, true).unwrap();
        assert!(failed.cancelled && failed.over_source && failed.release.cleanup && failed.enter.cleanup);
        assert!(!failed.release_observed);
        assert_eq!(failed.total, LIMIT + 1);
        assert_eq!(failed.items.len(), LIMIT);
    }
}
