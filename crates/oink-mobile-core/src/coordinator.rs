use crate::{
    TransportError, TransportRequest,
    model::{AttachmentKind, IncomingAnnouncement, Peer, PeerKind, PeerState, ProxyChoice, SendPlan, SharePayload},
    store::{StoreError, TransferStore},
};
use std::sync::{Arc, Mutex};

pub trait Transport: Send + Sync {
    fn send(&self, request: TransportRequest) -> Result<(), TransportError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentQuery {
    pub request_id: [u8; 16],
    pub peer_id: String,
    pub max_items: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentQueryResult {
    Query(CurrentQuery),
    NoEligiblePeer,
    SourceOffline,
}

/// Shared mobile behavior for the app, Share Extension and Android open-with.
/// Native shells call this object through a thin UniFFI facade; they do not
/// implement their own routing or download state machine.
#[derive(Debug)]
pub struct TransferCoordinator<T> {
    pub store: Arc<Mutex<TransferStore>>,
    transport: Arc<T>,
    auto_open: Mutex<Option<AutoOpen>>,
}

/// The single "download and open" slot (transfer PLAN): only the latest
/// open request survives; it is `Ready` once its file is complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoOpen {
    Pending(i64),
    Ready(i64),
}

/// Result of recording an announcement. `download_error` reports a failed
/// automatic download request so the caller can surface it.
#[derive(Debug)]
pub struct Ingested {
    pub record: crate::MessageRecord,
    pub download_error: Option<TransportError>,
}

impl<T: Transport> TransferCoordinator<T> {
    pub fn new(store: TransferStore, transport: Arc<T>) -> Self {
        Self::with_store(Arc::new(Mutex::new(store)), transport)
    }

    pub(crate) fn with_store(store: Arc<Mutex<TransferStore>>, transport: Arc<T>) -> Self {
        Self {
            store,
            transport,
            auto_open: Mutex::new(None),
        }
    }

    pub fn startup_current(&self, peers: &[Peer]) -> CurrentQueryResult {
        self.startup_current_with([0; 16], 64, peers)
    }

    pub fn startup_current_with(&self, request_id: [u8; 16], max_items: u16, peers: &[Peer]) -> CurrentQueryResult {
        let first = peers
            .iter()
            .filter(|p| p.kind == PeerKind::Pc)
            .min_by_key(|p| (p.relayed, p.connection_order));
        match first {
            Some(peer) if peer.state == PeerState::Ready => CurrentQueryResult::Query(CurrentQuery {
                request_id,
                peer_id: peer.id.clone(),
                max_items,
            }),
            _ => CurrentQueryResult::NoEligiblePeer,
        }
    }

    /// Sends exactly one v2 ClipboardCurrentQuery: the first direct Ready PC.
    /// The protocol adapter adds caller Boot/group fields before encoding the
    /// request as the shared v2 Message variant.
    pub fn request_current(
        &self,
        request_id: [u8; 16],
        max_items: u16,
        peers: &[Peer],
    ) -> Result<CurrentQueryResult, SendError> {
        let result = self.startup_current_with(request_id, max_items, peers);
        if let CurrentQueryResult::Query(query) = &result {
            self.transport
                .send(TransportRequest::CurrentQuery {
                    request_id: query.request_id,
                    peer_id: query.peer_id.clone(),
                    max_items: query.max_items,
                })
                .map_err(SendError::Transport)?;
        }
        Ok(result)
    }

    /// Records one announced item and starts its automatic download when it
    /// is a Reader-backed text. Images are never downloaded automatically
    /// (user decision 2026-11-10): the transfer page shows the PC's thumbnail
    /// (requested when the message becomes visible) and the original is
    /// downloaded only when the user opens it.
    pub fn ingest(&self, now_ms: i64, announcement: IncomingAnnouncement) -> Result<Ingested, CoordinatorError> {
        self.ingest_with_rich(now_ms, announcement, None)
    }

