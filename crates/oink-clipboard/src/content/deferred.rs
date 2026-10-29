use super::*;
use std::sync::Condvar;

type HoverReader = Arc<dyn Fn(RepresentationId) -> io::Result<FileReader> + Send + Sync>;

/// The original native source owns the provider until its real Drop callback.
/// Readers may wait for that callback; enumeration and hover never trigger it.
/// Completion/cancel is a single assignment, so a late callback cannot revive a
/// cancelled generation or replace the bytes already granted to a reader.
pub struct Deferred {
    inventory: Inventory,
    translator: Arc<dyn Translator>,
    state: Mutex<Option<Result<Arc<dyn Provider>, Unavailable>>>,
    files: Mutex<Option<Arc<dyn namespace::Namespace>>>,
    ready: Condvar,
    hover: Mutex<Option<HoverReader>>,
}
impl std::fmt::Debug for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredContent")
            .field("items", &self.inventory.items.len())
            .finish()
    }
}
impl Deferred {
    pub fn new(inventory: Inventory, translator: Arc<dyn Translator>) -> io::Result<Arc<Self>> {
        inventory.validate().map_err(unavailable)?;
        Ok(Arc::new(Self {
            inventory,
            translator,
            state: Mutex::new(None),
            files: Mutex::new(None),
            ready: Condvar::new(),
            hover: Mutex::new(None),
        }))
    }
    /// Installed before announcing the drag; the native object stays on its owner thread.
    pub fn set_hover_reader(&self, reader: HoverReader) {
        *self.hover.lock().unwrap() = Some(reader);
    }
    /// Called by the metadata worker before announcing the source offer. No
    /// traversal or file bytes are required to install this capability.
    pub fn attach_files(&self, files: Arc<dyn namespace::Namespace>) -> bool {
        let state = self.state.lock().unwrap();
        let mut current = self.files.lock().unwrap();
        if state.is_some() || current.is_some() {
            files.cancel();
            return false;
        }
        *current = Some(files);
        true
    }
    pub fn complete(&self, provider: Arc<dyn Provider>) -> bool {
        let mut received = provider.inventory();
        // The native Drop snapshot captures non-file formats and can still have
        // the original CF_HDROP first-root anchor. Reconcile it against the same
        // captured namespace used to announce the offer; never recapture paths or
        // renumber native slots. File reads remain bound to that namespace's identities.
        if let Some(files) = self.files.lock().unwrap().as_ref() {
            received = received.with_roots(files.roots());
        }
        // Metadata capture can skip individual broken roots before announcing
        // this drag. Its real Drop must match that same filtered inventory.
        let offered = self.inventory();
        let same = received.platform == offered.platform
            && received.items.len() == offered.items.len()
            && offered.items.iter().zip(&received.items).all(|(old, new)| {
                old.id == new.id
                    && old.file == new.file
                    && old.representations.len() == new.representations.len()
                    && old.representations.iter().zip(&new.representations).all(|(a, b)| {
                        a.id == b.id
                            && a.format == b.format
                            && a.medium == b.medium
                            && a.aspect == b.aspect
                            && a.index == b.index
                    })
            });
        if !same {
            tracing::warn!(reason = ?Unavailable::Superseded,
                offered_items = offered.items.len(), received_items = received.items.len(),
                "native drag Drop snapshot rejected: inventory mismatch");
            self.fail(Unavailable::Superseded);
            return false;
        }
        self.finish(Ok(provider))
    }
    pub fn fail(&self, reason: Unavailable) -> bool {
        self.finish(Err(reason))
    }
    fn finish(&self, result: Result<Arc<dyn Provider>, Unavailable>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.is_some() {
            return false;
        }
        *state = Some(result);
        self.ready.notify_all();
        true
    }
}
impl Provider for Deferred {
    fn cancel(&self) {
        self.fail(Unavailable::Cancelled);
        if let Some(files) = self.files.lock().unwrap().as_ref() {
            files.cancel();
        }
    }
    fn inventory(&self) -> Inventory {
        let mut inventory = self.inventory.clone();
        if let Some(files) = self.files.lock().unwrap().as_ref() {
            inventory = inventory.with_roots(files.roots());
        }
        inventory
    }
    fn open_hover(&self, id: RepresentationId) -> io::Result<FileReader> {
        let state = self.state.lock().unwrap().clone();
        if let Some(state) = state {
            return state.map_err(unavailable)?.open(id);
        }
        let reader = self
            .hover
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
        reader(id)
    }
    fn open(&self, id: RepresentationId) -> io::Result<FileReader> {
        if find(&self.inventory, id).is_none() {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let wait = oink_foundation::drag_trace::Trace::new(
            oink_foundation::drag_trace::stage::DEFERRED_WAIT,
            oink_foundation::drag_trace::Context {
                representation: id.0,
                ..oink_foundation::drag_trace::Context::current()
            },
        );
        let mut state = self.state.lock().unwrap();
        while state.is_none() {
            state = self.ready.wait(state).unwrap();
        }
        let provider = state
            .as_ref()
            .unwrap()
            .as_ref()
            .map_err(|reason| unavailable(*reason))?
            .clone();
        drop(state);
        wait.result(0);
        drop(wait);
        provider.open(id)
    }
    fn translator(&self) -> Arc<dyn Translator> {
        self.translator.clone()
    }
    fn namespace(&self) -> Option<Arc<dyn namespace::Namespace>> {
        self.files.lock().unwrap().clone()
    }
}
