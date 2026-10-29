//! Persistent native catalogs may refer to this device after a drag returns.
//! Serve their authenticated token operations locally, without a self peer or
//! transient drag/clipboard authority. Remote sources keep the network path.
use super::*;
use oink_clipboard::content::Registry;
use tokio::sync::mpsc;

pub(super) struct Source {
    pub device: DeviceId,
    registry: Arc<Registry>,
    pub interrupted: watch::Sender<bool>,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSource")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}
impl Source {
    pub fn new(device: DeviceId, registry: Arc<Registry>, paused: bool) -> Self {
        Self {
            device,
            registry,
            interrupted: watch::channel(paused).0,
        }
    }
}

pub(super) struct Link {
    links: Arc<FileLinks>,
    peer: DeviceId,
}
impl Link {
    pub fn new(links: Arc<FileLinks>, peer: DeviceId) -> Self {
        Self { links, peer }
    }
}
impl BulkLink for Link {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn bind_persistent(self: Arc<Self>, peer: DeviceId) -> io::Result<Arc<dyn BulkLink>> {
        if peer != self.peer {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(self)
    }
    fn supports_cloud_links(&self) -> bool {
        true
    }
    fn open(&self, peer: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async move {
            let activity = oink_foundation::maintenance::process()
                .enter(oink_foundation::maintenance::ActivityKind::FileTransfer)
                .ok_or(io::ErrorKind::WouldBlock)?;
            let (registry, permission) = {
                let state = self.links.state.lock().unwrap();
                let source = state
                    .local
                    .as_ref()
                    .filter(|source| source.device == peer && peer == self.peer && !state.paused)
                    .ok_or(io::ErrorKind::NotConnected)?;
                (source.registry.clone(), source.interrupted.subscribe())
            };
            let (a, ar) = mpsc::channel(2);
            let (b, br) = mpsc::channel(2);
            let mut server = Channel {
                out: Some(b),
                input: ar,
                permission: permission.clone(),
                client: false,
            };
            tokio::spawn(async move {
                let _activity = activity;
                if let Ok(Some(request)) = server.recv().await {
                    // Registry checks membership, receiver, token, versions and
                    // roots exactly as on a network persistent operation. Never
                    // accept a fresh grant or transient native representation.
                    if request.persistent_request() {
                        let _ = registry.serve(peer, request, &mut server).await;
                    }
                }
                server.finish();
            });
            Ok(Box::new(Channel {
                out: Some(a),
                input: br,
                permission,
                client: true,
            }) as Box<dyn BulkChannel>)
        })
    }
}
struct Channel {
    out: Option<mpsc::Sender<ContentMsg>>,
    input: mpsc::Receiver<ContentMsg>,
    permission: watch::Receiver<bool>,
    client: bool,
}
impl BulkChannel for Channel {
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let permission = self.permission.clone();
        Arc::new(move || !*permission.borrow())
    }
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if self.client && !message.persistent_request() {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            let out = self.out.as_ref().ok_or(io::ErrorKind::BrokenPipe)?;
            tokio::select! {
                biased;
                _ = revoked(&mut self.permission) => Err(io::ErrorKind::ConnectionAborted.into()),
                result = out.send(message.clone()) => result.map_err(|_| io::ErrorKind::BrokenPipe.into()),
            }
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = revoked(&mut self.permission) => Err(io::ErrorKind::ConnectionAborted.into()),
                result = self.input.recv() => Ok(result),
            }
        })
    }
    fn finish(&mut self) {
        self.out = None;
    }
}
