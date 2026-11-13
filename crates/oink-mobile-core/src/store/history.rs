//! The full shared index is independent of materialized bodies and permanent local copies.
use super::*;
use oink_foundation::{GroupId, clipboard_history as h, content::OfferId};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct PhoneHistory {
    pub group: Option<GroupId>,
    pub index: Vec<h::IndexEntry>,
    pub bodies: h::HistoryWindow,
    pub hidden: HashSet<OfferId>,
    pub clock: u64,
    pub sequence: u64,
}
impl PhoneHistory {
    pub fn merge_index(&mut self, remote: &[h::IndexEntry]) -> Result<(), StoreError> {
        self.index = h::merge_indexes(&self.index, remote).map_err(history_error)?;
        self.clock = self
            .clock
            .max(self.index.iter().map(|e| e.key.clock).max().unwrap_or(0));
        let ids: HashSet<_> = self.index.iter().map(|e| e.id).collect();
        self.hidden.retain(|id| ids.contains(id));
        // Rebuild only the materialized subset. Unloaded records still contribute to the digest.
        let records: Vec<_> = self
            .bodies
            .records()
            .iter()
            .filter(|r| ids.contains(&r.id) && !self.hidden.contains(&r.id))
            .cloned()
            .collect();
        let holders = self.bodies.holders().to_vec();
        self.bodies = h::HistoryWindow::new();
        self.bodies.merge(records, holders).map_err(history_error)?;
        Ok(())
    }
    pub fn merge_page(&mut self, page: &h::HistoryPage) -> Result<(), StoreError> {
        self.bodies
            .merge(page.records.clone(), page.holders.clone())
            .map_err(history_error)?;
        self.merge_index(&self.bodies.index())
    }
    pub fn digest(&self) -> [u8; 32] {
        h::window_digest(&self.index)
    }
}
fn history_error(e: impl std::fmt::Display) -> StoreError {
    StoreError::History(e.to_string())
}
pub(crate) fn record_key(id: OfferId) -> String {
    format!(
        "v2:{}:{}:{}",
        id.clip.origin.0.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        id.clip.seq,
        id.generation
    )
}
impl TransferStore {
    pub(crate) fn observe_history_clock(&self, clock: u64) -> Result<(), StoreError> {
        let mut history = self.load_history()?;
        if history.clock >= clock {
            return Ok(());
        }
        history.clock = clock;
        // A clock probe is not a history merge or a retention pass.
        self.db.execute(
            "INSERT OR REPLACE INTO shared_history(id,body) VALUES(1,?1)",
            [serde_json::to_string(&history).map_err(history_error)?],
        )?;
        Ok(())
    }
    pub(crate) fn load_history(&self) -> Result<PhoneHistory, StoreError> {
        let json: Option<String> = self
            .db
            .query_row("SELECT body FROM shared_history WHERE id=1", [], |r| r.get(0))
            .optional()?;
        json.map(|s| serde_json::from_str(&s).map_err(history_error))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    pub(crate) fn save_history(&mut self, history: &PhoneHistory) -> Result<Vec<i64>, StoreError> {
        // Index/Low handlers may have taken a snapshot before a concurrent
        // publication or clock reply. Their window save must not rewind identity.
        let current = self.load_history()?;
        let mut durable = history.clone();
        durable.clock = durable.clock.max(current.clock);
        durable.sequence = durable.sequence.max(current.sequence);
        let json = serde_json::to_string(&durable).map_err(history_error)?;
        self.db
            .execute("INSERT OR REPLACE INTO shared_history(id,body) VALUES(1,?1)", [json])?;
        let keep: HashSet<_> = history.index.iter().map(|e| record_key(e.id)).collect();
        let stale = {
            let mut stmt = self.db.prepare("SELECT h.message_id,h.record_key FROM shared_message h JOIN transfer_message m ON m.id=h.message_id WHERE m.direct_peer IS NULL AND ((h.permanent=0 AND m.outgoing=0) OR m.sensitive=1)")?;
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut removed = Vec::new();
        for (id, key) in stale {
            if !keep.contains(&key) {
                self.delete_message(id)?;
                removed.push(id);
            }
        }
        Ok(removed)
    }
    pub(crate) fn link_shared(&self, id: i64, offer: OfferId) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT OR IGNORE INTO shared_message(message_id,record_key,permanent) VALUES(?1,?2,0)",
            params![id, record_key(offer)],
        )?;
        Ok(())
    }
    pub(crate) fn keep_download(&self, id: i64) -> Result<(), StoreError> {
        self.db.execute("UPDATE shared_message SET permanent=1 WHERE message_id=?1 OR message_id=(SELECT parent_id FROM transfer_message WHERE id=?1)", [id])?;
        Ok(())
    }
    pub(crate) fn hide_shared(&self, id: i64) -> Result<(), StoreError> {
        let key: Option<String> = self
            .db
            .query_row("SELECT record_key FROM shared_message WHERE message_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(key) = key {
            let mut history = self.load_history()?;
            if let Some(entry) = history.index.iter().find(|e| record_key(e.id) == key) {
                history.hidden.insert(entry.id);
            }
            // Keep the full index/digest, but a local delete also removes cached inline bodies.
            history.merge_index(&[])?;
            self.db.execute(
                "UPDATE shared_history SET body=?1 WHERE id=1",
                [serde_json::to_string(&history).map_err(history_error)?],
            )?;
        }
        Ok(())
    }
}
impl TransferStore {
    pub(crate) fn history_inline_png(&self, id: i64, bytes: &[u8]) -> Result<(), StoreError> {
        let record = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        if matches!(record.download, DownloadState::Complete | DownloadState::Downloading) {
            return Ok(());
        }
        let directory = self.content_dir(&record.content_id)?;
        fs::create_dir_all(&directory)?;
        let path = directory.join("clipboard.png");
        fs::write(&path, bytes)?;
        // A paged inline body is window cache, not a permanent user download.
        self.db.execute(
            "UPDATE transfer_message SET download_state='complete',local_path=?1,mime='image/png',size=?2 WHERE id=?3",
            params![path.to_string_lossy(), bytes.len() as u64, id],
        )?;
        Ok(())
    }
}
impl TransferStore {
    pub(crate) fn set_inline_preview(&mut self, id: i64, preview: &h::InlinePreview) -> Result<(), StoreError> {
        let dir = self.thumbnail_dir();
        fs::create_dir_all(&dir)?;
        // Native image caches key by path. A distinct inline path makes the later
        // 512px file replace the decoded 160px image on both platforms.
        let path = dir.join(format!("{id}-inline.jpg"));
        fs::write(&path, &preview.jpeg)?;
        // Ready still renders the inline image; a width <=160 requests its 512px replacement.
        self.set_thumbnail(id, ThumbnailState::Ready, Some((&path, preview.width, preview.height)))
    }
    pub(crate) fn history_source(&self, id: i64) -> Result<Option<String>, StoreError> {
        Ok(self
            .db
            .query_row("SELECT source FROM shared_source WHERE message_id=?1 OR message_id=(SELECT parent_id FROM transfer_message WHERE id=?1) ORDER BY message_id DESC LIMIT 1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten())
    }
    pub(crate) fn refresh_holders(
        &self,
        history: &PhoneHistory,
        peers: &[crate::Peer],
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let mut changed = Vec::new();
        let records = {
            let mut stmt = self.db.prepare("SELECT message_id,record_key FROM shared_message")?;
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (id, key) in records {
            let Some(record) = self.by_id(id)? else {
                continue;
            };
            let Some(body) = history.bodies.records().iter().find(|r| record_key(r.id) == key) else {
                continue;
            };
            let mut holders: Vec<_> = history
                .bodies
                .holders()
                .iter()
                .filter(|e| {
                    let required = if super::super::v2_session::parse_file_content(&record.content_id).is_some() {
                        h::HolderScope::FILES
                    } else {
                        match record.kind {
                            AttachmentKind::Text => {
                                match body
                                    .formats
                                    .iter()
                                    .find(|format| {
                                        format.item == record.item_id
                                            && format.representation == record.representation_id
                                    })
                                    .and_then(|format| format.portable)
                                {
                                    Some(h::PortableFormat::Url) => h::HolderScope::URL,
                                    Some(h::PortableFormat::Html) => h::HolderScope::HTML,
                                    Some(h::PortableFormat::Rtf) => h::HolderScope::RTF,
                                    _ => h::HolderScope::TEXT,
                                }
                            }
                            AttachmentKind::Image => h::HolderScope::PNG,
                            _ => h::HolderScope::FILES,
                        }
                    };
                    e.record_id == body.id && e.state == h::HolderState::Holding && e.scope.0 & required.0 != 0
                })
                .collect();
            holders.sort_by_key(|entry| {
                let peer = peers
                    .iter()
                    .find(|p| p.id == super::super::v2_session::hex(&entry.holder.0));
                (
                    peer.is_none_or(|p| p.relayed),
                    entry.holder != body.origin,
                    peer.map(|p| p.connection_order).unwrap_or(u64::MAX),
                )
            });
            let mut rejected = Vec::new();
            for holder in &holders {
                let name = super::super::v2_session::hex(&holder.holder.0);
                let rev: Option<u64> = self
                    .db
                    .query_row(
                        "SELECT rev FROM shared_unavailable WHERE message_id=?1 AND holder=?2",
                        params![id, name],
                        |r| r.get(0),
                    )
                    .optional()?;
                if rev == Some(holder.rev) {
                    rejected.push(holder.holder);
                }
            }
            let source = holders
                .iter()
                .filter(|h| !rejected.contains(&h.holder))
                .find_map(|entry| {
                    let name = super::super::v2_session::hex(&entry.holder.0);
                    peers
                        .iter()
                        .any(|p| {
                            p.id == name
                                && p.kind == crate::PeerKind::Pc
                                && p.state == crate::PeerState::Ready
                                && p.accepts_clipboard
                                && !p.clipboard_pending
                        })
                        .then_some(name)
                });
            let state = if source.is_some() {
                SourceAvailability::Ready
            } else if holders.is_empty() || holders.iter().all(|h| rejected.contains(&h.holder)) {
                SourceAvailability::NoSuchItem
            } else {
                SourceAvailability::SourceOffline
            };
            self.db.execute(
                "INSERT OR REPLACE INTO shared_source(message_id,source) VALUES(?1,?2)",
                params![id, source],
            )?;
            if record.source_availability != state {
                self.db.execute(
                    "UPDATE transfer_message SET source_availability=?1,source_online=?2 WHERE id=?3",
                    params![state.as_str(), source.is_some(), id],
                )?;
                if let Some(record) = self.by_id(id)? {
                    changed.push(record);
                }
            }
        }
        Ok(changed)
    }
}
impl TransferStore {
    pub(crate) fn history_item_metadata(&self, id: i64, time_ms: i64, item: &h::HistoryItem) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET image_width=?1,image_height=?2,item_count=?3,created_ms=?5 WHERE id=?4",
            params![
                item.dimensions.map(|(w, _)| w),
                item.dimensions.map(|(_, h)| h),
                item.child_count,
                id,
                time_ms
            ],
        )?;
        Ok(())
    }
}
impl TransferStore {
    pub(crate) fn reject_holder(&self, id: i64, source: &str, history: &PhoneHistory) -> Result<bool, StoreError> {
        let key:Option<String>=self.db.query_row("SELECT record_key FROM shared_message WHERE message_id=?1 OR message_id=(SELECT parent_id FROM transfer_message WHERE id=?1)",[id],|r|r.get(0)).optional()?;
        let Some(key) = key else {
            return Ok(false);
        };
        let Some(holder) = history
            .bodies
            .holders()
            .iter()
            .find(|h| record_key(h.record_id) == key && super::super::v2_session::hex(&h.holder.0) == source)
        else {
            return Ok(false);
        };
        // A reader can only suppress its own failed route. It never forges the holder's rev/Gone.
        self.db.execute(
            "INSERT OR REPLACE INTO shared_unavailable(message_id,holder,rev) VALUES(?1,?2,?3)",
            params![id, source, holder.rev],
        )?;
        Ok(true)
    }
    pub(crate) fn reset_history_thumbnail(&self, id: i64) -> Result<(), StoreError> {
        self.db
            .execute("UPDATE transfer_thumbnail SET state='failed' WHERE record_id=?1", [id])?;
        Ok(())
    }
}
impl TransferStore {
    pub(crate) fn initialize_history_group(&mut self, group: GroupId) -> Result<Vec<i64>, StoreError> {
        let mut history = self.load_history()?;
        if history.group == Some(group) {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        if history.group.is_some_and(|old| old != group) {
            removed = {
                let mut statement = self
                    .db
                    .prepare("SELECT id FROM transfer_message WHERE parent_id IS NULL")?;
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<Vec<i64>, _>>()?
            };
            for id in &removed {
                self.delete_message(*id)?;
            }
            history = PhoneHistory {
                clock: history.clock,
                sequence: history.sequence,
                ..PhoneHistory::default()
            };
        }
        history.group = Some(group);
        self.db.execute(
            "INSERT OR REPLACE INTO shared_history(id,body) VALUES(1,?1)",
            [serde_json::to_string(&history).map_err(history_error)?],
        )?;
        Ok(removed)
    }
}
impl TransferStore {
    pub(crate) fn set_history_source(&self, id: i64, source: &str) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT OR REPLACE INTO shared_source(message_id,source) VALUES(?1,?2)",
            params![id, source],
        )?;
        self.db.execute(
            "UPDATE transfer_message SET source_online=1,source_availability='ready' WHERE id=?1",
            [id],
        )?;
        Ok(())
    }
}
impl TransferStore {
    pub(crate) fn history_inline_rich(
        &mut self,
        parent: i64,
        mut incoming: IncomingAnnouncement,
        bytes: &[u8],
        format: RichTextFormat,
    ) -> Result<(), StoreError> {
        incoming.kind = AttachmentKind::RichText;
        incoming.rich_text_format = Some(format);
        incoming.text = None;
        let record = self.insert_rich_child(0, parent, incoming)?;
        if matches!(record.download, DownloadState::Complete | DownloadState::Downloading) {
            return Ok(());
        }
        let dir = self.content_dir(&record.content_id)?;
        fs::create_dir_all(&dir)?;
        let path = dir.join(match format {
            RichTextFormat::Html => "content.html",
            RichTextFormat::Rtf => "content.rtf",
        });
        fs::write(&path, bytes)?;
        self.db.execute(
            "UPDATE transfer_message SET download_state='complete',local_path=?1 WHERE id=?2",
            params![path.to_string_lossy(), record.id],
        )?;
        self.mark_rich(parent, Some(format))?;
        Ok(())
    }
}
impl TransferStore {
    pub fn history_boundary_ms(&self) -> Result<Option<i64>, StoreError> {
        let history = self.load_history()?;
        if history.index.len() != h::WINDOW_MAX || history.index.iter().any(|e| history.bodies.get(e.id).is_none()) {
            return Ok(None);
        }
        let times = history.bodies.display_times();
        Ok(history
            .index
            .last()
            .and_then(|e| times.iter().find(|(id, _)| *id == e.id).map(|(_, time)| *time)))
    }
}
pub(crate) fn order_key(key: h::OrderKey) -> String {
    format!(
        "{:016x}{}{:016x}{:016x}",
        key.clock,
        super::super::v2_session::hex(&key.origin.0),
        key.seq,
        key.generation
    )
}
impl TransferStore {
    pub(crate) fn set_history_order(&self, id: i64, time_ms: i64, key: h::OrderKey) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT OR REPLACE INTO shared_order(record_id,order_key) VALUES(?1,?2)",
            params![id, order_key(key)],
        )?;
        self.db.execute(
            "UPDATE transfer_message SET created_ms=?1 WHERE id=?2",
            params![time_ms, id],
        )?;
        Ok(())
    }
    pub(crate) fn order_own_history(&self, record: &h::ClipRecord) -> Result<(), StoreError> {
        let ids = {
            let mut stmt = self
                .db
                .prepare("SELECT message_id FROM shared_message WHERE record_key=?1")?;
            stmt.query_map([record_key(record.id)], |row| row.get(0))?
                .collect::<Result<Vec<i64>, _>>()?
        };
        for id in ids {
            self.set_history_order(id, record.time_ms, record.key())?;
        }
        Ok(())
    }
}
impl TransferStore {
    pub(crate) fn shared_records(&self, offer: OfferId) -> Result<Vec<MessageRecord>, StoreError> {
        let mut statement=self.db.prepare(&format!("SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE transfer_message.id IN (SELECT message_id FROM shared_message WHERE record_key=?1) ORDER BY item_id,id"))?;
        statement
            .query_map([record_key(offer)], row_to_record)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}
impl TransferStore {
    pub(crate) fn set_history_paging(&mut self, paging: bool) {
        self.history_paging = paging;
    }
}
impl TransferStore {
    /// Current-head updates also arrive on Android without a history subscription.
    /// Superseded secrets must be erased there as well, including permanent downloads.
    pub(crate) fn has_newer_shared_head(&self, key: h::OrderKey) -> Result<bool, StoreError> {
        if self.load_history()?.index.iter().any(|entry| entry.key > key) {
            return Ok(true);
        }
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM shared_order o JOIN transfer_message m ON m.id=o.record_id WHERE m.direct_peer IS NULL AND o.order_key > ?1)",
            [order_key(key)], |r| r.get(0),
        )?)
    }
    pub(crate) fn purge_sensitive_before(&mut self, key: h::OrderKey) -> Result<Vec<i64>, StoreError> {
        let key_text = order_key(key);
        let candidates = {
            let mut stmt=self.db.prepare("SELECT m.id,h.record_key FROM transfer_message m LEFT JOIN shared_message h ON h.message_id=m.id LEFT JOIN shared_order o ON o.record_id=m.id WHERE m.sensitive=1 AND m.parent_id IS NULL AND m.direct_peer IS NULL AND (m.delivery_state IS NOT 'sending' OR o.order_key IS NOT NULL) AND COALESCE(o.order_key,'') < ?1")?;
            stmt.query_map([key_text], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut history = self.load_history()?;
        let mut erased: HashSet<_> = history
            .bodies
            .records()
            .iter()
            .filter(|r| r.sensitive() && r.key() < key)
            .map(|r| record_key(r.id))
            .collect();
        erased.extend(candidates.iter().filter_map(|(_, key)| key.clone()));
        if erased.is_empty() && candidates.is_empty() {
            return Ok(Vec::new());
        }
        history.index.retain(|e| !erased.contains(&record_key(e.id)));
        history.hidden.retain(|id| !erased.contains(&record_key(*id)));
        let records: Vec<_> = history
            .bodies
            .records()
            .iter()
            .filter(|r| !erased.contains(&record_key(r.id)))
            .cloned()
            .collect();
        let holders = history.bodies.holders().to_vec();
        history.bodies = h::HistoryWindow::new();
        history.bodies.merge(records, holders).map_err(history_error)?;
        let mut removed = self.save_history(&history)?;
        for (id, _) in candidates {
            if self.by_id(id)?.is_some() {
                self.delete_message(id)?;
                removed.push(id);
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use oink_foundation::{DeviceId, clipboard::Sensitivity, content::ClipId};
    pub(crate) fn record(seq: u64) -> h::ClipRecord {
        h::ClipRecord {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([7; 32]),
                    seq,
                },
                generation: 0,
            },
            clock: seq,
            time_ms: seq as i64,
            origin: DeviceId([7; 32]),
            proxy: None,
            sensitivity: Sensitivity::Normal,
            items: vec![h::HistoryItem {
                id: 0,
                kind: h::ItemKind::Text,
                mime: "text/plain".into(),
                name: None,
                bytes: Some(4),
                dimensions: None,
                child_count: None,
                children_bytes: None,
                file_node: None,
                file_version: None,
            }],
            formats: vec![h::HistoryFormat {
                item: 0,
                representation: 0,
                name: "text/plain".into(),
                portable: Some(h::PortableFormat::Text),
                bytes: Some(4),
                file_node: None,
            }],
            digests: vec![],
            inline: vec![h::InlinePayload {
                item: 0,
                representation: 0,
                format: h::PortableFormat::Text,
                bytes: b"text".to_vec(),
            }],
            preview: None,
            rev: 0,
        }
    }
    fn incoming(r: &h::ClipRecord) -> IncomingAnnouncement {
        IncomingAnnouncement {
            sender_id: "source".into(),
            proxy_id: None,
            content_id: record_key(r.id),
            kind: AttachmentKind::Text,
            display_name: "text".into(),
            mime: Some("text/plain".into()),
            size: 4,
            text: Some("text".into()),
            rich_text_format: None,
            source: crate::SourceState {
                source_id: "source".into(),
                availability: SourceAvailability::Ready,
            },
            operation_id: [1; 16],
            message_id: [2; 16],
            item_id: 0,
            representation_id: 0,
            sensitive: false,
        }
    }
    fn open() -> (tempfile::TempDir, TransferStore) {
        let d = tempfile::tempdir().unwrap();
        let s = TransferStore::open(d.path().join("db"), d.path().join("files")).unwrap();
        (d, s)
    }
    #[test]
    fn stale_window_snapshot_cannot_rewind_observed_clock_or_own_sequence() {
        let (_dir, mut store) = open();
        let stale = PhoneHistory::default();
        let current = PhoneHistory {
            clock: 100,
            sequence: 9,
            ..PhoneHistory::default()
        };
        store.save_history(&current).unwrap();
        store.observe_history_clock(250).unwrap();
        store.save_history(&stale).unwrap();
        let restored = store.load_history().unwrap();
        assert_eq!((restored.clock, restored.sequence), (250, 9));
    }

    #[test]
    fn full_index_digest_equals_pc_with_only_twenty_bodies_and_survives_restart() {
        let (dir, mut store) = open();
        let mut pc = h::HistoryWindow::new();
        pc.merge((1..=100).map(record), vec![]).unwrap();
        let mut phone = PhoneHistory::default();
        phone.merge_index(&pc.index()).unwrap();
        phone.merge_page(&pc.page(None, 20, true)).unwrap();
        phone.clock = 200;
        phone.sequence = 8;
        assert_eq!(phone.bodies.records().len(), 20);
        assert_eq!(phone.digest(), pc.digest());
        store.save_history(&phone).unwrap();
        drop(store);
        let store = TransferStore::open(dir.path().join("db"), dir.path().join("files")).unwrap();
        let restored = store.load_history().unwrap();
        assert_eq!(restored.digest(), pc.digest());
        assert_eq!((restored.clock, restored.sequence), (200, 8));
    }
    #[test]
    fn equal_display_times_page_in_shared_lamport_order_even_when_records_arrive_out_of_order() {
        let (_dir, mut store) = open();
        for seq in [3, 10, 1, 7, 2, 9, 4, 8, 5, 6] {
            let record = record(seq);
            let message = store.insert_announcement(42, incoming(&record)).unwrap();
            store.set_history_order(message.id, 42, record.key()).unwrap();
        }
        let first = store.page(None, 4).unwrap();
        assert_eq!(
            first.messages.iter().map(|m| m.content_id.clone()).collect::<Vec<_>>(),
            [10, 9, 8, 7].map(|seq| record_key(record(seq).id))
        );
        let second = store.page(first.next, 4).unwrap();
        assert_eq!(
            second.messages.iter().map(|m| m.content_id.clone()).collect::<Vec<_>>(),
            [6, 5, 4, 3].map(|seq| record_key(record(seq).id))
        );
        let third = store.page(second.next, 4).unwrap();
        assert_eq!(
            third.messages.iter().map(|m| m.content_id.clone()).collect::<Vec<_>>(),
            [2, 1].map(|seq| record_key(record(seq).id))
        );
    }
    #[test]
    fn twenty_entry_pages_reach_the_window_end_without_repulling_loaded_records() {
        let mut pc = h::HistoryWindow::new();
        pc.merge((1..=100).map(record), vec![]).unwrap();
        let mut phone = PhoneHistory::default();
        phone.merge_index(&pc.index()).unwrap();
        let mut before = None;
        for n in 1..=5 {
            let page = pc.page(before, 20, true);
            assert_eq!(page.records.len(), 20);
            phone.merge_page(&page).unwrap();
            assert_eq!(phone.bodies.records().len(), n * 20);
            assert_eq!(phone.digest(), pc.digest());
            before = page.next;
            assert_eq!(page.more, n < 5);
        }
    }
    #[test]
    fn local_delete_is_hidden_until_the_id_leaves_the_window() {
        let (_dir, mut store) = open();
        let r = record(1);
        let mut state = PhoneHistory::default();
        state
            .merge_page(&h::HistoryPage {
                records: vec![r.clone()],
                holders: vec![],
                more: false,
                next: None,
            })
            .unwrap();
        store.save_history(&state).unwrap();
        let m = store.insert_announcement(1, incoming(&r)).unwrap();
        store.link_shared(m.id, r.id).unwrap();
        store.delete_message(m.id).unwrap();
        let mut state = store.load_history().unwrap();
        assert!(state.hidden.contains(&r.id));
        assert!(state.bodies.get(r.id).is_none());
        let digest = state.digest();
        state
            .merge_page(&h::HistoryPage {
                records: vec![r.clone()],
                holders: vec![],
                more: false,
                next: None,
            })
            .unwrap();
        assert!(state.hidden.contains(&r.id));
        assert_eq!(digest, state.digest());
        assert!(
            state.bodies.get(r.id).is_none(),
            "a replay must not refill a deleted body"
        );
        let mut pc = h::HistoryWindow::new();
        pc.merge((2..=101).map(record), vec![]).unwrap();
        state.merge_index(&pc.index()).unwrap();
        assert!(!state.hidden.contains(&r.id));
    }
    #[test]
    fn eviction_drops_remote_metadata_but_keeps_downloaded_and_own_records() {
        let (_dir, mut store) = open();
        let mut state = PhoneHistory::default();
        state
            .merge_page(&h::HistoryPage {
                records: (1..=3).map(record).collect(),
                holders: vec![],
                more: false,
                next: None,
            })
            .unwrap();
        store.save_history(&state).unwrap();
        let mut ids = Vec::new();
        for seq in 1..=3 {
            let r = record(seq);
            let m = store.insert_announcement(seq as i64, incoming(&r)).unwrap();
            store.link_shared(m.id, r.id).unwrap();
            ids.push(m.id);
        }
        let path = store.begin_download(ids[1]).unwrap();
        fs::write(path, b"downloaded").unwrap();
        store.complete_download(ids[1]).unwrap();
        store
            .db
            .execute("UPDATE transfer_message SET outgoing=1 WHERE id=?1", [ids[2]])
            .unwrap();
        let mut pc = h::HistoryWindow::new();
        pc.merge((4..=103).map(record), vec![]).unwrap();
        state.merge_index(&pc.index()).unwrap();
        let removed = store.save_history(&state).unwrap();
        assert_eq!(removed, vec![ids[0]]);
        assert!(store.by_id(ids[1]).unwrap().is_some());
        assert!(store.by_id(ids[2]).unwrap().is_some());
    }
    #[test]
    fn availability_uses_any_online_holder_and_reader_failures_never_forge_holder_revisions() {
        let (_dir, mut store) = open();
        let record = record(1);
        let mut history = PhoneHistory::default();
        let holder = |n, rev, state| h::HolderEntry {
            record_id: record.id,
            holder: DeviceId([n; 32]),
            rev,
            state,
            scope: h::HolderScope::TEXT,
        };
        history
            .merge_page(&h::HistoryPage {
                records: vec![record.clone()],
                holders: vec![
                    holder(7, 1, h::HolderState::Holding),
                    holder(8, 2, h::HolderState::Holding),
                ],
                more: false,
                next: None,
            })
            .unwrap();
        store.save_history(&history).unwrap();
        let message = store.insert_announcement(1, incoming(&record)).unwrap();
        store.link_shared(message.id, record.id).unwrap();
        let peer = crate::Peer {
            id: crate::v2_session::hex(&[8; 32]),
            kind: crate::PeerKind::Pc,
            state: crate::PeerState::Ready,
            accepts_clipboard: true,
            connection_order: 1,
            relayed: false,
            clipboard_pending: false,
        };
        store.refresh_holders(&history, std::slice::from_ref(&peer)).unwrap();
        assert_eq!(store.history_source(message.id).unwrap(), Some(peer.id.clone()));
        assert_eq!(
            store.by_id(message.id).unwrap().unwrap().source_availability,
            SourceAvailability::Ready
        );
        store.refresh_holders(&history, &[]).unwrap();
        assert_eq!(
            store.by_id(message.id).unwrap().unwrap().source_availability,
            SourceAvailability::SourceOffline
        );
        let digest = history.digest();
        store.reject_holder(message.id, &peer.id, &history).unwrap();
        assert_eq!(history.digest(), digest);
        assert_eq!(history.bodies.holders()[1].rev, 2);
        store.refresh_holders(&history, std::slice::from_ref(&peer)).unwrap();
        assert_eq!(
            store.by_id(message.id).unwrap().unwrap().source_availability,
            SourceAvailability::SourceOffline
        );
        history
            .merge_page(&h::HistoryPage {
                records: vec![],
                holders: vec![holder(7, 3, h::HolderState::Gone), holder(8, 3, h::HolderState::Gone)],
                more: false,
                next: None,
            })
            .unwrap();
        store.refresh_holders(&history, &[peer]).unwrap();
        assert_eq!(
            store.by_id(message.id).unwrap().unwrap().source_availability,
            SourceAvailability::NoSuchItem
        );
    }
    #[test]
    fn inline_images_and_rich_text_preserve_an_active_body_file() {
        let (_dir, mut store) = open();
        let parent = store.insert_announcement(1, incoming(&record(1))).unwrap();
        for format in [None, Some(RichTextFormat::Html), Some(RichTextFormat::Rtf)] {
            let mut item = incoming(&record(
                2 + format.map_or(0, |f| if f == RichTextFormat::Html { 1 } else { 2 }),
            ));
            item.text = None;
            item.kind = if format.is_some() {
                AttachmentKind::RichText
            } else {
                AttachmentKind::Image
            };
            item.rich_text_format = format;
            let message = if format.is_some() {
                store.insert_rich_child(1, parent.id, item.clone()).unwrap()
            } else {
                store.insert_announcement(1, item.clone()).unwrap()
            };
            let path = store.begin_download(message.id).unwrap();
            let attempt = store.attempt(message.id).unwrap();
            store.append_download(message.id, 0, b"partial").unwrap();
            match format {
                Some(format) => store.history_inline_rich(parent.id, item, b"inline", format).unwrap(),
                None => store.history_inline_png(message.id, b"inline").unwrap(),
            }
            let active = store.by_id(message.id).unwrap().unwrap();
            assert_eq!(active.download, DownloadState::Downloading);
            assert_eq!(active.local_path.as_deref(), path.to_str());
            assert_eq!(store.attempt(message.id), Some(attempt));
            assert_eq!(fs::read(&path).unwrap(), b"partial");
            store.append_download(message.id, 7, b" rest").unwrap();
            store.complete_download(message.id).unwrap();
            assert_eq!(fs::read(path).unwrap(), b"partial rest");
        }
    }

    #[test]
    fn inline_160_preview_stays_visible_until_the_512_preview_replaces_it() {
        let (_dir, mut store) = open();
        let mut image = incoming(&record(1));
        image.kind = AttachmentKind::Image;
        image.text = None;
        let record = store.insert_announcement(1, image).unwrap();
        let preview = h::encode_inline_preview(160, 160, vec![200; 160 * 160 * 4]).unwrap();
        store.set_inline_preview(record.id, &preview).unwrap();
        let small = store.by_id(record.id).unwrap().unwrap();
        assert_eq!(small.image_preview(), Some(crate::ImagePreview::Thumbnail));
        assert_eq!(small.download, DownloadState::Placeholder);
        let attempt = store.begin_thumbnail(record.id).unwrap();
        let loading = store.by_id(record.id).unwrap().unwrap();
        assert_eq!(loading.image_preview(), Some(crate::ImagePreview::Thumbnail));
        assert_eq!(loading.thumbnail_path, small.thumbnail_path);
        store
            .complete_thumbnail(
                record.id,
                attempt,
                super::super::ReceivedThumbnail {
                    mime: "image/jpeg",
                    width: 512,
                    height: 512,
                    bytes: b"preview body",
                    source_bytes: None,
                },
            )
            .unwrap();
        let large = store.by_id(record.id).unwrap().unwrap();
        assert_eq!(large.thumbnail_width, Some(512));
        assert_ne!(large.thumbnail_path, small.thumbnail_path);
        assert!(!Path::new(small.thumbnail_path.as_ref().unwrap()).exists());
        assert!(Path::new(large.thumbnail_path.as_ref().unwrap()).is_file());
        assert_eq!(large.download, DownloadState::Placeholder);
    }
    #[test]
    fn superseded_password_purges_permanent_bytes_children_and_index_only_identity() {
        for sensitivity in [Sensitivity::Concealed, Sensitivity::Transient] {
            let (_dir, mut store) = open();
            let mut secret = record(1);
            secret.sensitivity = sensitivity;
            let mut history = PhoneHistory::default();
            history
                .merge_page(&h::HistoryPage {
                    records: vec![secret.clone()],
                    holders: vec![],
                    more: false,
                    next: None,
                })
                .unwrap();
            store.save_history(&history).unwrap();
            let mut item = incoming(&secret);
            item.sensitive = true;
            let message = store.insert_announcement(1, item.clone()).unwrap();
            store.link_shared(message.id, secret.id).unwrap();
            store.set_history_order(message.id, 1, secret.key()).unwrap();
            let path = store.begin_download(message.id).unwrap();
            fs::write(&path, b"secret body").unwrap();
            store.complete_download(message.id).unwrap();
            item.content_id.push_str(":r:0:1");
            item.representation_id = 1;
            store
                .history_inline_rich(message.id, item, b"<b>secret</b>", RichTextFormat::Html)
                .unwrap();
            let child = store.folder_children(message.id).unwrap().pop().unwrap();
            let rich_path = child.local_path.unwrap();
            // Both exceptions to ordinary eviction (own and downloaded) must lose to head-only.
            store
                .db
                .execute("UPDATE transfer_message SET outgoing=1 WHERE id=?1", [message.id])
                .unwrap();
            let mut index_only = PhoneHistory::default();
            index_only.merge_index(&history.index).unwrap();
            let mut pc = h::HistoryWindow::new();
            pc.merge(vec![secret, record(2)], vec![]).unwrap();
            index_only.merge_index(&pc.index()).unwrap();
            assert_eq!(index_only.digest(), pc.digest());
            assert!(index_only.index.iter().all(|entry| !entry.head_only));
            history.merge_index(&pc.index()).unwrap();
            assert_eq!(store.save_history(&history).unwrap(), vec![message.id]);
            assert!(store.by_id(message.id).unwrap().is_none());
            assert!(store.by_id(child.id).unwrap().is_none());
            assert!(!path.exists());
            assert!(!std::path::Path::new(&rich_path).exists());
            let persisted = store.load_history().unwrap();
            assert!(!persisted.bodies.records().iter().any(|r| r.sensitive()));
            assert!(persisted.hidden.is_empty());
        }
    }

    #[test]
    fn holder_revision_changes_do_not_require_old_page_bodies() {
        let mut pc = h::HistoryWindow::new();
        pc.merge((1..=100).map(record), vec![]).unwrap();
        let mut phone = PhoneHistory::default();
        phone.merge_index(&pc.index()).unwrap();
        phone.merge_page(&pc.page(None, 20, true)).unwrap();
        let old = record(1);
        pc.merge(
            vec![],
            vec![h::HolderEntry {
                record_id: old.id,
                holder: DeviceId([8; 32]),
                rev: 15,
                state: h::HolderState::Holding,
                scope: h::HolderScope::TEXT,
            }],
        )
        .unwrap();
        phone.merge_index(&pc.index()).unwrap();
        assert!(phone.bodies.get(old.id).is_none());
        assert_eq!(phone.digest(), pc.digest());
    }
}
