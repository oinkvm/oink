//! Receiving a phone-to-phone transfer (protocol-v2 5.2, "手机点对点").
//!
//! Another phone streams attachments to this one alone: one `ContentOffer`
//! per item followed by its representation chunks, written straight into the
//! item's attachment directory while they arrive. Each item becomes its own
//! received record as soon as its offer arrives. Nothing is forwarded or
//! announced to the group. Either end's interruption fails the transfer;
//! nothing is resumed.

use super::*;
use std::io::Write;

/// Why a running receive ends before its content is complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DirectEnd {
    /// The sending phone's user cancelled: both ends drop the records.
    SenderCancelled,
    /// This phone's user cancelled: both ends keep them as cancelled.
    ReceiverCancelled,
    /// The link to the sending phone went down.
    PeerLost,
}

/// One accepted phone-to-phone receive.
#[derive(Debug)]
pub(super) struct DirectReceive {
    sensitive: bool,
    message_id: [u8; 16],
    source: dto::Boot,
    count: u16,
    expected_bytes: u64,
    created: Instant,
    started: bool,
    /// Records created so far (a receive cancel names one of them; 清理传输记录
    /// keeps them).
    pub(super) records: Vec<i64>,
    end: tokio::sync::watch::Sender<Option<DirectEnd>>,
}

#[cfg(test)]
impl DirectReceive {
    /// A receive under way that recorded `records` so far.
    pub(super) fn running(source: dto::Boot, records: Vec<i64>) -> Self {
        Self {
            sensitive: false,
            message_id: [0; 16],
            source,
            count: 2,
            expected_bytes: 0,
            created: Instant::now(),
            started: true,
            records,
            end: tokio::sync::watch::channel(None).0,
        }
    }
}

/// How a receive ended, for both ends.
#[derive(Debug)]
struct Failed {
    end: Option<DirectEnd>,
    reason: DeliveryFailure,
    detail: String,
}

impl Failed {
    fn ended(end: DirectEnd) -> Self {
        let (reason, detail) = match end {
            DirectEnd::SenderCancelled | DirectEnd::ReceiverCancelled => {
                (DeliveryFailure::Cancelled, "the transfer was cancelled")
            }
            DirectEnd::PeerLost => (DeliveryFailure::PeerDisconnected, "the sending phone disconnected"),
        };
        Self {
            end: Some(end),
            reason,
            detail: detail.into(),
        }
    }

    fn stream(error: impl std::fmt::Display) -> Self {
        Self {
            end: None,
            reason: DeliveryFailure::PeerDisconnected,
            detail: format!("the transfer stream broke: {error}"),
        }
    }

    fn invalid(detail: impl std::fmt::Display) -> Self {
        Self {
            end: None,
            reason: DeliveryFailure::Interrupted,
            detail: format!("invalid phone-to-phone transfer: {detail}"),
        }
    }

    fn io(error: impl Into<std::io::Error>) -> Self {
        let error = error.into();
        Self {
            end: None,
            reason: if error.kind() == std::io::ErrorKind::StorageFull {
                DeliveryFailure::StorageFull
            } else {
                DeliveryFailure::Interrupted
            },
            detail: format!("writing the received content failed: {error}"),
        }
    }

    fn store(error: crate::StoreError) -> Self {
        match error {
            crate::StoreError::Io(error) => Self::io(error),
            error => Self {
                end: None,
                reason: DeliveryFailure::Interrupted,
                detail: error.to_string(),
            },
        }
    }
}

/// The item being received.
struct Item {
    record: i64,
    attempt: u64,
    kind: AttachmentKind,
    name: String,
    path: PathBuf,
    file: fs::File,
    size: u64,
    written: u64,
    content_id: String,
}

