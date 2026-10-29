//! A native export can outlive adoption without retaining duplicate bytes.
//! Each already-open reader owns a Batch pin until it closes.
use super::*;
use std::io::Read;

#[derive(Clone)]
pub(super) struct Capture(Arc<Inner>);
struct Inner {
    path: PathBuf,
    batch: Mutex<Option<Batch>>,
    adopted: Mutex<std::collections::HashSet<NodeId>>,
}
impl Capture {
    pub fn new(batch: Batch) -> Self {
        Self(Arc::new(Inner {
            path: batch.path().to_owned(),
            batch: Mutex::new(Some(batch)),
            adopted: Mutex::new(std::collections::HashSet::new()),
        }))
    }
    pub fn path(&self) -> &Path {
        &self.0.path
    }
    pub fn pin(&self) -> io::Result<Batch> {
        self.0
            .batch
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))
    }
    pub fn release(&self) {
        self.0.batch.lock().unwrap().take();
    }
    pub fn adopted(&self, roots: &[NodeId], required: &[NodeId]) -> bool {
        let mut adopted = self.0.adopted.lock().unwrap();
        adopted.extend(roots.iter().copied().filter(|root| required.contains(root)));
        !required.is_empty() && required.iter().all(|root| adopted.contains(root))
    }
    pub fn external_writers_started(&self) -> io::Result<()> {
        self.pin()?.external_writers_started()
    }
    pub fn external_writers_finished(&self) -> io::Result<()> {
        self.pin()?.external_writers_finished()
    }
    pub fn abandon(&self) -> io::Result<()> {
        match self.0.batch.lock().unwrap().as_ref() {
            Some(batch) => batch.abandon(),
            None => Ok(()),
        }
    }
}

pub(super) struct Reader {
    pub inner: crate::FileReader,
    pub _batch: Batch,
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.inner.read(bytes)
    }
}
