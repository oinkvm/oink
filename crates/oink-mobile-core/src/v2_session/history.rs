//! Foreground-only history subscription. Phones are leaves, never delta forwarders.
use super::*;
use crate::store::history::PhoneHistory;
use oink_foundation::clipboard_history as h;

#[derive(Debug)]
pub(super) struct ClockPublication {
    payload: SharePayload,
    peers: Vec<Peer>,
    queries: HashMap<String, [u8; 16]>,
    clock: u64,
}

#[derive(Default)]
pub(super) struct HistorySync {
    group: Option<GroupId>,
    foreground: bool,
    peer: Option<String>,
    unsubscribe: std::collections::HashSet<String>,
    index_request: Option<[u8; 16]>,
    page_request: Option<[u8; 16]>,
    before: Option<h::OrderKey>,
    more: bool,
    pending: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
}
impl<L: MobileV2Link> MobileV2Session<L> {
    pub(super) fn sync_history_group(&self) -> Result<(), MobileV2TransportError> {
        let group = self.transport.link.group_id().unwrap_or(self.transport.identity.group);
        if self.history.lock().expect("history mutex").group == Some(group) {
            return Ok(());
        }
        self.transport
            .abort_all_clock_publications((DeliveryFailure::Interrupted, "clipboard group changed".into()));
        let removed = self.with_store(|store| store.initialize_history_group(group))?;
        let mut sync = self.history.lock().expect("history mutex");
        sync.group = Some(group);
        sync.peer = None;
        sync.index_request = None;
        sync.page_request = None;
        sync.pending.take();
        sync.unsubscribe.clear();
        drop(sync);
        self.fresh_clipboard.group_changed(None);
        if !removed.is_empty() {
            self.transport.emit(TransferEvent::Removed { record_ids: removed });
        }
        Ok(())
    }
    pub fn set_history_foreground(&self, foreground: bool) -> Result<(), MobileV2TransportError> {
        let previous = {
            let mut sync = self.history.lock().expect("history mutex");
            sync.foreground = foreground;
            if !foreground {
                sync.index_request = None;
                sync.page_request = None;
                sync.pending.take();
                sync.peer.take()
            } else {
                None
            }
        };
        if !foreground {
            self.with_store(|store| store.set_history_paging(false));
        }
        if let Some(peer) = previous {
            self.history.lock().expect("history mutex").unsubscribe.insert(peer);
        }
        self.flush_history_unsubscribes()?;
        if foreground {
            self.update_history_peer()?;
        }
        Ok(())
    }
    pub(super) fn update_history_peer(&self) -> Result<(), MobileV2TransportError> {
        self.flush_history_unsubscribes()?;
        let chosen = self
            .peers()
            .into_iter()
            .filter(|p| {
                p.kind == crate::PeerKind::Pc
                    && self.transport.link.supports_history(&p.id)
                    && !p.relayed
                    && p.state == crate::PeerState::Ready
                    && p.accepts_clipboard
                    && !p.clipboard_pending
            })
            .min_by_key(|p| p.connection_order)
            .map(|p| p.id);
        let (old, next) = {
            let mut sync = self.history.lock().expect("history mutex");
            if !sync.foreground || sync.peer == chosen {
                return Ok(());
            }
            let old = sync.peer.take();
            sync.peer = chosen.clone();
            sync.before = None;
            sync.more = true;
            sync.index_request = None;
            sync.page_request = None;
            sync.pending.take();
            (old, chosen)
        };
        self.with_store(|store| store.set_history_paging(next.is_some()));
        if let Some(old) = old {
            self.history.lock().expect("history mutex").unsubscribe.insert(old);
            self.flush_history_unsubscribes()?;
        }
        if let Some(peer) = next {
            let result = self
                .send_history_summary(&peer, true)
                .and_then(|()| self.request_history_index(&peer))
                .and_then(|()| self.start_history_page(&peer, None));
            if let Err(error) = result {
                let mut sync = self.history.lock().expect("history mutex");
                sync.peer = None;
                sync.index_request = None;
                sync.page_request = None;
                self.with_store(|store| store.set_history_paging(false));
                return Err(error);
            }
        }
        self.refresh_history_availability()?;
        Ok(())
    }
    fn flush_history_unsubscribes(&self) -> Result<(), MobileV2TransportError> {
        let peers = self.peers();
        let ready: Vec<_> = self
            .history
            .lock()
            .expect("history mutex")
            .unsubscribe
            .iter()
            .filter(|id| {
                peers.iter().any(|p| {
                    p.id == **id
                        && p.state == crate::PeerState::Ready
                        && p.accepts_clipboard
                        && !p.clipboard_pending
                        && !p.relayed
                })
            })
            .cloned()
            .collect();
        // A pause prohibits even history control traffic. If the app backgrounds
        // while paused, withdraw its old subscription once that PC becomes usable.
        for peer in ready {
            self.send_history_summary(&peer, false)?;
            self.history.lock().expect("history mutex").unsubscribe.remove(&peer);
        }
        Ok(())
    }
    pub(crate) fn fail_history_receive(&self, peer: &str, first: &v2::Message, detail: String) {
        let mut sync = self.history.lock().expect("history mutex");
        if sync.peer.as_deref() != Some(peer) {
            return;
        }
        match first {
            v2::Message::HistoryRecords(records) if records.request_id().ok().map(|id| id.0) == sync.page_request => {
                sync.page_request = None;
                if let Some(reply) = sync.pending.take() {
                    let _ = reply.send(Err(detail));
                }
            }
            v2::Message::HistoryIndex(index) if index.request_id().ok().map(|id| id.0) == sync.index_request => {
                sync.index_request = None;
            }
            _ => {}
        }
    }
    fn history_snapshot(&self) -> Result<PhoneHistory, MobileV2TransportError> {
        self.with_store(|store| store.load_history()).map_err(Into::into)
    }
    fn send_history_summary(&self, peer: &str, subscribe: bool) -> Result<(), MobileV2TransportError> {
        let history = self.history_snapshot()?;
        let group = self.transport.link.group_id().unwrap_or(self.transport.identity.group);
        let summary = dto::HistorySummary::from_raw(
            [
                (1, id16(group.0).write()),
                (2, RawValue::Bytes(history.digest().to_vec())),
                (3, (history.index.len() as u32).write()),
                (
                    4,
                    history
                        .index
                        .last()
                        .map(|e| dto::HistoryOrder::from_domain(e.key).expect("valid index").write())
                        .unwrap_or(RawValue::Null),
                ),
                (5, subscribe.write()),
            ]
            .into(),
        )
        .map_err(protocol_error)?;
        self.transport.send_message(peer, v2::Message::HistorySummary(summary))
    }
    fn request_history_index(&self, peer: &str) -> Result<(), MobileV2TransportError> {
        let request = rand::random();
        {
            let mut sync = self.history.lock().expect("history mutex");
            if sync.index_request.is_some() {
                return Ok(());
            }
            sync.index_request = Some(request);
        }
        let request =
            dto::HistoryIndexRequest::from_raw([(1, id16(request).write())].into()).map_err(protocol_error)?;
        self.transport
            .send_message(peer, v2::Message::HistoryIndexRequest(request))
    }
    fn start_history_page(&self, peer: &str, before: Option<h::OrderKey>) -> Result<(), MobileV2TransportError> {
        let request = rand::random();
        self.history.lock().expect("history mutex").page_request = Some(request);
        let pull = dto::HistoryPull::from_query(request, dto::HistoryQuery::Page { before, max: 20 }, true)
            .map_err(protocol_error)?;
        if let Err(e) = self.transport.send_message(peer, v2::Message::HistoryPull(pull)) {
            self.history.lock().expect("history mutex").page_request = None;
            return Err(e);
        }
        Ok(())
    }
    pub fn history_has_more(&self) -> bool {
        let sync = self.history.lock().expect("history mutex");
        sync.peer.is_some() && (sync.more || sync.page_request.is_some())
    }
    pub async fn load_older_history(&self) -> Result<(), MobileV2TransportError> {
        let (peer, before, reply) = {
            let mut sync = self.history.lock().expect("history mutex");
            if !sync.foreground || !sync.more {
                return Ok(());
            }
            if sync.pending.is_some() {
                return Err(link_error("history page already requested"));
            }
            let Some(peer) = sync.peer.clone() else {
                return Ok(());
            };
            let (send, reply) = tokio::sync::oneshot::channel();
            sync.pending = Some(send);
            (peer, sync.before, reply)
        };
        let needs_page = self.history.lock().expect("history mutex").page_request.is_none();
        if needs_page && let Err(error) = self.start_history_page(&peer, before) {
            self.history.lock().expect("history mutex").pending.take();
            return Err(error);
        }
        reply.await.map_err(link_error)?.map_err(link_error)
    }
    pub(super) fn receive_history(&self, peer: &str, message: v2::Message) -> Result<(), MobileV2TransportError> {
        {
            let sync = self.history.lock().expect("history mutex");
            let own_delta = matches!(&message,v2::Message::HistoryDelta(d) if d.to_domain().ok().is_some_and(|change|hex(&change.author().0)==peer));
            let direct = self.peers().iter().any(|p| {
                p.id == peer
                    && p.state == crate::PeerState::Ready
                    && !p.relayed
                    && p.accepts_clipboard
                    && !p.clipboard_pending
            });
            if !sync.foreground || !direct || (sync.peer.as_deref() != Some(peer) && !own_delta) {
                return Ok(());
            }
        }
        match message {
            v2::Message::HistoryIndexRequest(request) => {
                let history = self.history_snapshot()?;
                let entries = history
                    .index
                    .iter()
                    .map(|e| dto::HistoryIndexEntry::from_domain(e).map(|d| d.write()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(protocol_error)?;
                let index = dto::HistoryIndex::from_raw(
                    [
                        (1, request.request_id().map_err(protocol_error)?.write()),
                        (2, RawValue::Array(entries)),
                        (3, RawValue::Bytes(history.digest().to_vec())),
                    ]
                    .into(),
                )
                .map_err(protocol_error)?;
                self.transport.send_message(peer, v2::Message::HistoryIndex(index))?;
            }
            v2::Message::HistoryPull(pull) => {
                let history = self.history_snapshot()?;
                let page = match pull.query().map_err(protocol_error)? {
                    dto::HistoryQuery::Page { before, max } => {
                        history
                            .bodies
                            .page(before, max as usize, pull.inline().map_err(protocol_error)?)
                    }
                    dto::HistoryQuery::Ids(ids) => h::HistoryPage {
                        records: history
                            .bodies
                            .records()
                            .iter()
                            .filter(|r| ids.contains(&r.id))
                            .cloned()
                            .collect(),
                        holders: history
                            .bodies
                            .holders()
                            .iter()
                            .filter(|h| ids.contains(&h.record_id))
                            .cloned()
                            .collect(),
                        more: false,
                        next: None,
                    },
                };
                let records = dto::HistoryRecords::from_page(
                    pull.request_id().map_err(protocol_error)?.0,
                    &page,
                    history.digest(),
                )
                .map_err(protocol_error)?;
                self.transport
                    .send_message(peer, v2::Message::HistoryRecords(records))?;
            }
            v2::Message::HistorySummary(summary) => {
                if summary.group().map_err(protocol_error)?.0
                    != self
                        .transport
                        .link
                        .group_id()
                        .unwrap_or(self.transport.identity.group)
                        .0
                {
                    return Err(link_error("history group mismatch"));
                }
                if summary.digest().map_err(protocol_error)?.0 != self.history_snapshot()?.digest() {
                    self.request_history_index(peer)?;
                }
            }
            v2::Message::HistoryIndex(index) => {
                let request = index.request_id().map_err(protocol_error)?.0;
                {
                    let mut sync = self.history.lock().expect("history mutex");
                    if sync.index_request != Some(request) {
                        return Ok(());
                    }
                    sync.index_request = None;
                }
                let mut remote = index.domain_entries().map_err(protocol_error)?;
                if h::window_digest(&remote) != index.digest().map_err(protocol_error)?.0 {
                    return Err(link_error("invalid history index digest"));
                }
                for entry in &mut remote {
                    entry.holders.retain(|(holder, _)| self.pc_holder(*holder));
                }
                let mut history = self.history_snapshot()?;
                self.retain_pc_holders(&mut history);
                let changed = h::reconcile_indexes(&history.index, &remote).pull;
                history.merge_index(&remote)?;
                let removed = self.with_store(|store| store.save_history(&history))?;
                if !removed.is_empty() {
                    self.transport.emit(TransferEvent::Removed { record_ids: removed });
                }
                // Only refresh already materialized bodies. Missing older bodies are loaded by scrolling.
                let ids: Vec<_> = changed
                    .into_iter()
                    .filter(|id| history.bodies.get(*id).is_some())
                    .collect();
                for ids in ids.chunks(20) {
                    let pull = dto::HistoryPull::from_query(rand::random(), dto::HistoryQuery::Ids(ids.to_vec()), true)
                        .map_err(protocol_error)?;
                    self.transport.send_message(peer, v2::Message::HistoryPull(pull))?;
                }
            }
            v2::Message::HistoryRecords(records) => {
                let mut page = records.to_page().map_err(protocol_error)?;
                page.holders.retain(|holder| self.pc_holder(holder.holder));
                let request = records.request_id().map_err(protocol_error)?.0;
                if self.history.lock().expect("history mutex").page_request == Some(request)
                    && (page.records.len() > 20 || (page.more && page.next.is_none()))
                {
                    return Err(link_error("invalid history page"));
                }
                let mut history = self.history_snapshot()?;
                history.merge_page(&page)?;
                let hidden_page =
                    !page.records.is_empty() && page.records.iter().all(|r| history.hidden.contains(&r.id));
                self.save_and_materialize(history)?;
                let mut sync = self.history.lock().expect("history mutex");
                let mut next_hidden = None;
                if sync.page_request == Some(request) {
                    sync.page_request = None;
                    sync.before = page.next;
                    sync.more = page.more;
                    self.with_store(|store| store.set_history_paging(page.more));
                    if hidden_page && page.more {
                        next_hidden = page.next;
                    } else if let Some(reply) = sync.pending.take() {
                        let _ = reply.send(Ok(()));
                    }
                }
                drop(sync);
                if let Some(before) = next_hidden {
                    self.start_history_page(peer, Some(before))?;
                }
                if records.digest().map_err(protocol_error)?.0 != self.history_snapshot()?.digest() {
                    self.request_history_index(peer)?;
                }
            }
            v2::Message::HistoryDelta(delta) => {
                let change = delta.to_domain().map_err(protocol_error)?;
                if !self.transport.link.is_member(&hex(&change.author().0)) {
                    return Err(link_error("history author is not a member"));
                }
                let mut history = self.history_snapshot()?;
                match change {
                    h::HistoryChange::Record(record) => history.merge_page(&h::HistoryPage {
                        records: vec![record],
                        holders: vec![],
                        more: false,
                        next: None,
                    })?,
                    h::HistoryChange::Holder(holder) => {
                        if !self.pc_holder(holder.holder) {
                            return Ok(());
                        }
                        // Unknown/evicted ids must never re-enter the window through holder updates.
                        if let Some(entry) = history.index.iter_mut().find(|e| e.id == holder.record_id) {
                            if let Some((_, rev)) = entry.holders.iter_mut().find(|(id, _)| *id == holder.holder) {
                                *rev = (*rev).max(holder.rev);
                            } else {
                                entry.holders.push((holder.holder, holder.rev));
                            }
                            history.bodies.merge(vec![], vec![holder]).map_err(link_error)?;
                        }
                    }
                }
                self.save_and_materialize(history)?;
                let selected_pc = self.history.lock().expect("history mutex").peer.as_deref() == Some(peer);
                if selected_pc && delta.digest().map_err(protocol_error)?.0 != self.history_snapshot()?.digest() {
                    self.request_history_index(peer)?;
                }
            }
            _ => {}
        }
        self.refresh_history_availability()
    }
    fn save_and_materialize(&self, mut history: PhoneHistory) -> Result<(), MobileV2TransportError> {
        self.retain_pc_holders(&mut history);
        let removed = self.with_store(|store| store.save_history(&history))?;
        if !removed.is_empty() {
            self.transport.emit(TransferEvent::Removed { record_ids: removed });
        }
        let times = history.bodies.display_times();
        for record in history.bodies.records() {
            if history.hidden.contains(&record.id) {
                continue;
            }
            let mut display = record.clone();
            display.time_ms = times
                .iter()
                .find(|(id, _)| *id == record.id)
                .map_or(record.time_ms, |(_, time)| *time);
            if record.origin == self.device_id() {
                self.with_store(|store| store.order_own_history(&display))?;
                continue;
            }
            self.materialize_history(&display)?;
        }
        Ok(())
    }
    fn materialize_history(&self, record: &h::ClipRecord) -> Result<(), MobileV2TransportError> {
        let offer = dto::HistoryRecord::from_domain(record)
            .and_then(|r| r.id())
            .map_err(protocol_error)?;
        let existing = self.with_store(|store| store.shared_records(record.id))?;
        let operation_id = existing.first().map(|r| r.operation_id).unwrap_or_else(rand::random);
        let has_files = record.items.iter().any(|item| item.file_node.is_some());
        for item in &record.items {
            if has_files && item.file_node.is_none() {
                continue;
            }
            let formats: Vec<_> = record.formats.iter().filter(|f| f.item == item.id).collect();
            let format = formats
                .iter()
                .find(|f| {
                    matches!(
                        f.portable,
                        Some(h::PortableFormat::Text | h::PortableFormat::Png | h::PortableFormat::Url)
                    )
                })
                .or_else(|| formats.first());
            if format.is_none() && item.file_node.is_none() {
                continue;
            }
            let representation = format.map_or(0, |format| format.representation);
            let kind = match item.kind {
                h::ItemKind::Text => AttachmentKind::Text,
                h::ItemKind::Image => AttachmentKind::Image,
                h::ItemKind::File if item.name.as_deref().and_then(image_mime_for_name).is_some() => {
                    AttachmentKind::Image
                }
                h::ItemKind::File => AttachmentKind::File,
                h::ItemKind::Directory => AttachmentKind::Folder,
                _ => continue,
            };
            let content_id = match item.file_node.zip(item.file_version) {
                Some((node, version)) => file_content_id(&offer, node, version),
                None => content_id_for_representation(&offer, item.id, representation),
            }
            .map_err(protocol_error)?;
            let text = record
                .inline
                .iter()
                .find(|p| p.item == item.id && matches!(p.format, h::PortableFormat::Text | h::PortableFormat::Url))
                .and_then(|p| String::from_utf8(p.bytes.clone()).ok());
            let incoming = IncomingAnnouncement {
                sender_id: hex(&record.origin.0),
                proxy_id: record.proxy.map(|p| hex(&p.0)),
                content_id,
                kind,
                display_name: item.name.clone().unwrap_or_else(|| "clipboard".into()),
                mime: Some(item.mime.clone()),
                size: item.bytes.unwrap_or(0),
                text,
                rich_text_format: None,
                source: crate::SourceState {
                    source_id: hex(&record.id.clip.origin.0),
                    availability: crate::SourceAvailability::SourceOffline,
                },
                operation_id,
                message_id: operation_id,
                item_id: item.id,
                representation_id: representation,
                sensitive: record.sensitive(),
            };
            let stored = self.with_store(|store| {
                let stored = store.insert_announcement(record.time_ms, incoming.clone())?;
                if kind == AttachmentKind::Image
                    && item.file_node.is_none()
                    && let Some(inline) = record.inline.iter().find(|body| {
                        body.item == item.id
                            && body.representation == representation
                            && body.format == h::PortableFormat::Png
                    })
                {
                    store.history_inline_png(stored.id, &inline.bytes)?;
                }
                if kind == AttachmentKind::Text {
                    let rich = record
                        .inline
                        .iter()
                        .filter(|p| {
                            p.item == item.id && matches!(p.format, h::PortableFormat::Html | h::PortableFormat::Rtf)
                        })
                        .min_by_key(|p| p.format as u8);
                    if let Some(rich) = rich {
                        let mut incoming = incoming;
                        incoming.content_id = content_id_for_representation(&offer, item.id, rich.representation)
                            .map_err(|e| crate::StoreError::History(e.to_string()))?;
                        incoming.representation_id = rich.representation;
                        let format = if rich.format == h::PortableFormat::Html {
                            crate::RichTextFormat::Html
                        } else {
                            crate::RichTextFormat::Rtf
                        };
                        store.history_inline_rich(stored.id, incoming, &rich.bytes, format)?;
                    }
                }
                store.link_shared(stored.id, record.id)?;
                store.set_history_order(stored.id, record.time_ms, record.key())?;
                store.history_item_metadata(stored.id, record.time_ms, item)?;
                if kind == AttachmentKind::Image
                    && stored.thumbnail_path.is_none()
                    && let Some(preview) = &record.preview
                {
                    store.set_inline_preview(stored.id, preview)?;
                }
                Ok::<_, crate::StoreError>(stored)
            })?;
            self.announcement(&stored, false);
        }
        Ok(())
    }
    pub(super) fn refresh_history_availability(&self) -> Result<(), MobileV2TransportError> {
        let mut history = self.history_snapshot()?;
        let old_digest = history.digest();
        self.retain_pc_holders(&mut history);
        if old_digest != history.digest() {
            let removed = self.with_store(|store| store.save_history(&history))?;
            if !removed.is_empty() {
                self.transport.emit(TransferEvent::Removed { record_ids: removed });
            }
        }
        let peers = self.peers();
        let changed = self.with_store(|store| store.refresh_holders(&history, &peers))?;
        for record in changed {
            self.announcement(&record, false);
        }
        Ok(())
    }
}
impl<L: MobileV2Link> V2Transport<L> {
    pub(super) fn is_clock_query(&self, peer: &str, request: [u8; 16]) -> bool {
        self.clock_publications
            .lock()
            .expect("clock publication mutex")
            .values()
            .any(|p| p.queries.get(peer) == Some(&request))
    }
    pub(super) fn abort_clock_publication(&self, operation: [u8; 16], failure: Failure) {
        let pending = self
            .clock_publications
            .lock()
            .expect("clock publication mutex")
            .remove(&operation);
        if pending.is_some() {
            self.settle(operation, Err(Some(failure)));
        }
    }
    pub(super) fn abort_all_clock_publications(&self, failure: Failure) {
        let pending = std::mem::take(&mut *self.clock_publications.lock().expect("clock publication mutex"));
        for operation in pending.into_keys() {
            self.settle(operation, Err(Some(failure.clone())));
        }
    }
    pub(super) fn abort_clock_peer(&self, peer: &str, failure: Failure) {
        let operations: Vec<_> = self
            .clock_publications
            .lock()
            .expect("clock publication mutex")
            .iter()
            .filter(|(_, p)| p.peers.iter().any(|p| p.kind == crate::PeerKind::Pc && p.id == peer))
            .map(|(id, _)| *id)
            .collect();
        for operation in operations {
            self.abort_clock_publication(operation, failure.clone());
        }
    }
    pub(super) fn check_clock_targets(&self) {
        let peers = self.peers();
        for peer in peers
            .iter()
            .filter(|peer| peer.state == crate::PeerState::Ready && !peer.accepts_clipboard)
        {
            self.abort_clock_peer(
                &peer.id,
                (DeliveryFailure::Paused, "clock source paused clipboard sharing".into()),
            );
        }
        let lost: Vec<_> = self
            .clock_publications
            .lock()
            .expect("clock publication mutex")
            .iter()
            .filter(|(_, publication)| {
                publication
                    .peers
                    .iter()
                    .filter(|p| p.kind == crate::PeerKind::Pc)
                    .any(|target| {
                        !peers.iter().any(|p| {
                            p.id == target.id
                                && !p.relayed
                                && p.kind == crate::PeerKind::Pc
                                && p.state == crate::PeerState::Ready
                                && p.accepts_clipboard
                                && !p.clipboard_pending
                        })
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        for operation in lost {
            self.abort_clock_publication(
                operation,
                (
                    DeliveryFailure::Interrupted,
                    "clock source is no longer available".into(),
                ),
            );
        }
    }
    pub(super) fn receive_publication_clock(
        &self,
        peer: &str,
        request: [u8; 16],
        result: Result<u64, v2::Error>,
    ) -> Result<(), v2::Error> {
        let (operation, ready) = {
            let mut publications = self.clock_publications.lock().expect("clock publication mutex");
            let Some(operation) = publications
                .iter()
                .find(|(_, p)| p.queries.get(peer) == Some(&request))
                .map(|(id, _)| *id)
            else {
                return Ok(());
            };
            let clock = match result {
                Ok(clock) => clock,
                Err(error) => {
                    publications.remove(&operation);
                    drop(publications);
                    self.settle(operation, Err(Some((DeliveryFailure::Interrupted, error.to_string()))));
                    return Err(error);
                }
            };
            // Persist even partial observations, including a barrier cancelled before
            // its remaining replies. The next publication must not reuse an older clock.
            let persisted = self.store.lock().expect("store mutex").observe_history_clock(clock);
            if let Err(error) = persisted {
                publications.remove(&operation);
                drop(publications);
                self.settle(operation, Err(Some((DeliveryFailure::Interrupted, error.to_string()))));
                return Err(v2::ErrorCode::StateConflict.into());
            }
            let publication = publications.get_mut(&operation).expect("clock publication");
            publication.clock = publication.clock.max(clock);
            publication.queries.remove(peer);
            let ready = publication.queries.is_empty();
            (operation, if ready { publications.remove(&operation) } else { None })
        };
        if let Some(publication) = ready {
            let result = self.publish_history_text(&publication.payload, publication.peers, publication.clock);
            // Release the clock barrier only after all publication sends were started.
            let failure = result.err().map(start_failure);
            self.settle(operation, Err(failure));
        }
        Ok(())
    }
    pub(super) fn broadcast_history_text(&self, payload: &SharePayload) -> Result<(), MobileV2TransportError> {
        let peers: Vec<_> = self
            .peers()
            .into_iter()
            .filter(|p| {
                p.state == crate::PeerState::Ready
                    && !p.relayed
                    && p.accepts_clipboard
                    && !p.clipboard_pending
                    && self.link.supports_history(&p.id)
            })
            .collect();
        if peers.is_empty() {
            return Err(MobileV2TransportError::NoTarget);
        }
        let queries: HashMap<_, _> = peers
            .iter()
            .filter(|p| p.kind == crate::PeerKind::Pc)
            .map(|p| (p.id.clone(), rand::random()))
            .collect();
        if queries.is_empty() {
            return self.publish_history_text(payload, peers, 0);
        }
        // A share extension and background capture do not subscribe to history.
        // Read clocks without bodies instead, including Empty PC clipboards, before
        // allocating the phone-authored identity. Never block the B0 receive task.
        {
            let mut deliveries = self.deliveries.lock().expect("delivery mutex");
            let Some(delivery) = deliveries.get_mut(&payload.operation_id) else {
                return Err(MobileV2TransportError::Cancelled);
            };
            delivery.pending += 1; // held until the clock barrier publishes or fails
        }
        self.clock_publications.lock().expect("clock publication mutex").insert(
            payload.operation_id,
            ClockPublication {
                payload: payload.clone(),
                peers,
                queries: queries.clone(),
                clock: 0,
            },
        );
        if self.delivery_cancelled(payload.operation_id) {
            self.abort_clock_publication(payload.operation_id, cancelled_failure());
            return Ok(());
        }
        for (peer, request) in queries {
            if !self.is_clock_query(&peer, request) {
                continue;
            }
            let mut identity = self.identity;
            if let Some(group) = self.link.group_id() {
                identity.group = group;
            }
            let result = current_query(&identity, request, 1)
                .map_err(protocol_error)
                .and_then(|query| self.send_message(&peer, query));
            if let Err(error) = result {
                self.abort_clock_publication(payload.operation_id, start_failure(error));
                break;
            }
        }
        Ok(())
    }
    fn publish_history_text(
        &self,
        payload: &SharePayload,
        peers: Vec<Peer>,
        clock: u64,
    ) -> Result<(), MobileV2TransportError> {
        if self.delivery_cancelled(payload.operation_id) {
            return Err(MobileV2TransportError::Cancelled);
        }
        let (record, digest, removed) = {
            let mut store = self.store.lock().expect("transfer store mutex poisoned");
            let mut history = store.load_history()?;
            history.clock = history
                .clock
                .max(clock)
                .checked_add(1)
                .ok_or_else(|| link_error("Lamport clock exhausted"))?;
            history.sequence = history
                .sequence
                .checked_add(1)
                .ok_or_else(|| link_error("clipboard sequence exhausted"))?;
            let id = oink_foundation::content::OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: self.identity.device,
                    seq: history.sequence,
                },
                generation: 1,
            };
            let mut record = h::ClipRecord {
                id,
                clock: history.clock,
                time_ms: now_ms(),
                origin: self.identity.device,
                proxy: None,
                sensitivity: payload.sensitivity,
                items: Vec::new(),
                formats: Vec::new(),
                digests: Vec::new(),
                inline: Vec::new(),
                preview: None,
                rev: 0,
            };
            let own = store.outgoing_records(payload.operation_id)?;
            for (index, item) in payload.items.iter().enumerate() {
                let item_id = index as u32;
                let reps = item.representations();
                let text = reps.first().ok_or_else(|| link_error("broadcast is not text"))?.1;
                let fingerprint = match item {
                    crate::SendItem::RichText { text, html, rtf } => {
                        oink_foundation::clipboard_dedupe::Fingerprint::rich_text(text, html.as_deref(), rtf.as_deref())
                    }
                    _ => oink_foundation::clipboard_dedupe::Fingerprint::text(text),
                };
                let mut digests = h::HistoryDigest::from_fingerprint(&fingerprint);
                for digest in &mut digests {
                    digest.item = item_id;
                }
                record.digests.extend(digests);
                record.items.push(h::HistoryItem {
                    id: item_id,
                    kind: h::ItemKind::Text,
                    mime: "text/plain".into(),
                    name: None,
                    bytes: Some(text.len() as u64),
                    dimensions: None,
                    child_count: None,
                    children_bytes: None,
                    file_node: None,
                    file_version: None,
                });
                for (rep, (format, body)) in reps.into_iter().enumerate() {
                    let (portable, name) = match format {
                        crate::TextFormat::Plain => (h::PortableFormat::Text, "mime:text/plain;charset=utf-8"),
                        crate::TextFormat::Html => (h::PortableFormat::Html, "mime:text/html;charset=utf-8"),
                        crate::TextFormat::Rtf => (h::PortableFormat::Rtf, "mime:application/rtf"),
                    };
                    record.formats.push(h::HistoryFormat {
                        item: item_id,
                        representation: rep as u32,
                        name: name.into(),
                        portable: Some(portable),
                        bytes: Some(body.len() as u64),
                        file_node: None,
                    });
                    record.inline.push(h::InlinePayload {
                        item: item_id,
                        representation: rep as u32,
                        format: portable,
                        bytes: body.as_bytes().to_vec(),
                    });
                }
                if let Some(own) = own.iter().find(|r| r.item_id == item_id) {
                    store.link_shared(own.id, id)?;
                    store.set_history_order(own.id, record.time_ms, record.key())?;
                }
            }
            record.redact();
            record.inline = h::select_inline(record.inline);
            record.validate().map_err(link_error)?;
            history.merge_page(&h::HistoryPage {
                records: vec![record.clone()],
                holders: vec![],
                more: false,
                next: None,
            })?;
            let removed = store.save_history(&history)?;
            (record, history.digest(), removed)
        };
        if !removed.is_empty() {
            self.emit(TransferEvent::Removed { record_ids: removed });
        }
        let record =
            dto::HistoryDelta::from_domain(&h::HistoryChange::Record(record), digest).map_err(protocol_error)?;
        let message = v2::Message::HistoryDelta(record);
        let oversized = v2::frame::encode(&message, v2::limits::LOW_FRAME)
            .map_err(protocol_error)?
            .len()
            > MAX_FRAME;
        let mut sent = false;
        for peer in peers {
            if oversized {
                if let Some(delivery) = self
                    .deliveries
                    .lock()
                    .expect("delivery mutex")
                    .get_mut(&payload.operation_id)
                {
                    delivery.pending += 1;
                }
                self.history_publications
                    .lock()
                    .expect("history publications mutex")
                    .insert((peer.id.clone(), payload.operation_id));
                let operation = ClipboardLowOperation {
                    operation_id: payload.operation_id,
                    read: None,
                    parts: vec![LowPart::Message(message.clone())],
                    total: 0,
                    cancel: Some(self.upload_cancel(payload.operation_id)),
                };
                if let Err(error) = self.link.send_low(&peer.id, operation) {
                    self.history_publications
                        .lock()
                        .expect("history publications mutex")
                        .remove(&(peer.id.clone(), payload.operation_id));
                    self.settle(payload.operation_id, Err(Some(start_failure(error))));
                    continue;
                }
            } else {
                self.send_message(&peer.id, message.clone())?;
                if let Some(delivery) = self
                    .deliveries
                    .lock()
                    .expect("delivery mutex")
                    .get_mut(&payload.operation_id)
                {
                    delivery.delivered = true;
                }
            }
            sent = true;
        }
        if !sent {
            return Err(MobileV2TransportError::NoTarget);
        }
        Ok(())
    }
}
impl<L: MobileV2Link> MobileV2Session<L> {
    pub(super) fn reject_history_holder(&self, id: i64, source: &str) -> Result<bool, MobileV2TransportError> {
        let history = self.history_snapshot()?;
        if !self.with_store(|s| s.reject_holder(id, source, &history))? {
            return Ok(false);
        }
        self.refresh_history_availability()?;
        Ok(self
            .with_store(|s| s.by_id(id))?
            .is_some_and(|r| r.source_availability == crate::SourceAvailability::Ready))
    }
}
pub(super) fn domain_offer(id: &dto::OfferId) -> Result<oink_foundation::content::OfferId, v2::Error> {
    Ok(oink_foundation::content::OfferId {
        clip: oink_foundation::content::ClipId {
            origin: DeviceId(
                id.clip()?
                    .origin()?
                    .public_key()?
                    .0
                    .try_into()
                    .map_err(|_| v2::ErrorCode::MalformedFrame)?,
            ),
            seq: id.clip()?.seq()?,
        },
        generation: id.generation()?,
    })
}
impl<L: MobileV2Link> MobileV2Session<L> {
    pub(crate) fn finish_history_send(
        &self,
        peer: &str,
        operation: [u8; 16],
        result: Result<(), MobileV2TransportError>,
    ) {
        let publication = self
            .transport
            .history_publications
            .lock()
            .expect("history publications mutex")
            .remove(&(peer.to_owned(), operation));
        if publication {
            // Low transport completion keeps the share extension alive; no PC Holding is awaited.
            self.transport
                .settle(operation, result.map_err(|e| Some(start_failure(e))));
        } else if let Err(error) = result {
            self.error(None, None, error.to_string());
        }
    }
}
impl<L: MobileV2Link> MobileV2Session<L> {
    pub(super) fn reply_clock_query(
        &self,
        peer: &str,
        query: &dto::ClipboardCurrentQuery,
    ) -> Result<(), MobileV2TransportError> {
        let group = self.transport.link.group_id().unwrap_or(self.transport.identity.group);
        if query.group().map_err(protocol_error)?.0 != group.0 {
            return Err(link_error("current query group mismatch"));
        }
        self.validate_boot(peer, &query.caller().map_err(protocol_error)?)
            .map_err(protocol_error)?;
        // Phones expose no clipboard body/holder. Empty still carries their durable
        // clock, useful to authenticated metadata clients and role-aware test peers.
        let clock = self.with_store(|store| store.load_history().map(|history| history.clock))?;
        let result = dto::ClipboardCurrentResult::from_raw(
            [
                (1, query.request_id().map_err(protocol_error)?.write()),
                (2, boot(self.transport.identity.boot).write()),
                (3, dto::CurrentResultStatus::Empty.write()),
                (7, clock.write()),
            ]
            .into(),
        )
        .map_err(protocol_error)?;
        self.transport
            .send_message(peer, v2::Message::ClipboardCurrentResult(result))
    }
    pub(super) fn purge_sensitive_for_head(&self, offer: &dto::ClipboardOffer) -> Result<bool, MobileV2TransportError> {
        self.with_store(|store| {
            store.observe_history_clock(offer.clock().map_err(|e| crate::StoreError::History(e.to_string()))?)
        })?;
        let id = domain_offer(&offer.id().map_err(protocol_error)?).map_err(protocol_error)?;
        let key = h::OrderKey {
            clock: offer.clock().map_err(protocol_error)?,
            origin: id.clip.origin,
            seq: id.clip.seq,
            generation: id.generation,
        };
        let sensitive = matches!(
            offer.sensitivity().map_err(protocol_error)?,
            dto::Sensitivity::Concealed | dto::Sensitivity::Transient
        );
        if sensitive && self.with_store(|store| store.has_newer_shared_head(key))? {
            return Ok(false);
        }
        let removed = self.with_store(|store| store.purge_sensitive_before(key))?;
        if !removed.is_empty() {
            self.transport.emit(TransferEvent::Removed { record_ids: removed });
        }
        Ok(true)
    }
}
impl<L: MobileV2Link> MobileV2Session<L> {
    fn pc_holder(&self, holder: DeviceId) -> bool {
        let id = hex(&holder.0);
        holder != self.device_id()
            && self.transport.link.is_member(&id)
            && self.transport.link.is_pc_member(&id)
            && !self
                .peers()
                .iter()
                .any(|peer| peer.id == id && peer.kind == crate::PeerKind::Mobile)
    }
    fn retain_pc_holders(&self, history: &mut PhoneHistory) {
        let pcs = history
            .index
            .iter()
            .flat_map(|entry| entry.holders.iter().map(|(id, _)| *id))
            .filter(|id| self.pc_holder(*id))
            .collect();
        history.bodies.retain_pc_holders(&pcs);
        for entry in &mut history.index {
            entry.holders.retain(|(holder, _)| pcs.contains(holder));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Link(
        Mutex<Vec<(String, v2::Message)>>,
        Mutex<Vec<ClipboardLowOperation>>,
        Mutex<Weak<MobileV2Session<Self>>>,
    );
    impl MobileV2Link for Link {
        fn send_frame(&self, peer: &str, frame: Vec<u8>) -> Result<(), MobileV2TransportError> {
            let message = OpaqueFrame::parse_exact(&frame, MAX_FRAME)
                .and_then(|frame| frame.decode())
                .map_err(protocol_error)?;
            if let v2::Message::ClipboardCurrentQuery(query) = &message {
                let session = self.2.lock().unwrap().upgrade().unwrap();
                if session.transport.is_clock_query(peer, query.request_id().unwrap().0) {
                    session.receive_message(
                        peer,
                        v2::Message::ClipboardCurrentResult(super::super::tests::clock_result(peer, query, 0)),
                    )?;
                    return Ok(());
                }
            }
            self.0.lock().unwrap().push((peer.into(), message));
            Ok(())
        }
        fn send_low(&self, _: &str, operation: ClipboardLowOperation) -> Result<(), MobileV2TransportError> {
            self.1.lock().unwrap().push(operation);
            Ok(())
        }
    }
    fn session() -> (tempfile::TempDir, Arc<Link>, Arc<MobileV2Session<Link>>) {
        let d = tempfile::tempdir().unwrap();
        let store = TransferStore::open(d.path().join("db"), d.path().join("files")).unwrap();
        let link = Arc::new(Link::default());
        let s = Arc::new(MobileV2Session::new(
            store,
            MobileV2Identity::from_parts([1; 32], [2; 16], [3; 16]),
            link.clone(),
        ));
        *link.2.lock().unwrap() = Arc::downgrade(&s);
        (d, link, s)
    }
    fn peer(n: u8, relayed: bool) -> Peer {
        Peer {
            id: hex(&[n; 32]),
            kind: crate::PeerKind::Pc,
            state: crate::PeerState::Ready,
            accepts_clipboard: true,
            connection_order: n as u64,
            relayed,
            clipboard_pending: false,
        }
    }
    #[test]
    fn history_subscribes_to_one_direct_pc_only_while_foreground() {
        let (_d, link, s) = session();
        s.set_peers(vec![peer(4, true), peer(5, false), peer(6, false)]);
        assert!(link.0.lock().unwrap().is_empty());
        s.set_history_foreground(true).unwrap();
        let sent = link.0.lock().unwrap();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|(p, _)| *p == hex(&[5; 32])));
        assert!(matches!(sent[0].1, v2::Message::HistorySummary(_)));
        drop(sent);
        s.set_history_foreground(false).unwrap();
        let sent = link.0.lock().unwrap();
        assert!(matches!(&sent.last().unwrap().1,v2::Message::HistorySummary(s) if !s.subscribe().unwrap()));
    }
    #[tokio::test]
    async fn interrupted_body_page_finishes_the_waiter_and_retry_uses_the_same_boundary() {
        let (_d, link, session) = session();
        let s = session;
        s.set_peers(vec![peer(7, false)]);
        s.set_history_foreground(true).unwrap();
        let first = s.history.lock().unwrap().page_request.unwrap();
        let waiting = {
            let s = s.clone();
            tokio::spawn(async move { s.load_older_history().await })
        };
        while s.history.lock().unwrap().pending.is_none() {
            tokio::task::yield_now().await;
        }
        let mut pc = h::HistoryWindow::new();
        pc.merge(vec![crate::store::history::tests::record(1)], vec![]).unwrap();
        let response = v2::Message::HistoryRecords(
            dto::HistoryRecords::from_page(first, &pc.page(None, 20, true), pc.digest()).unwrap(),
        );
        s.fail_history_receive(&hex(&[7; 32]), &response, "interrupted".into());
        assert!(waiting.await.unwrap().is_err());
        assert!(s.history.lock().unwrap().page_request.is_none());
        let retry = {
            let s = s.clone();
            tokio::spawn(async move { s.load_older_history().await })
        };
        while s.history.lock().unwrap().page_request.is_none() {
            tokio::task::yield_now().await;
        }
        let request = s.history.lock().unwrap().page_request.unwrap();
        assert_ne!(request, first);
        {
            let sent = link.0.lock().unwrap();
            assert!(
                matches!(&sent.last().unwrap().1,v2::Message::HistoryPull(pull) if matches!(pull.query().unwrap(),dto::HistoryQuery::Page{before:None,max:20}))
            );
        }
        s.receive_message(
            &hex(&[7; 32]),
            v2::Message::HistoryRecords(
                dto::HistoryRecords::from_page(request, &pc.page(None, 20, true), pc.digest()).unwrap(),
            ),
        )
        .unwrap();
        retry.await.unwrap().unwrap();
        assert!(!s.history_has_more());
    }

    #[test]
    fn backgrounding_while_paused_defers_only_the_unsubscribe_until_resume() {
        let (_d, link, s) = session();
        s.set_peers(vec![peer(7, false)]);
        s.set_history_foreground(true).unwrap();
        link.0.lock().unwrap().clear();
        let mut paused = peer(7, false);
        paused.accepts_clipboard = false;
        s.set_peers(vec![paused]);
        s.set_history_foreground(false).unwrap();
        assert!(link.0.lock().unwrap().is_empty());
        s.set_peers(vec![peer(7, false)]);
        let sent = link.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0].1,v2::Message::HistorySummary(summary) if !summary.subscribe().unwrap()));
    }

    #[test]
    fn an_inline_phone_author_does_not_become_an_index_sync_peer() {
        let (_d, link, s) = session();
        let mut phone = peer(8, false);
        phone.kind = crate::PeerKind::Mobile;
        s.set_peers(vec![phone]);
        s.set_history_foreground(true).unwrap();
        let mut record = crate::store::history::tests::record(1);
        record.id.clip.origin = DeviceId([8; 32]);
        record.origin = record.id.clip.origin;
        let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Record(record), [0; 32]).unwrap();
        s.receive_message(&hex(&[8; 32]), v2::Message::HistoryDelta(delta))
            .unwrap();
        assert_eq!(s.history_snapshot().unwrap().index.len(), 1);
        assert!(
            link.0.lock().unwrap().is_empty(),
            "only the selected PC is queried for its index"
        );
    }

