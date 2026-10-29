//! First content demand schedules fulfilment on the source drag apartment.
//! OLE Drop can race that demand: Virtual::snapshot owns the single capture.
use super::*;

pub(super) struct Drag {
    files: Arc<Virtual>,
    schedule: Box<dyn Fn(Arc<Virtual>) -> io::Result<()> + Send + Sync>,
    started: OnceLock<Result<(), Unavailable>>,
}
impl Drag {
    pub(super) fn new(
        files: Arc<Virtual>,
        schedule: impl Fn(Arc<Virtual>) -> io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            files,
            schedule: Box::new(schedule),
            started: OnceLock::new(),
        }
    }
    fn fulfil(&self) -> io::Result<()> {
        self.started
            .get_or_init(|| {
                if self.files.state.lock().unwrap().is_some() {
                    return Ok(());
                }
                self.files.progressed();
                (self.schedule)(self.files.clone()).map_err(|error| {
                    self.files.cancel();
                    content::reason(&error)
                })
            })
            .map_err(content::unavailable)?;
        let mut state = self.files.state.lock().unwrap();
        while state.is_none() {
            let (next, _) = self
                .files
                .ready
                .wait_timeout(state, std::time::Duration::from_secs(1))
                .unwrap();
            state = next;
            if state.is_none()
                && self.files.progress.lock().unwrap().elapsed()
                    >= std::time::Duration::from_millis(crate::model::NO_PROGRESS_MS)
            {
                *state = Some(Err(Unavailable::NoProgress));
                self.files.ready.notify_all();
            }
        }
        state
            .as_ref()
            .unwrap()
            .as_ref()
            .map(|_| ())
            .map_err(|reason| content::unavailable(*reason))
    }
}
impl Namespace for Drag {
    fn release_fulfilled(&self, roots: &[NodeId]) {
        self.files.release_fulfilled(roots);
    }
    fn native_file_metadata(&self) -> crate::content::native_metadata::NativeFileMetadata {
        self.files.native_file_metadata()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.files.one_shot_roots()
    }
    fn prepare(&self) -> io::Result<()> {
        self.fulfil()
    }
    fn roots(&self) -> Vec<FileNode> {
        self.files.roots()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.files.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.fulfil()?;
        self.files.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
        self.fulfil()?;
        self.files.open(node, version, offset)
    }
    fn cancel(&self) {
        self.files.cancel();
    }
}
