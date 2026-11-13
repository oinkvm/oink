//! 「清理传输记录」 (Settings › 传输, user 2026-11-11): every finished record
//! and its files on this phone go; records still moving stay.

use super::{StoreError, TransferStore};
use crate::model::MessageId;
use rusqlite::params;
use std::{collections::HashSet, fs};

impl TransferStore {
    /// Deletes every finished conversation message with its folder children,
    /// thumbnails and downloaded files, in one transaction. A message still
    /// moving stays, and gets its record normally when it ends: an own share
    /// still sending, a download still running (of the message or of a file
    /// inside it), and the messages in `receiving` (phone-to-phone receives
    /// that have not ended, also between two of their items). Returns the
    /// deleted conversation messages.
    pub fn clear_history(&mut self, receiving: &HashSet<MessageId>) -> Result<Vec<MessageId>, StoreError> {
        let mut history = self.load_history()?;
        let tx = self.db.transaction()?;
        let mut ids: Vec<MessageId> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM transfer_message m WHERE parent_id IS NULL
                   AND delivery_state IS NOT 'sending' AND download_state <> 'downloading'
                   AND NOT EXISTS (SELECT 1 FROM transfer_message c
                                   WHERE c.parent_id=m.id AND c.download_state='downloading')",
            )?;
            stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?
        };
        ids.retain(|id| !receiving.contains(id));
        for id in &ids {
            let key = tx.query_row("SELECT record_key FROM shared_message WHERE message_id=?1", [id], |r| {
                r.get::<_, String>(0)
            });
            if let Ok(key) = key
                && let Some(entry) = history.index.iter().find(|e| super::history::record_key(e.id) == key)
            {
                history.hidden.insert(entry.id);
            }
        }
        tx.execute(
            "UPDATE shared_history SET body=?1 WHERE id=1",
            [serde_json::to_string(&history).map_err(|e| StoreError::History(e.to_string()))?],
        )?;
        // (record, content id, thumbnail file) of every deleted record,
        // folder children included; their files go after the commit.
        let mut deleted: Vec<(MessageId, String, Option<String>)> = Vec::new();
        {
            let mut records = tx.prepare(
                "SELECT m.id,m.content_id,t.path FROM transfer_message m
                 LEFT JOIN transfer_thumbnail t ON t.record_id=m.id WHERE m.id=?1 OR m.parent_id=?1",
            )?;
            let mut delete = tx.prepare("DELETE FROM transfer_message WHERE id=?1")?;
            for id in &ids {
                let rows = records.query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                for row in rows {
                    deleted.push(row?);
                }
                delete.execute(params![id])?;
            }
        }
        tx.commit()?;
        for (id, content_id, thumbnail) in deleted {
            self.attempts.remove(&id);
            self.thumbnail_attempts.remove(&id);
            self.remove_attachment_dir(&content_id);
            if let Some(path) = thumbnail {
                let _ = fs::remove_file(path);
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AttachmentKind, DeliveryState, IncomingAnnouncement, SourceAvailability, SourceState};
    use crate::store::{OutgoingItem, ReceivedThumbnail};
    use std::path::{Path, PathBuf};

    fn open() -> (tempfile::TempDir, TransferStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TransferStore::open(dir.path().join("history.sqlite"), dir.path().join("files")).unwrap();
        (dir, store)
    }

    fn incoming(id: &str, n: u8, kind: AttachmentKind) -> IncomingAnnouncement {
        IncomingAnnouncement {
            sender_id: "pc".into(),
            proxy_id: None,
            content_id: id.into(),
            kind,
            display_name: format!("{id}.bin"),
            mime: None,
            size: 4,
            text: (kind == AttachmentKind::Text).then(|| id.into()),
            rich_text_format: None,
            source: SourceState {
                source_id: "pc".into(),
                availability: SourceAvailability::Ready,
            },
            operation_id: [n; 16],
            message_id: [n + 1; 16],
            item_id: 1,
            representation_id: 1,
            sensitive: false,
        }
    }

    /// A received file whose download completed.
    fn downloaded(store: &mut TransferStore, id: &str, n: u8) -> (MessageId, PathBuf) {
        let record = store
            .insert_announcement(i64::from(n), incoming(id, n, AttachmentKind::File))
            .unwrap();
        let path = store.begin_download(record.id).unwrap();
        store.append_download(record.id, 0, b"body").unwrap();
        store.complete_download(record.id).unwrap();
        (record.id, path)
    }

    fn own(store: &mut TransferStore, id: &str, n: u8) -> (MessageId, PathBuf) {
        let dir = store.content_path(id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("own.bin");
        fs::write(&path, b"own").unwrap();
        let record = store
            .insert_outgoing(
                i64::from(n),
                vec![OutgoingItem {
                    sender_id: "self".into(),
                    content_id: id.into(),
                    kind: AttachmentKind::File,
                    display_name: "own.bin".into(),
                    mime: None,
                    size: 3,
                    text: None,
                    rich_text_format: None,
                    local_path: Some(path.clone()),
                    operation_id: [n; 16],
                    message_id: [n + 1; 16],
                    item_id: 1,
                    image_size: None,
                    rich: None,
                }],
            )
            .unwrap()
            .remove(0);
        (record.id, path)
    }

    fn ids(store: &TransferStore) -> Vec<MessageId> {
        let mut ids: Vec<_> = store.page(None, 200).unwrap().messages.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        ids
    }

    fn exists(path: &Path) -> bool {
        path.exists()
    }

    #[test]
    fn finished_records_and_their_files_go_and_moving_ones_stay() {
        let (_dir, mut store) = open();
        // Finished: a text, a download, a placeholder, a failed download,
        // a sent and a failed own share.
        let text = store
            .insert_announcement(1, incoming("text", 1, AttachmentKind::Text))
            .unwrap()
            .id;
        let (done, done_path) = downloaded(&mut store, "done", 2);
        let placeholder = store
            .insert_announcement(3, incoming("placeholder", 3, AttachmentKind::File))
            .unwrap()
            .id;
        let failed = store
            .insert_announcement(4, incoming("failed", 4, AttachmentKind::File))
            .unwrap()
            .id;
        store.begin_download(failed).unwrap();
        store.fail_download(failed).unwrap();
        let (sent, sent_path) = own(&mut store, "sent", 5);
        store.set_delivery(&[sent], DeliveryState::Sent, None).unwrap();
        let (own_failed, _) = own(&mut store, "own-failed", 6);
        store
            .set_delivery(
                &[own_failed],
                DeliveryState::Failed,
                Some((crate::DeliveryFailure::Interrupted, "x")),
            )
            .unwrap();
        // Moving: an own share still sending, a running download, and a
        // finished item of a phone-to-phone receive still running.
        let (sending, sending_path) = own(&mut store, "sending", 7);
        let downloading = store
            .insert_announcement(8, incoming("downloading", 8, AttachmentKind::File))
            .unwrap()
            .id;
        let downloading_path = store.begin_download(downloading).unwrap();
        let (received, received_path) = downloaded(&mut store, "received", 9);

        let mut removed = store.clear_history(&HashSet::from([received])).unwrap();
        removed.sort_unstable();
        assert_eq!(removed, vec![text, done, placeholder, failed, sent, own_failed]);
        assert_eq!(ids(&store), vec![sending, downloading, received]);
        assert!(!exists(&done_path) && !exists(&sent_path));
        assert!(exists(&sending_path) && exists(&downloading_path) && exists(&received_path));
        // The running download keeps its attempt and completes normally.
        assert!(store.attempt(downloading).is_some());
        store.append_download(downloading, 0, b"body").unwrap();
        store.complete_download(downloading).unwrap();
        // Once they ended, a second clear takes them too.
        store.set_delivery(&[sending], DeliveryState::Sent, None).unwrap();
        let mut removed = store.clear_history(&HashSet::new()).unwrap();
        removed.sort_unstable();
        assert_eq!(removed, vec![sending, downloading, received]);
        assert!(ids(&store).is_empty());
        assert_eq!(store.cached_bytes().unwrap(), 0);
    }

    #[test]
    fn a_folder_goes_with_its_children_and_thumbnails_unless_a_child_downloads() {
        let (_dir, mut store) = open();
        let folder = store
            .insert_announcement(1, incoming("folder", 1, AttachmentKind::Folder))
            .unwrap()
            .id;
        let mut child = incoming("folder-child", 2, AttachmentKind::File);
        child.item_id = 2;
        let child = store.insert_directory_file(2, folder, child).unwrap().id;
        let child_path = store.begin_download(child).unwrap();
        let mut image = incoming("image", 3, AttachmentKind::Image);
        image.mime = Some("image/png".into());
        image.size = 20 * 1024 * 1024;
        let image = store.insert_announcement(3, image).unwrap().id;
        let attempt = store.begin_thumbnail(image).unwrap();
        store
            .complete_thumbnail(
                image,
                attempt,
                ReceivedThumbnail {
                    mime: "image/jpeg",
                    width: 4,
                    height: 4,
                    bytes: b"jpeg",
                    source_bytes: None,
                },
            )
            .unwrap();
        let thumbnail = PathBuf::from(store.by_id(image).unwrap().unwrap().thumbnail_path.unwrap());

        // A file inside the folder still downloads: the folder stays whole.
        assert_eq!(store.clear_history(&HashSet::new()).unwrap(), vec![image]);
        assert!(!exists(&thumbnail));
        assert_eq!(ids(&store), vec![folder]);
        assert!(exists(&child_path));

        store.append_download(child, 0, b"body").unwrap();
        store.complete_download(child).unwrap();
        assert_eq!(store.clear_history(&HashSet::new()).unwrap(), vec![folder]);
        assert!(store.by_id(child).unwrap().is_none());
        assert!(!exists(&child_path));
    }
}