impl<L: MobileV2Link> MobileV2Session<L> {
    /// Accepts or refuses the Begin of a phone-to-phone transfer.
    pub(super) fn accept_direct(
        &self,
        peer: &str,
        begin: &dto::ClipboardShareBegin,
    ) -> Result<(), MobileV2TransportError> {
        let operation_id = begin.operation_id().map_err(protocol_error)?.0;
        let source = begin.source().map_err(protocol_error)?;
        let count = begin.item_count().map_err(protocol_error)?;
        let bytes = begin.bytes().map_err(protocol_error)?;
        let refusal = if !self.transport.link.accepts_direct() {
            Some(dto::ShareRejectReason::ReceiveOff)
        } else if begin.origin_sender().map_err(protocol_error)? != source.device_id().map_err(protocol_error)?
            || begin
                .target()
                .map_err(protocol_error)?
                .is_some_and(|target| target != boot(self.transport.identity.boot))
            || !(1..=MAX_SHARE_ITEMS).contains(&count)
            || bytes.is_none()
            // Text is broadcast, never sent phone to phone.
            || !matches!(
                begin.kind().map_err(protocol_error)?,
                dto::ShareKind::Image | dto::ShareKind::File | dto::ShareKind::Folder | dto::ShareKind::Html | dto::ShareKind::Rtf
            )
        {
            Some(dto::ShareRejectReason::Invalid)
        } else {
            let mut direct = self.direct.lock().expect("direct receive mutex poisoned");
            direct.retain(|_, receive| receive.started || receive.created.elapsed() < SHARE_LIFETIME);
            let key = (peer.to_owned(), operation_id);
            if direct.contains_key(&key) {
                Some(dto::ShareRejectReason::Duplicate)
            } else if direct.len() >= MAX_PENDING {
                Some(dto::ShareRejectReason::Busy)
            } else {
                direct.insert(
                    key,
                    DirectReceive {
                        sensitive: begin
                            .sensitivity()
                            .map_err(protocol_error)?
                            .is_some_and(|v| matches!(v, dto::Sensitivity::Concealed | dto::Sensitivity::Transient)),
                        message_id: begin.message_id().map_err(protocol_error)?.0,
                        source,
                        count,
                        expected_bytes: bytes.unwrap_or(0),
                        created: Instant::now(),
                        started: false,
                        records: Vec::new(),
                        end: tokio::sync::watch::channel(None).0,
                    },
                );
                None
            }
        };
        let raw = RawMap::from([(1, id16(operation_id).write())]);
        let message = match refusal {
            None => {
                v2::Message::ClipboardShareAccept(dto::ClipboardShareAccept::from_raw(raw).map_err(protocol_error)?)
            }
            Some(reason) => {
                let mut raw = raw;
                raw.insert(2, dto::ShareStatus::Unsupported.write());
                raw.insert(3, reason.write());
                v2::Message::ClipboardShareReject(dto::ClipboardShareReject::from_raw(raw).map_err(protocol_error)?)
            }
        };
        let result = self.transport.send_message(peer, message);
        if result.is_err() && refusal.is_none() {
            self.direct
                .lock()
                .expect("direct receive mutex poisoned")
                .remove(&(peer.to_owned(), operation_id));
        }
        result
    }

    pub(super) fn has_direct(&self, peer: &str, operation_id: [u8; 16]) -> bool {
        self.direct
            .lock()
            .expect("direct receive mutex poisoned")
            .contains_key(&(peer.to_owned(), operation_id))
    }