    #[test]
    fn background_deltas_do_not_enter_the_phone_store() {
        let (_d, _link, s) = session();
        s.set_peers(vec![peer(7, false)]);
        let record = crate::store::history::tests::record(1);
        let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Record(record), [0; 32]).unwrap();
        s.receive_message(&hex(&[7; 32]), v2::Message::HistoryDelta(delta))
            .unwrap();
        assert!(s.history_snapshot().unwrap().index.is_empty());
    }
    #[test]
    fn phone_broadcast_uses_low_for_inline_text_over_the_reliable_limit_and_keeps_canonical_reads() {
        let (_d, link, s) = session();
        s.set_peers(vec![peer(7, false)]);
        let text = "x".repeat(20_000);
        s.send_share(
            SharePayload {
                sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
                items: vec![crate::SendItem::text(text.clone(), Some("<b>hi</b>".into()), None)],
                origin: crate::SendOrigin::TransferPage,
                operation_id: [8; 16],
                message_id: [9; 16],
            },
            crate::ProxyChoice::Automatic,
        )
        .unwrap();
        assert_eq!(link.1.lock().unwrap().len(), 1);
        assert!(link.1.lock().unwrap()[0].is_history());
        let history = s.history_snapshot().unwrap();
        let record = &history.bodies.records()[0];
        assert_eq!(record.inline.len(), 2);
        assert_eq!(record.inline[0].bytes, text.as_bytes());
        assert!(history.bodies.holders().is_empty());
    }
    #[test]
    fn full_pc_index_and_first_body_page_have_equal_digests_without_loading_older_bodies() {
        let (_d, link, s) = session();
        s.set_peers(vec![peer(7, false)]);
        s.set_history_foreground(true).unwrap();
        let sent = link.0.lock().unwrap();
        let index_id = sent
            .iter()
            .find_map(|(_, m)| {
                if let v2::Message::HistoryIndexRequest(r) = m {
                    Some(r.request_id().unwrap().0)
                } else {
                    None
                }
            })
            .unwrap();
        let page_id = sent
            .iter()
            .find_map(|(_, m)| {
                if let v2::Message::HistoryPull(r) = m {
                    Some(r.request_id().unwrap().0)
                } else {
                    None
                }
            })
            .unwrap();
        drop(sent);
        let mut pc = h::HistoryWindow::new();
        pc.merge((1..=100).map(crate::store::history::tests::record), vec![])
            .unwrap();
        s.receive_message(
            &hex(&[7; 32]),
            v2::Message::HistoryIndex(dto::HistoryIndex::from_window(index_id, &pc).unwrap()),
        )
        .unwrap();
        s.receive_message(
            &hex(&[7; 32]),
            v2::Message::HistoryRecords(
                dto::HistoryRecords::from_page(page_id, &pc.page(None, 20, true), pc.digest()).unwrap(),
            ),
        )
        .unwrap();
        let history = s.history_snapshot().unwrap();
        assert_eq!(history.digest(), pc.digest());
        assert_eq!(history.bodies.records().len(), 20);
        assert_eq!(s.with_store(|store| store.page(None, 20)).unwrap().messages.len(), 20);
        assert!(s.history_has_more());
    }
    #[test]
    fn history_files_reuse_file_reads_and_phone_holders_are_ignored() {
        let (_d, link, s) = session();
        let mut phone = peer(8, false);
        phone.kind = crate::PeerKind::Mobile;
        s.set_peers(vec![peer(7, false), phone]);
        s.set_history_foreground(true).unwrap();
        let mut record = crate::store::history::tests::record(1);
        record.inline.clear();
        record.formats.clear();
        record.items[0].kind = h::ItemKind::Directory;
        record.items[0].name = Some("folder".into());
        record.items[0].mime = "inode/directory".into();
        record.items[0].file_node = Some(37);
        record.items[0].file_version = Some(11);
        let mut other_root = record.items[0].clone();
        other_root.id = 4_000_000_000;
        other_root.kind = h::ItemKind::File;
        other_root.name = Some("document.pdf".into());
        other_root.mime = "application/pdf".into();
        other_root.file_node = Some(u32::MAX as u64 + 42);
        other_root.file_version = Some(19);
        record.items.push(other_root);
        let mut pc = h::HistoryWindow::new();
        pc.merge(vec![record.clone()], vec![]).unwrap();
        let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Record(record.clone()), pc.digest()).unwrap();
        s.receive_message(&hex(&[7; 32]), v2::Message::HistoryDelta(delta))
            .unwrap();
        let records = s.with_store(|store| store.shared_records(record.id)).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].operation_id, records[0].operation_id);
        assert_eq!(records[1].item_id, 4_000_000_000);
        assert_eq!(
            parse_file_content(&records[1].content_id),
            Some((u32::MAX as u64 + 42, 19))
        );
        assert_eq!(records[0].kind, AttachmentKind::Folder);
        assert_eq!(parse_file_content(&records[0].content_id), Some((37, 11)));
        let holder = h::HolderEntry {
            record_id: record.id,
            holder: DeviceId([8; 32]),
            rev: 1,
            state: h::HolderState::Holding,
            scope: h::HolderScope::FILES,
        };
        let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Holder(holder.clone()), pc.digest()).unwrap();
        s.receive_message(&hex(&[7; 32]), v2::Message::HistoryDelta(delta))
            .unwrap();
        assert!(s.history_snapshot().unwrap().bodies.holders().is_empty());
        let holder = h::HolderEntry {
            holder: DeviceId([7; 32]),
            ..holder
        };
        pc.merge(vec![], vec![holder.clone()]).unwrap();
        let delta = dto::HistoryDelta::from_domain(&h::HistoryChange::Holder(holder), pc.digest()).unwrap();
        s.receive_message(&hex(&[7; 32]), v2::Message::HistoryDelta(delta))
            .unwrap();
        assert_eq!(
            s.with_store(|store| store.history_source(records[0].id)).unwrap(),
            Some(hex(&[7; 32]))
        );
        // Materializing, downloading and selecting a holder never creates a phone Holding.
        assert!(!link.0.lock().unwrap().iter().any(|(_,m)| matches!(m,v2::Message::HistoryDelta(d) if matches!(d.to_domain().unwrap(),h::HistoryChange::Holder(_)))));
    }

    #[test]
    fn older_clock_skewed_body_does_not_invalidate_the_native_page_cursor() {
        let (_d, _link, s) = session();
        let mut oldest = crate::store::history::tests::record(1);
        oldest.time_ms = 1000;
        let mut pc = h::HistoryWindow::new();
        pc.merge(
            vec![
                oldest,
                crate::store::history::tests::record(2),
                crate::store::history::tests::record(3),
            ],
            vec![],
        )
        .unwrap();
        let mut history = PhoneHistory::default();
        history.merge_index(&pc.index()).unwrap();
        history.merge_page(&pc.page(None, 2, true)).unwrap();
        s.save_and_materialize(history).unwrap();
        let first = s.with_store(|store| store.page(None, 1)).unwrap();
        assert_eq!(first.messages[0].created_ms, 3);
        let mut history = s.history_snapshot().unwrap();
        history
            .merge_page(&pc.page(pc.page(None, 2, true).next, 20, true))
            .unwrap();
        s.save_and_materialize(history).unwrap();
        let rest = s.with_store(|store| store.page(first.next, 20)).unwrap();
        assert_eq!(rest.messages.len(), 2);
        assert!(rest.messages.iter().all(|r| r.created_ms == 1000));
        assert!(rest.messages[0].history_order > rest.messages[1].history_order);
    }

    #[test]
    fn inline_sensitive_phone_sends_are_head_only_and_erase_own_rich_bodies() {
        for sensitivity in [
            oink_foundation::clipboard::Sensitivity::Concealed,
            oink_foundation::clipboard::Sensitivity::Transient,
        ] {
            let (_d, link, s) = session();
            let events = Arc::new(Mutex::new(Vec::new()));
            let received = events.clone();
            s.set_event_sink(Arc::new(move |event| received.lock().unwrap().push(event)));
            s.set_peers(vec![peer(7, false)]);
            let payload = |operation, sensitivity| SharePayload {
                sensitivity,
                items: vec![crate::SendItem::text(
                    "fixture".into(),
                    Some("<b>fixture</b>".into()),
                    None,
                )],
                origin: crate::SendOrigin::TransferPage,
                operation_id: [operation; 16],
                message_id: [operation; 16],
            };
            let ids = s
                .send_share(payload(1, sensitivity), crate::ProxyChoice::Automatic)
                .unwrap();
            let history = s.history_snapshot().unwrap();
            assert!(history.index[0].head_only);
            let record = &history.bodies.records()[0];
            assert_eq!(record.sensitivity, sensitivity);
            assert_eq!(record.inline.len(), 2);
            assert!(record.digests.is_empty());
            let own = s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap();
            assert!(own.sensitive);
            let child = s
                .with_store(|store| store.folder_children(own.id))
                .unwrap()
                .pop()
                .unwrap();
            let path = child.local_path.unwrap();
            assert!(std::path::Path::new(&path).is_file());
            s.send_share(
                payload(2, oink_foundation::clipboard::Sensitivity::Normal),
                crate::ProxyChoice::Automatic,
            )
            .unwrap();
            assert!(s.with_store(|store| store.by_id(own.id)).unwrap().is_none());
            assert!(!std::path::Path::new(&path).exists());
            assert!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event,TransferEvent::Removed{record_ids} if record_ids.contains(&own.id)))
            );
            assert!(!link.0.lock().unwrap().iter().any(|(_,message)|matches!(message,v2::Message::HistoryDelta(delta) if matches!(delta.to_domain().unwrap(),h::HistoryChange::Holder(_)))));
        }
    }
    #[test]
    fn large_sensitive_phone_text_keeps_classification_and_binds_its_proxy_head_before_purge() {
        let (_d, link, s) = session();
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        let pc = hex(&source.device.0);
        s.set_peers(vec![peer(7, false)]);
        let ids = s
            .send_share(
                SharePayload {
                    sensitivity: oink_foundation::clipboard::Sensitivity::Transient,
                    items: vec![crate::SendItem::Text {
                        text: "x".repeat(33 * 1024),
                    }],
                    origin: crate::SendOrigin::TransferPage,
                    operation_id: [8; 16],
                    message_id: [9; 16],
                },
                crate::ProxyChoice::Automatic,
            )
            .unwrap();
        assert!(s.history_snapshot().unwrap().index.is_empty());
        let sent = link.0.lock().unwrap();
        assert!(
            matches!(&sent[0].1,v2::Message::ClipboardShareBegin(begin) if begin.sensitivity().unwrap()==Some(dto::Sensitivity::Transient))
        );
        drop(sent);
        let mut raw = super::super::tests::text_offer(&source, 4, dto::Sensitivity::Transient).into_raw();
        raw.insert(8, device(s.device_id()).write());
        raw.insert(9, device(source.device).write());
        raw.insert(10, id16([8; 16]).write());
        let own_head = dto::ClipboardOffer::from_raw(raw).unwrap();
        s.receive_message(&pc, v2::Message::ClipboardOffer(own_head)).unwrap();
        assert!(s.with_store(|store| store.by_id(ids[0])).unwrap().unwrap().sensitive);
        s.receive_message(
            &pc,
            v2::Message::ClipboardOffer(super::super::tests::text_offer(&source, 5, dto::Sensitivity::Normal)),
        )
        .unwrap();
        assert!(s.with_store(|store| store.by_id(ids[0])).unwrap().is_none());
    }

    #[test]
    fn a_deleted_shared_record_stays_deleted_when_the_current_head_is_replayed() {
        let (_d, _link, s) = session();
        let source = MobileV2Identity::from_parts([7; 32], [2; 16], [3; 16]);
        s.set_peers(vec![peer(7, false)]);
        let mut record = crate::store::history::tests::record(1);
        record.id.generation = 1;
        let mut history = PhoneHistory::default();
        history
            .merge_page(&h::HistoryPage {
                records: vec![record.clone()],
                holders: vec![],
                more: false,
                next: None,
            })
            .unwrap();
        s.save_and_materialize(history).unwrap();
        let message = s
            .with_store(|store| store.page(None, 20))
            .unwrap()
            .messages
            .pop()
            .unwrap();
        s.with_store(|store| store.delete_message(message.id)).unwrap();
        let digest = s.history_snapshot().unwrap().digest();
        s.receive_message(
            &hex(&source.device.0),
            v2::Message::ClipboardOffer(super::super::tests::text_offer(&source, 1, dto::Sensitivity::Normal)),
        )
        .unwrap();
        assert!(s.with_store(|store| store.page(None, 20)).unwrap().messages.is_empty());
        let history = s.history_snapshot().unwrap();
        assert_eq!(history.digest(), digest);
        assert!(history.hidden.contains(&record.id));
        assert!(history.bodies.get(record.id).is_none());
    }

    #[test]
    fn an_inline_history_page_does_not_complete_a_current_download_in_flight() {
        let (_d, _link, s) = session();
        let record = crate::store::history::tests::record(1);
        let mut metadata = record.clone();
        metadata.inline.clear();
        s.materialize_history(&metadata).unwrap();
        let (id, attempt) = s.with_store(|store| {
            let id = store.shared_records(record.id).unwrap()[0].id;
            store.set_history_source(id, &hex(&record.origin.0)).unwrap();
            store.begin_download(id).unwrap();
            (id, store.attempt(id).unwrap())
        });
        s.apply_download_reply(
            [8; 16],
            id,
            attempt,
            DownloadReply::Chunk {
                offset: 0,
                bytes: b"te".to_vec(),
            },
        )
        .unwrap();
        s.materialize_history(&record).unwrap();
        let active = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
        assert_eq!(active.download, crate::DownloadState::Downloading);
        assert!(active.text.is_none());
        s.apply_download_reply(
            [8; 16],
            id,
            attempt,
            DownloadReply::Chunk {
                offset: 2,
                bytes: b"xt".to_vec(),
            },
        )
        .unwrap();
        assert!(
            s.apply_download_reply([8; 16], id, attempt, DownloadReply::End(4))
                .unwrap()
        );
        let complete = s.with_store(|store| store.by_id(id)).unwrap().unwrap();
        assert_eq!(complete.download, crate::DownloadState::Complete);
        assert_eq!(complete.text.as_deref(), Some("text"));
        assert!(s.with_store(|store| store.attempt(id)).is_none());
    }

    #[test]
    fn ready_inline_download_announces_after_releasing_the_store() {
        let (_d, _link, s) = session();
        let mut image = crate::store::history::tests::record(1);
        image.items[0].kind = h::ItemKind::Image;
        image.items[0].mime = "image/png".into();
        image.formats[0].portable = Some(h::PortableFormat::Png);
        image.formats[0].name = "mime:image/png".into();
        image.inline[0].format = h::PortableFormat::Png;
        image.inline[0].bytes = b"inline body".to_vec();
        let mut history = PhoneHistory::default();
        history
            .merge_page(&h::HistoryPage {
                records: vec![image.clone()],
                holders: vec![],
                more: false,
                next: None,
            })
            .unwrap();
        s.save_and_materialize(history).unwrap();
        let id = s.with_store(|store| store.shared_records(image.id)).unwrap()[0].id;
        let store = s.coordinator.store.clone();
        let announced = Arc::new(AtomicBool::new(false));
        let seen = announced.clone();
        s.set_event_sink(Arc::new(move |event| {
            if matches!(
                event,
                TransferEvent::Announcement {
                    auto_open_ready: true,
                    ..
                }
            ) {
                assert!(
                    store.try_lock().is_ok(),
                    "ready download retained its store guard during announcement"
                );
                seen.store(true, Ordering::SeqCst);
            }
        }));
        s.request_download(id, true).unwrap();
        assert!(announced.load(Ordering::SeqCst));
        assert!(s.take_ready_auto_open().unwrap().is_some());
    }

    #[test]
    fn a_fully_inline_bitmap_is_readable_without_a_holder_but_remains_window_cache() {
        let (_d, _link, s) = session();
        let mut image = crate::store::history::tests::record(1);
        image.items[0].kind = h::ItemKind::Image;
        image.items[0].mime = "image/png".into();
        image.formats[0].portable = Some(h::PortableFormat::Png);
        image.formats[0].name = "mime:image/png".into();
        image.inline[0].format = h::PortableFormat::Png;
        image.inline[0].bytes = b"opaque inline image bytes".to_vec();
        let mut history = PhoneHistory::default();
        history
            .merge_page(&h::HistoryPage {
                records: vec![image.clone()],
                holders: vec![],
                more: false,
                next: None,
            })
            .unwrap();
        s.save_and_materialize(history).unwrap();
        let message = s
            .with_store(|store| store.shared_records(image.id))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(message.download, crate::DownloadState::Complete);
        assert_eq!(message.image_preview(), Some(crate::ImagePreview::Original));
        let path = message.local_path.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), image.inline[0].bytes);
        let mut history = s.history_snapshot().unwrap();
        history
            .merge_index(
                &(2..=101)
                    .map(crate::store::history::tests::record)
                    .map(|record| h::IndexEntry {
                        id: record.id,
                        key: record.key(),
                        rev: record.rev,
                        holders: vec![],
                        head_only: false,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        s.save_and_materialize(history).unwrap();
        assert!(s.with_store(|store| store.by_id(message.id)).unwrap().is_none());
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn phone_text_allocates_a_persistent_identity_and_sends_no_holder() {
        let (_d, link, s) = session();
        s.set_peers(vec![peer(7, false)]);
        let payload = |n| SharePayload {
            sensitivity: oink_foundation::clipboard::Sensitivity::Normal,
            items: vec![crate::SendItem::Text { text: "hello".into() }],
            origin: crate::SendOrigin::TransferPage,
            operation_id: [n; 16],
            message_id: [n; 16],
        };
        s.send_share(payload(1), crate::ProxyChoice::Automatic).unwrap();
        let history = s.history_snapshot().unwrap();
        assert_eq!(history.index.len(), 1);
        assert_eq!(history.index[0].id.clip.origin, s.device_id());
        let sent = link.0.lock().unwrap();
        assert!(
            matches!(&sent[0].1,v2::Message::HistoryDelta(d) if matches!(d.to_domain().unwrap(),h::HistoryChange::Record(_)))
        );
        assert_eq!(sent.len(), 1);
        assert!(history.bodies.holders().is_empty());
        drop(sent);
        s.send_share(payload(2), crate::ProxyChoice::Automatic).unwrap();
        assert_eq!(s.history_snapshot().unwrap().sequence, 2);
    }
}