    /// `ingest` of a text item announced with its rich text (2026-11-12):
    /// the rich text is recorded as the message's hidden child before the
    /// text download starts, so the text's completion always sees it. It is
    /// fetched after the text (`MobileV2Session`).
    pub fn ingest_with_rich(
        &self,
        now_ms: i64,
        announcement: IncomingAnnouncement,
        rich: Option<IncomingAnnouncement>,
    ) -> Result<Ingested, CoordinatorError> {
        let mut store = self.store.lock().expect("transfer store mutex poisoned");
        // Content its source marked as a secret is fetched only when the user
        // asks for it (shows it, copies it, or reads the current text).
        let auto_download =
            !announcement.sensitive && announcement.kind == AttachmentKind::Text && announcement.text.is_none();
        let record = store.insert_announcement(now_ms, announcement)?;
        let record = match rich {
            Some(rich) => {
                store.insert_rich_child(now_ms, record.id, rich)?;
                store
                    .by_id(record.id)?
                    .ok_or(CoordinatorError::Store(StoreError::MessageNotFound(record.id)))?
            }
            None => record,
        };
        if !(auto_download
            && record.source_online
            && matches!(
                record.download,
                crate::DownloadState::Placeholder | crate::DownloadState::Failed
            ))
        {
            return Ok(Ingested {
                record,
                download_error: None,
            });
        }
        store.begin_download(record.id)?;
        drop(store);
        let download_error = self.transport.send(download_request(&record)).err();
        let mut store = self.store.lock().expect("transfer store mutex poisoned");
        if download_error.is_some() {
            store.fail_download(record.id)?;
        }
        let record = store
            .by_id(record.id)?
            .ok_or(CoordinatorError::Store(StoreError::MessageNotFound(record.id)))?;
        Ok(Ingested { record, download_error })
    }

    pub fn plan_send(&self, payload: SharePayload, peers: &[Peer], choice: ProxyChoice) -> Result<SendPlan, SendError> {
        // A phone publishes only fully inline text. Larger text/rich text uses
        // the PC proxy, which keeps the bytes after a share extension disconnects.
        let text_only = payload.items.iter().all(|item| item.kind() == AttachmentKind::Text);
        let needs_proxy = !text_only
            || payload
                .items
                .iter()
                .try_fold(0u64, |total, item| total.checked_add(item.size()))
                .is_none_or(|total| total > oink_foundation::clipboard_history::INLINE_MAX as u64);
        if let ProxyChoice::Direct { device, .. } = &choice
            && !text_only
        {
            // Attachments to a phone go to it alone; there is no other
            // device to fall back to (an offline target fails at once).
            return peers
                .iter()
                .any(|p| p.id == *device && p.state == PeerState::Ready && p.accepts_clipboard)
                .then(|| SendPlan::Direct {
                    target_id: device.clone(),
                    payload,
                })
                .ok_or(SendError::NoProxy);
        }
        if !needs_proxy {
            return Ok(SendPlan::Broadcast { payload });
        }
        let selected = match choice {
            ProxyChoice::Device(id) => peers
                .iter()
                .filter(|p| eligible_proxy(p))
                .min_by_key(|p| (p.id != id, p.clipboard_pending, p.relayed, p.connection_order))
                .map(|p| p.id.clone()),
            ProxyChoice::Automatic | ProxyChoice::Direct { .. } => peers
                .iter()
                .filter(|p| eligible_proxy(p))
                .min_by_key(|p| (p.clipboard_pending, p.relayed, p.connection_order))
                .map(|p| p.id.clone()),
        }
        .ok_or(SendError::NoProxy)?;
        Ok(SendPlan::Proxy {
            proxy_id: selected,
            payload,
        })
    }