    /// Runs one accepted receive on its Low stream to its end. Every end
    /// (complete, failed, cancelled) is reported through the records and
    /// `DirectEnded`; only a stream nobody accepted is an error.
    pub(super) async fn receive_direct(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        first: v2::Message,
        stream: &mut LowStream,
    ) -> Result<(), MobileV2TransportError> {
        let key = (peer.to_owned(), operation_id);
        let (mut end, message_id, source, count, expected) = {
            let mut direct = self.direct.lock().expect("direct receive mutex poisoned");
            let receive = direct
                .get_mut(&key)
                .filter(|receive| !receive.started)
                .ok_or_else(|| link_error("transfer was not accepted"))?;
            receive.started = true;
            (
                receive.end.subscribe(),
                receive.message_id,
                receive.source.clone(),
                receive.count,
                receive.expected_bytes,
            )
        };
        let mut current = None;
        let result = self
            .direct_body(
                peer,
                operation_id,
                (message_id, source, count, expected),
                first,
                stream,
                &mut end,
                &mut current,
            )
            .await;
        // Settled under the map lock: a cancel arriving later finds no
        // running receive and acts on the stored records instead.
        let ended = {
            let mut direct = self.direct.lock().expect("direct receive mutex poisoned");
            direct.remove(&key).and_then(|receive| *receive.end.borrow())
        };
        let records: Vec<i64> = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .direct_received(peer, operation_id)
            .map(|records| records.iter().map(|record| record.id).collect())
            .unwrap_or_default();
        let failure = match result {
            Ok(()) if ended != Some(DirectEnd::SenderCancelled) => {
                self.reply(peer, operation_id, dto::ShareStatus::Complete);
                self.direct_ended(operation_id, records, None);
                return Ok(());
            }
            Ok(()) => Failed::ended(DirectEnd::SenderCancelled),
            Err(failure) => failure,
        };
        drop(current);
        let end = failure.end.or(ended);
        if end == Some(DirectEnd::SenderCancelled) {
            self.remove_received(operation_id, records);
            return Ok(());
        }
        let failure = match end {
            Some(end) if failure.end.is_none() => Failed::ended(end),
            _ => failure,
        };
        stream.cancel();
        match end {
            Some(DirectEnd::ReceiverCancelled) => self.reply(peer, operation_id, dto::ShareStatus::Cancelled),
            Some(DirectEnd::PeerLost) => {}
            _ => self.reply(peer, operation_id, dto::ShareStatus::Failed),
        }
        {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            for id in &records {
                if store
                    .by_id(*id)
                    .ok()
                    .flatten()
                    .is_some_and(|record| record.download != crate::DownloadState::Complete)
                {
                    let _ = store.fail_direct(*id, failure.reason, &failure.detail);
                }
            }
        }
        for id in &records {
            let refreshed = self
                .coordinator
                .store
                .lock()
                .expect("transfer store mutex poisoned")
                .by_id(*id);
            if let Ok(Some(record)) = refreshed {
                self.announcement(&record, false);
            }
        }
        // The outcome is on the records and in DirectEnded, not an error.
        self.direct_ended(operation_id, records, Some(failure.reason));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn direct_body(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        (message_id, source, count, expected): ([u8; 16], dto::Boot, u16, u64),
        first: v2::Message,
        stream: &mut LowStream,
        end: &mut tokio::sync::watch::Receiver<Option<DirectEnd>>,
        current: &mut Option<Item>,
    ) -> Result<(), Failed> {
        let mut message = first;
        let mut items = 0u16;
        let mut received = 0u64;
        let mut first_record = true;
        let mut last_report = Instant::now();
        loop {
            match message {
                v2::Message::ContentOffer(offer) => {
                    if current.is_some() || items >= count {
                        return Err(Failed::invalid("unexpected item"));
                    }
                    *current = Some(self.direct_item(
                        peer,
                        operation_id,
                        message_id,
                        &source,
                        &offer,
                        count,
                        expected,
                        first_record,
                    )?);
                    first_record = false;
                }
                v2::Message::ContentRepresentationBytes(chunk) => {
                    let item = current
                        .as_mut()
                        .ok_or_else(|| Failed::invalid("bytes without an item"))?;
                    let id = chunk
                        .offer()
                        .and_then(|offer| {
                            content_id_for_representation(&offer, item_id_of(&item.content_id), chunk.representation()?)
                        })
                        .map_err(Failed::invalid)?;
                    let bytes = chunk.bytes().map_err(Failed::invalid)?.0;
                    if id != item.content_id
                        || bytes.is_empty()
                        || chunk.offset().map_err(Failed::invalid)? != item.written
                        || item.written + bytes.len() as u64 > item.size
                        || received + bytes.len() as u64 > expected
                    {
                        return Err(Failed::invalid("chunk out of order or beyond the announced size"));
                    }
                    // A deleted record or a cleared cache ends the receive
                    // like the user cancelling it.
                    if self
                        .coordinator
                        .store
                        .lock()
                        .expect("transfer store mutex poisoned")
                        .attempt(item.record)
                        != Some(item.attempt)
                    {
                        return Err(Failed::ended(DirectEnd::ReceiverCancelled));
                    }
                    item.file.write_all(&bytes).map_err(Failed::io)?;
                    item.written += bytes.len() as u64;
                    received += bytes.len() as u64;
                    if last_report.elapsed() >= Duration::from_millis(250) {
                        last_report = Instant::now();
                        self.transport.emit(TransferEvent::DownloadProgress {
                            operation_id,
                            record_id: item.record,
                            received: item.written,
                            total: Some(item.size),
                        });
                    }
                }
                v2::Message::ContentRepresentationEnd(done) => {
                    let item = current.take().ok_or_else(|| Failed::invalid("end without an item"))?;
                    if done.size().map_err(Failed::invalid)? != item.written || item.written != item.size {
                        return Err(Failed::invalid("item size does not match"));
                    }
                    self.finish_item(operation_id, item).await?;
                    items += 1;
                }
                v2::Message::ClipboardShareTextEnd(done) => {
                    if done.operation_id().map_err(Failed::invalid)?.0 != operation_id
                        || current.is_some()
                        || items != count
                        || received != expected
                    {
                        return Err(Failed::invalid("transfer ended before its items"));
                    }
                    let tail = tokio::select! {
                        tail = stream.recv() => tail.map_err(Failed::stream)?,
                        () = ended(end) => return Err(Failed::ended(end.borrow().expect("signalled end"))),
                    };
                    if tail.is_some() || !stream.effect_validity()() {
                        return Err(Failed::invalid("trailing records"));
                    }
                    stream.finish();
                    return Ok(());
                }
                _ => return Err(Failed::invalid("unexpected message")),
            }
            message = tokio::select! {
                next = stream.recv() => next
                    .map_err(Failed::stream)?
                    .ok_or_else(|| Failed::stream("the sender closed the stream early"))?,
                () = ended(end) => return Err(Failed::ended(end.borrow().expect("signalled end"))),
            };
        }
    }

    /// Records one arriving item and opens its file.
    #[allow(clippy::too_many_arguments)]
    fn direct_item(
        &self,
        peer: &str,
        operation_id: [u8; 16],
        message_id: [u8; 16],
        source: &dto::Boot,
        offer: &dto::ContentOffer,
        count: u16,
        expected: u64,
        first: bool,
    ) -> Result<Item, Failed> {
        let valid = (|| -> Result<bool, v2::Error> {
            Ok(offer.operation_id()?.0 == operation_id
                && offer.message_id()?.0 == message_id
                && offer.source()? == *source
                && offer.proxy()?.is_none()
                && offer.real_sender()? == device_id(peer)?
                && offer.offer()? == self_offer_id(peer, operation_id, offer.item_id()?)?
                && offer.availability()? == dto::ContentAvailability::Ready
                && offer.bytes()?.is_some()
                && !matches!(offer.kind()?, dto::ContentKind::Text | dto::ContentKind::Url))
        })()
        .map_err(Failed::invalid)?;
        if !valid {
            return Err(Failed::invalid("item does not belong to this transfer"));
        }
        let mut announcement = content_announcement(offer).map_err(Failed::invalid)?;
        announcement.sensitive = self
            .direct
            .lock()
            .expect("direct receive mutex poisoned")
            .get(&(peer.to_owned(), operation_id))
            .is_some_and(|receive| receive.sensitive);
        announcement.source.availability = crate::SourceAvailability::Ready;
        // The sender's original size: the image keeps its shape from the
        // placeholder to the finished file.
        let image_size = offer
            .image_width()
            .and_then(|width| Ok(width.zip(offer.image_height()?)))
            .map_err(Failed::invalid)?
            .filter(|_| announcement.kind == AttachmentKind::Image);
        let (record, path, attempt) = {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            let record = store
                .insert_announcement(now_ms(), announcement)
                .map_err(Failed::store)?;
            store.set_direct(&[record.id], peer, None).map_err(Failed::store)?;
            if let Some(size) = image_size {
                store.set_image_size(record.id, size).map_err(Failed::store)?;
            }
            let path = store.begin_download(record.id).map_err(Failed::store)?;
            let attempt = store.attempt(record.id).expect("download just began");
            (record, path, attempt)
        };
        if let Some(receive) = self
            .direct
            .lock()
            .expect("direct receive mutex poisoned")
            .get_mut(&(peer.to_owned(), operation_id))
        {
            receive.records.push(record.id);
        }
        let file = fs::OpenOptions::new().append(true).open(&path).map_err(Failed::io)?;
        let refreshed = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .by_id(record.id);
        if let Ok(Some(record)) = refreshed {
            self.announcement(&record, false);
        }
        if first {
            self.transport.emit(TransferEvent::DirectIncoming {
                operation_id,
                sender_id: peer.to_owned(),
                record_ids: vec![record.id],
                item_count: u32::from(count),
                first_name: record.display_name.clone(),
                total_bytes: expected,
            });
        }
        Ok(Item {
            record: record.id,
            attempt,
            kind: record.kind,
            name: record.display_name.clone(),
            path,
            file,
            size: record.size,
            written: 0,
            content_id: record.content_id,
        })
    }

    /// Completes one item; a folder archive is unpacked into its directory
    /// first (off the network task).
    async fn finish_item(&self, operation_id: [u8; 16], mut item: Item) -> Result<(), Failed> {
        item.file.flush().map_err(Failed::io)?;
        drop(item.file);
        if item.kind == AttachmentKind::Folder {
            let parent = item.path.parent().map(Path::to_path_buf).unwrap_or_default();
            // The folder takes its own name; the archive moves aside first
            // (its download name may be the same).
            let archive = parent.join(".folder.zip");
            fs::rename(&item.path, &archive).map_err(Failed::io)?;
            let directory = parent.join(crate::store::stored_file_name(
                AttachmentKind::Folder,
                &item.name,
                None,
                None,
            ));
            let unpacked = directory.clone();
            let (files, tree) = tokio::task::spawn_blocking(move || {
                let files = unpack_folder(&archive, &unpacked)?;
                Ok::<_, std::io::Error>((files, local_tree(&unpacked)?))
            })
            .await
            .map_err(|error| Failed::io(std::io::Error::other(error)))?
            .map_err(Failed::io)?;
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            if store.attempt(item.record) != Some(item.attempt) {
                return Err(Failed::ended(DirectEnd::ReceiverCancelled));
            }
            store
                .complete_folder(item.record, &directory, files)
                .map_err(Failed::store)?;
            let folder = store
                .by_id(item.record)
                .map_err(Failed::store)?
                .ok_or_else(|| Failed::invalid("the folder record is gone"))?;
            record_local_files(&mut store, &folder, &tree).map_err(Failed::store)?;
        } else {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            if store.attempt(item.record) != Some(item.attempt) {
                return Err(Failed::ended(DirectEnd::ReceiverCancelled));
            }
            store.complete_download(item.record).map_err(Failed::store)?;
        }
        self.transport.emit(TransferEvent::DownloadProgress {
            operation_id,
            record_id: item.record,
            received: item.size,
            total: Some(item.size),
        });
        let refreshed = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .by_id(item.record);
        if let Ok(Some(record)) = refreshed {
            self.announcement(&record, false);
        }
        Ok(())
    }

    fn reply(&self, peer: &str, operation_id: [u8; 16], status: dto::ShareStatus) {
        if let Ok(result) =
            dto::ClipboardShareResult::from_raw([(1, id16(operation_id).write()), (2, status.write())].into())
        {
            // A sender that is already gone needs no answer.
            let _ = self
                .transport
                .send_message(peer, v2::Message::ClipboardShareResult(result));
        }
    }

    fn direct_ended(&self, operation_id: [u8; 16], record_ids: Vec<i64>, failure: Option<DeliveryFailure>) {
        self.transport.emit(TransferEvent::DirectEnded {
            operation_id,
            record_ids,
            completed: failure.is_none(),
            failure,
        });
    }

    fn remove_received(&self, operation_id: [u8; 16], records: Vec<i64>) {
        {
            let mut store = self.coordinator.store.lock().expect("transfer store mutex poisoned");
            for id in &records {
                let _ = store.delete_message(*id);
            }
        }
        self.direct_ended(operation_id, records.clone(), Some(DeliveryFailure::Cancelled));
        if !records.is_empty() {
            self.transport.emit(TransferEvent::Removed { record_ids: records });
        }
    }

    /// The sending phone cancelled (`ClipboardShareCancel`). A running
    /// receive stops and drops its records; one that already ended (its
    /// stream broke first) has its stored records deleted now.
    pub(super) fn direct_cancelled_by_sender(&self, peer: &str, operation_id: [u8; 16]) {
        let key = (peer.to_owned(), operation_id);
        {
            let mut direct = self.direct.lock().expect("direct receive mutex poisoned");
            if let Some(receive) = direct.get(&key) {
                if receive.started {
                    receive.end.send_replace(Some(DirectEnd::SenderCancelled));
                } else {
                    direct.remove(&key);
                }
                return;
            }
        }
        let records: Vec<i64> = self
            .coordinator
            .store
            .lock()
            .expect("transfer store mutex poisoned")
            .direct_received(peer, operation_id)
            .map(|records| records.iter().map(|record| record.id).collect())
            .unwrap_or_default();
        if !records.is_empty() {
            self.remove_received(operation_id, records);
        }
    }

    /// The link to `peer` went down: its receives fail as disconnected.
    pub(super) fn direct_peer_lost(&self, peer: &str) {
        self.direct
            .lock()
            .expect("direct receive mutex poisoned")
            .retain(|(source, _), receive| {
                if source != peer {
                    return true;
                }
                receive.end.send_replace(Some(DirectEnd::PeerLost));
                receive.started
            });
    }

    /// This phone's user cancels the receive that record `record_id`
    /// belongs to. Both ends end it as cancelled; it is not retried.
    pub fn cancel_receive(&self, record_id: i64) -> Result<(), MobileV2TransportError> {
        let direct = self.direct.lock().expect("direct receive mutex poisoned");
        let receive = direct
            .values()
            .find(|receive| receive.started && receive.records.contains(&record_id))
            .ok_or(MobileV2TransportError::NotFound(record_id))?;
        receive.end.send_replace(Some(DirectEnd::ReceiverCancelled));
        Ok(())
    }
}

/// Completes when a receive-ending signal is set; never when its sender is
/// gone without one.
async fn ended(end: &mut tokio::sync::watch::Receiver<Option<DirectEnd>>) {
    if end.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The item id inside a `...:r:<item>:<rep>` content id.
fn item_id_of(content_id: &str) -> u32 {
    content_id
        .rsplit(':')
        .nth(1)
        .and_then(|item| item.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Unpacks a received folder archive (Stored ZIP, `archive_folder`) into
/// `directory` and removes the archive. Entries must stay inside the folder;
/// returns the number of files.
fn unpack_folder(archive: &Path, directory: &Path) -> std::io::Result<u32> {
    use std::io::Error;
    let mut zip = zip::ZipArchive::new(fs::File::open(archive)?).map_err(Error::other)?;
    if zip.len() > MAX_FOLDER_ENTRIES {
        return Err(Error::other("received folder has too many entries"));
    }
    fs::create_dir_all(directory)?;
    let mut files = 0u32;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(Error::other)?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| Error::other("received folder entry leaves the folder"))?;
        let target = directory.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = fs::File::create(&target)?;
            std::io::copy(&mut entry, &mut output)?;
            files += 1;
        } else {
            return Err(Error::other("received folder contains a symlink or special entry"));
        }
    }
    drop(zip);
    fs::remove_file(archive)?;
    Ok(files)
}

/// One entry of a received folder on this phone. Ids follow a fixed walk
/// (breadth first, names sorted): the root is 1, so the same directory
/// always gives the same ids. Child records and listings both use them.
#[derive(Debug)]
pub(super) struct LocalNode {
    pub id: u64,
    pub parent: Option<u64>,
    pub name: String,
    pub path: PathBuf,
    pub directory: bool,
    pub size: u64,
}

pub(super) const LOCAL_ROOT: u64 = 1;

pub(super) fn local_tree(root: &Path) -> std::io::Result<Vec<LocalNode>> {
    let mut nodes = vec![LocalNode {
        id: LOCAL_ROOT,
        parent: None,
        name: String::new(),
        path: root.to_path_buf(),
        directory: true,
        size: 0,
    }];
    let mut next = 0;
    while next < nodes.len() {
        if nodes[next].directory {
            let parent = nodes[next].id;
            let mut children: Vec<_> = fs::read_dir(&nodes[next].path)?.collect::<Result<_, _>>()?;
            children.sort_by_key(fs::DirEntry::file_name);
            for child in children {
                let metadata = child.metadata()?;
                let id = nodes.len() as u64 + 1;
                nodes.push(LocalNode {
                    id,
                    parent: Some(parent),
                    name: child.file_name().to_string_lossy().into_owned(),
                    path: child.path(),
                    directory: metadata.is_dir(),
                    size: if metadata.is_dir() { 0 } else { metadata.len() },
                });
            }
        }
        next += 1;
    }
    Ok(nodes)
}

/// Every file of a received folder becomes a complete child record of it,
/// keyed by its listing id (`MessageRecord::directory_node`), so the host's
/// folder browser shows and opens them like downloaded files of a PC folder.
fn record_local_files(
    store: &mut TransferStore,
    folder: &MessageRecord,
    tree: &[LocalNode],
) -> Result<(), crate::StoreError> {
    let offer = parse_content_id(&folder.content_id).map_err(|_| crate::StoreError::InvalidContentId)?;
    for node in tree.iter().filter(|node| !node.directory) {
        let mime = image_mime_for_name(&node.name);
        let child = store.insert_directory_file(
            now_ms(),
            folder.id,
            IncomingAnnouncement {
                sender_id: folder.sender_id.clone(),
                proxy_id: None,
                content_id: file_content_id(&offer, node.id, 1).map_err(|_| crate::StoreError::InvalidContentId)?,
                kind: if mime.is_some() {
                    AttachmentKind::Image
                } else {
                    AttachmentKind::File
                },
                display_name: node.name.clone(),
                mime: mime.map(str::to_owned),
                size: node.size,
                text: None,
                rich_text_format: None,
                source: SourceState {
                    source_id: folder.sender_id.clone(),
                    availability: crate::SourceAvailability::Ready,
                },
                operation_id: folder.operation_id,
                message_id: folder.message_id,
                item_id: folder.item_id,
                representation_id: 0,
                sensitive: folder.sensitive,
            },
        )?;
        store.set_local_body(child.id, &node.path)?;
        if let Some(peer) = &folder.direct_peer {
            store.set_direct(&[child.id], peer, None)?;
        }
    }
    Ok(())
}

impl<L: MobileV2Link> MobileV2Session<L> {
    /// Lists directory `node` (0: the root) of a folder received phone to
    /// phone from its unpacked copy on this phone; the sending phone keeps
    /// nothing to list. One page, pushed as `DirectoryPage` like a remote
    /// listing. Without its local copy (cleared cache, failed receive) the
    /// folder is unavailable.
    pub(super) fn list_local_directory(
        &self,
        operation_id: [u8; 16],
        folder: &MessageRecord,
        node: u64,
    ) -> Result<(), MobileV2TransportError> {
        let root = folder
            .local_path
            .as_deref()
            .filter(|_| folder.download == crate::DownloadState::Complete)
            .map(Path::new)
            .filter(|path| path.is_dir())
            .ok_or(MobileV2TransportError::SourceUnavailable(
                match folder.source_availability {
                    crate::SourceAvailability::Ready | crate::SourceAvailability::SourceOffline => {
                        crate::SourceAvailability::NoSuchItem
                    }
                    gone => gone,
                },
            ))?;
        let tree = local_tree(root).map_err(io_failure)?;
        let directory = if node == 0 { LOCAL_ROOT } else { node };
        if !tree.iter().any(|entry| entry.id == directory && entry.directory) {
            return Err(MobileV2TransportError::NotFound(folder.id));
        }
        let mut entries: Vec<DirectoryEntry> = tree
            .iter()
            .filter(|entry| entry.parent == Some(directory))
            .map(|entry| DirectoryEntry {
                id: entry.id,
                parent: entry.parent,
                name: entry.name.clone(),
                kind: if entry.directory { "directory" } else { "file" }.into(),
                size: (!entry.directory).then_some(entry.size),
                version: 1,
                child: None,
            })
            .collect();
        // Each file's complete child carries its `local_path`: the host makes
        // the folder page thumbnails from it (nothing to ask the sender).
        self.attach_children(folder.id, &mut entries)?;
        self.transport.emit(TransferEvent::DirectoryPage {
            operation_id,
            folder_message_id: folder.id,
            // As asked: the host keys the root page under 0, not LOCAL_ROOT.
            directory: node,
            entries,
            next_version: None,
            next_offset: None,
        });
        Ok(())
    }
}
