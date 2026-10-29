//! Native catalogs are recovered before network owners are necessarily ready.
//! Keep metadata available offline; each read uses the current backend owner.
use super::*;
use std::sync::{Mutex, OnceLock, Weak};

#[derive(Clone, Default)]
pub struct Connector(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    generation: u64,
    current: Option<(Arc<dyn BulkLink>, tokio::runtime::Handle)>,
    namespaces: Vec<Weak<Restored>>,
}
impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceConnections").finish_non_exhaustive()
    }
}
/// An old backend shutting down cannot remove its replacement's transport.
pub struct Connection {
    connector: Weak<Mutex<State>>,
    generation: u64,
}
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceConnection").finish_non_exhaustive()
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(connector) = self.connector.upgrade() {
            let mut state = connector.lock().unwrap();
            if state.generation == self.generation {
                state.current = None;
                state.invalidate();
            }
        }
    }
}
impl State {
    fn invalidate(&mut self) {
        self.namespaces.retain(|weak| {
            if let Some(namespace) = weak.upgrade() {
                if let Some((_, current)) = namespace.current.lock().unwrap().take() {
                    current.cancel();
                }
                true
            } else {
                false
            }
        });
    }
}
impl Connector {
    pub fn connect(&self, link: Arc<dyn BulkLink>, runtime: tokio::runtime::Handle) -> Connection {
        let mut state = self.0.lock().unwrap();
        state.invalidate();
        state.generation = state.generation.checked_add(1).expect("backend generation exhausted");
        state.current = Some((link, runtime.clone()));
        let generation = state.generation;
        Connection {
            connector: Arc::downgrade(&self.0),
            generation,
        }
    }
    pub fn restore(&self, descriptor: Descriptor) -> io::Result<Arc<dyn Namespace>> {
        descriptor.validate()?;
        let namespace = Arc::new(Restored {
            descriptor,
            connector: self.clone(),
            current: Mutex::default(),
            cancelled: AtomicBool::new(false),
        });
        let mut state = self.0.lock().unwrap();
        state.namespaces.retain(|weak| weak.strong_count() > 0);
        state.namespaces.push(Arc::downgrade(&namespace));
        Ok(namespace)
    }
}
pub fn native_connector() -> &'static Connector {
    static CONNECTOR: OnceLock<Connector> = OnceLock::new();
    CONNECTOR.get_or_init(Connector::default)
}
struct Restored {
    descriptor: Descriptor,
    connector: Connector,
    current: Mutex<Option<(u64, Linked)>>,
    cancelled: AtomicBool,
}
impl Restored {
    fn current(&self) -> io::Result<Linked> {
        // Always take the connection lock before the namespace lock. Keeping
        // both during construction closes the connect/disconnect race: a stale
        // Linked cannot be installed just after its owner was invalidated.
        let state = self.connector.0.lock().unwrap();
        let mut current = self.current.lock().unwrap();
        if self.cancelled.load(Ordering::Acquire) {
            return Err(unavailable(Unavailable::Cancelled));
        }
        let (link, runtime) = state.current.as_ref().ok_or(io::ErrorKind::NotConnected)?;
        if let Some((generation, files)) = current.as_ref()
            && *generation == state.generation
        {
            return Ok(files.clone());
        }
        let files = Linked::restore(self.descriptor.clone(), link.clone(), runtime.clone())?;
        *current = Some((state.generation, files.clone()));
        Ok(files)
    }
}
impl Namespace for Restored {
    fn native_file_metadata(&self) -> crate::content::native_metadata::NativeFileMetadata {
        self.descriptor.native.clone()
    }
    fn source_link(&self) -> Option<Descriptor> {
        Some(self.descriptor.clone())
    }
    fn origin(&self) -> Option<DeviceId> {
        Some(self.descriptor.offer.clip.origin)
    }
    fn roots(&self) -> Vec<FileNode> {
        self.descriptor.roots.clone()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.current()?.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.current()?.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        self.current()?.open(node, version, offset)
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some((_, current)) = self.current.lock().unwrap().as_ref() {
            current.cancel();
        }
    }
}