    pub fn send(&self, plan: SendPlan) -> Result<(), SendError> {
        let request = match plan {
            SendPlan::Broadcast { payload } => TransportRequest::Broadcast { payload },
            SendPlan::Proxy { proxy_id, payload } => TransportRequest::ProxyShare { proxy_id, payload },
            SendPlan::Direct { target_id, payload } => TransportRequest::DirectShare { target_id, payload },
        };
        self.transport.send(request).map_err(SendError::Transport)
    }

    /// Starts a typed v2 content read for one historical message.  The store
    /// transition and the v2 request are one core operation, so native hosts
    /// cannot accidentally mark a file downloading without sending a read.
    ///
    /// `open=true` puts the message into the single auto-open slot,
    /// replacing any earlier one; `open=false` (a plain download button)
    /// clears the slot. Returns `true` when the slot is ready to be taken
    /// right away because the file is already complete.
    pub fn request_download(&self, id: i64, open: bool) -> Result<bool, CoordinatorError> {
        let record = {
            let mut store = self.store.lock().expect("transfer store mutex poisoned");
            let record = store.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
            let existing_file = record.download == crate::DownloadState::Complete
                && record
                    .local_path
                    .as_deref()
                    .is_some_and(|path| std::path::Path::new(path).is_file());
            let active = store.attempt(id).is_some();
            let slot = open.then_some(if existing_file {
                AutoOpen::Ready(id)
            } else {
                AutoOpen::Pending(id)
            });
            if existing_file || active || record.outgoing {
                if existing_file {
                    store.keep_download(id)?;
                }
                if record.outgoing && !existing_file {
                    return Err(StoreError::OwnMessage(id).into());
                }
                *self.auto_open.lock().expect("auto-open mutex poisoned") = slot;
                return Ok(existing_file && open);
            }
            store.begin_download(id)?;
            *self.auto_open.lock().expect("auto-open mutex poisoned") = slot;
            record
        };
        if let Err(error) = self.transport.send(download_request(&record)) {
            self.store
                .lock()
                .expect("transfer store mutex poisoned")
                .fail_download(id)?;
            self.finish_auto_open(id);
            return Err(CoordinatorError::Transport(error));
        }
        Ok(false)
    }

    /// Starts downloading `id` for a caller that waits for the body itself
    /// (the one-tap copy of the current clipboard). Unlike `request_download`
    /// it leaves the auto-open slot alone, so a file the user asked to open
    /// still opens. Does nothing while a download of `id` is active.
    pub(crate) fn start_download(&self, id: i64) -> Result<(), CoordinatorError> {
        let record = {
            let mut store = self.store.lock().expect("transfer store mutex poisoned");
            if store.attempt(id).is_some() {
                return Ok(());
            }
            store.begin_download(id)?;
            store.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?
        };
        if let Err(error) = self.transport.send(download_request(&record)) {
            self.store
                .lock()
                .expect("transfer store mutex poisoned")
                .fail_download(id)?;
            return Err(CoordinatorError::Transport(error));
        }
        Ok(())
    }

    pub fn auto_open_id(&self) -> Option<i64> {
        self.auto_open
            .lock()
            .expect("auto-open mutex poisoned")
            .map(|slot| match slot {
                AutoOpen::Pending(id) | AutoOpen::Ready(id) => id,
            })
    }

    /// Marks the slot ready when `id` was waiting for its download. Returns
    /// whether the host should now take the slot.
    pub(crate) fn complete_auto_open(&self, id: i64) -> bool {
        let mut slot = self.auto_open.lock().expect("auto-open mutex poisoned");
        if *slot == Some(AutoOpen::Pending(id)) {
            *slot = Some(AutoOpen::Ready(id));
            return true;
        }
        false
    }

    /// Drops a pending slot whose download failed.
    pub fn finish_auto_open(&self, id: i64) {
        let mut slot = self.auto_open.lock().expect("auto-open mutex poisoned");
        if *slot == Some(AutoOpen::Pending(id)) {
            *slot = None;
        }
    }

    pub(crate) fn cancel_auto_open(&self) {
        self.auto_open.lock().expect("auto-open mutex poisoned").take();
    }

