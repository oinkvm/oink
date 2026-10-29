//! Stable native identity slots; discovery only appends, never renumbers readers.
use super::*;
pub(super) struct DeliveredFile {
    pub entry: FileEntry,
    pub hydrated: AtomicU64,
    pub opens: AtomicI64,
    pub deleted: AtomicBool,
    pub withdrawn: AtomicBool,
    pub local_final: AtomicBool,
    pub selected: Mutex<(FileMetadata, Option<u64>)>,
}
impl DeliveredFile {
    pub fn new(entry: FileEntry, deleted: bool) -> Self {
        Self {
            selected: Mutex::new((
                FileMetadata {
                    exec_bits: None,
                    size: entry.size,
                    modified_ms: entry.modified_ms,
                },
                None,
            )),
            entry,
            hydrated: AtomicU64::new(0),
            opens: AtomicI64::new(0),
            deleted: AtomicBool::new(deleted),
            withdrawn: AtomicBool::new(false),
            local_final: AtomicBool::new(false),
        }
    }
}
impl Delivery {
    pub(super) fn files(&self) -> Vec<Arc<DeliveredFile>> {
        self.files.lock().unwrap().clone()
    }
    pub(super) fn entries(&self) -> Vec<FileEntry> {
        self.files().iter().map(|file| file.entry.clone()).collect()
    }
    pub(super) fn get_file(&self, index: usize) -> Option<Arc<DeliveredFile>> {
        self.files.lock().unwrap().get(index).cloned()
    }
    pub(super) fn file(&self, index: usize) -> Arc<DeliveredFile> {
        self.get_file(index).expect("validated native slot")
    }
}