    pub(crate) fn take_ready_auto_open(&self) -> Result<Option<crate::MessageRecord>, StoreError> {
        let store = self.store.lock().expect("transfer store mutex poisoned");
        let mut slot = self.auto_open.lock().expect("auto-open mutex poisoned");
        let Some(AutoOpen::Ready(id)) = *slot else {
            return Ok(None);
        };
        *slot = None;
        Ok(store.by_id(id)?.filter(|record| {
            record.download == crate::DownloadState::Complete
                && record
                    .local_path
                    .as_deref()
                    .is_some_and(|path| std::path::Path::new(path).is_file())
        }))
    }
}

fn download_request(record: &crate::MessageRecord) -> TransportRequest {
    TransportRequest::Download {
        operation_id: record.operation_id,
        message_id: record.message_id,
        item_id: record.item_id,
        representation_id: record.representation_id,
        source_id: record.proxy_id.clone().unwrap_or_else(|| record.sender_id.clone()),
        content_id: record.content_id.clone(),
    }
}

fn eligible_proxy(peer: &Peer) -> bool {
    peer.kind == PeerKind::Pc
        && peer.state == PeerState::Ready
        && peer.accepts_clipboard
        && !peer.relayed
        && !peer.clipboard_pending
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("no online PC is available as a proxy")]
    NoProxy,
    #[error("transport failed: {0:?}")]
    Transport(TransportError),
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("transport failed: {0:?}")]
    Transport(TransportError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{PeerKind, PeerState, SendItem, SendOrigin, SourceState},
        store::TransferStore,
    };

    fn image(content_id: &str, size: u64) -> IncomingAnnouncement {
        IncomingAnnouncement {
            sender_id: "pc".into(),
            proxy_id: None,
            content_id: content_id.into(),
            kind: AttachmentKind::Image,
            display_name: "image.png".into(),
            mime: Some("image/png".into()),
            size,
            text: None,
            rich_text_format: None,
            source: SourceState {
                source_id: "pc".into(),
                availability: crate::SourceAvailability::Ready,
            },
            operation_id: [1; 16],
            message_id: [2; 16],
            item_id: 0,
            representation_id: 0,
            sensitive: false,
        }
    }

    /// User decision 2026-11-10: no image original is downloaded on arrival,
    /// whatever its size; only opening it downloads it.
    #[test]
    fn images_of_any_size_are_never_downloaded_on_arrival() {
        let c = coordinator();
        for (n, size) in [0, 1, 10, 1024 * 1024, 300 * 1024 * 1024].into_iter().enumerate() {
            let record = c.ingest(n as i64, image(&format!("image{n}"), size)).unwrap().record;
            assert_eq!(record.download, crate::DownloadState::Placeholder, "size {size}");
        }
        assert!(c.transport.requests.lock().unwrap().is_empty());
        let opened = c.store.lock().unwrap().page(None, 10).unwrap().messages[0].id;
        c.request_download(opened, true).unwrap();
        assert!(matches!(
            c.transport.requests.lock().unwrap().as_slice(),
            [TransportRequest::Download { .. }]
        ));
    }

    #[test]
    fn plain_download_clears_the_auto_open_slot_and_open_overwrites_it() {
        let c = coordinator();
        let first = c.ingest(1, image("first", 10)).unwrap().record;
        let second = c.ingest(2, image("second", 10)).unwrap().record;
        assert!(!c.request_download(first.id, true).unwrap());
        assert_eq!(c.auto_open_id(), Some(first.id));
        c.request_download(second.id, false).unwrap();
        assert_eq!(c.auto_open_id(), None);
        assert!(!c.complete_auto_open(first.id));
    }

    #[test]
    fn non_png_images_are_sent_through_a_proxy_as_images() {
        let c = coordinator();
        let peers = vec![Peer {
            id: "pc".into(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 0,
            relayed: false,
            clipboard_pending: false,
        }];
        let photo = SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::Image {
                path: "photo.heic".into(),
                encoded_size: 10,
                mime: "image/heic".into(),
                staged: false,
                dimensions: None,
            }],
            origin: SendOrigin::TransferPage,
            operation_id: [5; 16],
            message_id: [6; 16],
        };
        assert!(matches!(
            c.plan_send(photo, &peers, ProxyChoice::Automatic).unwrap(),
            SendPlan::Proxy { payload, .. } if payload.items[0].kind() == AttachmentKind::Image
        ));
    }
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Mock {
        sent: AtomicUsize,
        requests: StdMutex<Vec<TransportRequest>>,
    }
    impl Transport for Mock {
        fn send(&self, request: TransportRequest) -> Result<(), TransportError> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            Ok(())
        }
    }

    fn coordinator() -> TransferCoordinator<Mock> {
        let dir = tempfile::tempdir().unwrap();
        // Keep the temp directory alive by using a database in /tmp for this
        // unit test; production callers own both paths for the session life.
        let db = dir.path().join("history.sqlite");
        let files = dir.path().join("files");
        let store = TransferStore::open(db, files).unwrap();
        TransferCoordinator::new(
            store,
            Arc::new(Mock {
                sent: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
            }),
        )
    }

    #[test]
    fn startup_queries_only_first_ready_pc() {
        let c = coordinator();
        let peers = vec![
            Peer {
                id: "mobile".into(),
                kind: PeerKind::Mobile,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 0,
                relayed: false,
                clipboard_pending: false,
            },
            Peer {
                id: "pc2".into(),
                kind: PeerKind::Pc,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 2,
                relayed: false,
                clipboard_pending: false,
            },
            Peer {
                id: "pc1".into(),
                kind: PeerKind::Pc,
                state: PeerState::Ready,
                accepts_clipboard: true,
                connection_order: 1,
                relayed: false,
                clipboard_pending: false,
            },
        ];
        assert_eq!(
            c.startup_current(&peers),
            CurrentQueryResult::Query(CurrentQuery {
                request_id: [0; 16],
                peer_id: "pc1".into(),
                max_items: 64,
            })
        );
    }

    #[test]
    fn small_text_broadcasts_and_file_uses_proxy() {
        let c = coordinator();
        let peers = vec![Peer {
            id: "pc".into(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 0,
            relayed: false,
            clipboard_pending: false,
        }];
        let small = SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::Text { text: "hello".into() }],
            origin: SendOrigin::TransferPage,
            operation_id: [1; 16],
            message_id: [2; 16],
        };
        assert!(matches!(
            c.plan_send(small, &peers, ProxyChoice::Automatic).unwrap(),
            SendPlan::Broadcast { .. }
        ));
        let file = SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![SendItem::File {
                path: "x".into(),
                name: "x".into(),
                size: 1,
                mime: None,
                staged: false,
            }],
            origin: SendOrigin::TransferPage,
            operation_id: [3; 16],
            message_id: [4; 16],
        };
        assert!(
            matches!(c.plan_send(file, &peers, ProxyChoice::Automatic).unwrap(), SendPlan::Proxy { proxy_id, .. } if proxy_id == "pc")
        );
    }

    /// Only text is broadcast (protocol-v2 5.2): a small PNG goes to the
    /// chosen device too. A chosen phone gets attachments alone; text still
    /// goes to everyone, and an offline phone is no target at all.
    #[test]
    fn only_text_is_broadcast_and_a_chosen_phone_gets_attachments_alone() {
        let c = coordinator();
        let peer = |id: &str, kind| Peer {
            id: id.into(),
            kind,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 0,
            relayed: false,
            clipboard_pending: false,
        };
        let peers = vec![peer("pc", PeerKind::Pc), peer("tablet", PeerKind::Mobile)];
        let payload = |item| SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![item],
            origin: SendOrigin::TransferPage,
            operation_id: [1; 16],
            message_id: [2; 16],
        };
        let png = || {
            payload(SendItem::Image {
                path: "dot.png".into(),
                encoded_size: 10,
                mime: "image/png".into(),
                staged: false,
                dimensions: None,
            })
        };
        let text = || payload(SendItem::Text { text: "hi".into() });
        let tablet = || ProxyChoice::Direct {
            device: "tablet".into(),
            name: "Tablet".into(),
        };
        assert!(matches!(
            c.plan_send(png(), &peers, ProxyChoice::Automatic).unwrap(),
            SendPlan::Proxy { proxy_id, .. } if proxy_id == "pc"
        ));
        assert!(matches!(
            c.plan_send(png(), &peers, tablet()).unwrap(),
            SendPlan::Direct { target_id, .. } if target_id == "tablet"
        ));
        assert!(matches!(
            c.plan_send(text(), &peers, tablet()).unwrap(),
            SendPlan::Broadcast { .. }
        ));
        // Text with its rich text is still text: broadcast while all its
        // formats together fit the broadcast budget, else through a PC.
        let rich = |html: String| payload(SendItem::text("hi".into(), Some(html), Some("{\\rtf1 hi}".into())));
        assert!(matches!(
            c.plan_send(rich("<b>hi</b>".into()), &peers, ProxyChoice::Automatic)
                .unwrap(),
            SendPlan::Broadcast { .. }
        ));
        assert!(matches!(
            c.plan_send(rich("x".repeat(64 * 1024)), &peers, ProxyChoice::Automatic).unwrap(),
            SendPlan::Proxy { proxy_id, .. } if proxy_id == "pc"
        ));
        let offline = vec![peer("pc", PeerKind::Pc)];
        assert!(matches!(
            c.plan_send(png(), &offline, tablet()),
            Err(SendError::NoProxy)
        ));
    }

    #[test]
    fn current_request_contains_v2_correlation_fields() {
        let c = coordinator();
        let peers = vec![Peer {
            id: "pc".into(),
            kind: PeerKind::Pc,
            state: PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 0,
            relayed: false,
            clipboard_pending: false,
        }];
        let result = c.request_current([7; 16], 12, &peers).unwrap();
        assert_eq!(
            result,
            CurrentQueryResult::Query(CurrentQuery {
                request_id: [7; 16],
                peer_id: "pc".into(),
                max_items: 12
            })
        );
        let requests = c.transport.requests.lock().unwrap();
        assert!(
            matches!(requests.first(), Some(TransportRequest::CurrentQuery { request_id, peer_id, max_items }) if *request_id == [7; 16] && peer_id == "pc" && *max_items == 12)
        );
    }

    #[test]
    fn opening_an_image_emits_a_content_request_with_all_ids() {
        let c = coordinator();
        let item = IncomingAnnouncement {
            sender_id: "phone".into(),
            proxy_id: Some("pc".into()),
            content_id: "content_1".into(),
            kind: AttachmentKind::Image,
            display_name: "photo.png".into(),
            mime: Some("image/png".into()),
            size: 100,
            text: None,
            rich_text_format: None,
            source: SourceState {
                source_id: "pc".into(),
                availability: crate::SourceAvailability::Ready,
            },
            operation_id: [1; 16],
            message_id: [2; 16],
            item_id: 3,
            representation_id: 4,
            sensitive: false,
        };
        let record = c.ingest(10, item).unwrap().record;
        assert_eq!(record.download, crate::DownloadState::Placeholder);
        assert!(c.transport.requests.lock().unwrap().is_empty());
        c.request_download(record.id, true).unwrap();
        let requests = c.transport.requests.lock().unwrap();
        assert!(
            matches!(requests.first(), Some(TransportRequest::Download { operation_id, message_id, item_id, representation_id, source_id, content_id }) if *operation_id == [1; 16] && *message_id == [2; 16] && *item_id == 3 && *representation_id == 4 && source_id == "pc" && content_id == "content_1")
        );
    }
}
