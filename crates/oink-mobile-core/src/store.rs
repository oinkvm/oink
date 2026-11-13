use crate::model::{
    AttachmentKind, DeliveryFailure, DeliveryState, DownloadState, Id16, IncomingAnnouncement, MessageId,
    MessageRecord, Page, PageCursor, RichTextFormat, SourceAvailability, ThumbnailState,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::HashMap,
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

mod clear_history;
pub(crate) mod history;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("shared history: {0}")]
    History(String),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid content id")]
    InvalidContentId,
    #[error("message {0} not found")]
    MessageNotFound(MessageId),
    #[error("source is offline")]
    SourceOffline,
    #[error("source unavailable: {0:?}")]
    SourceUnavailable(SourceAvailability),
    #[error("download is already in progress")]
    DownloadInProgress,
    #[error("message {0} was sent by this device and has no remote source")]
    OwnMessage(MessageId),
    #[error("invalid page size")]
    InvalidPageSize,
}

/// Shares being prepared (attachment copies, folder archives), under the
/// attachment root. Content directories never start with a dot, so no
/// record maps here. It is not cached content: "clear cache" and its size
/// leave it alone, and a new session empties it.
const STAGING_DIR: &str = ".staging";

/// Column order shared by every SELECT and `row_to_record`.
const COLUMNS: &str = "id,created_ms,sender_id,proxy_id,content_id,kind,display_name,mime,size,text,\
rich_text_format,download_state,local_path,source_online,source_availability,operation_id,message_id,\
item_id,representation_id,parent_id,item_count,outgoing,delivery_state,delivery_failure,delivery_detail,sensitive,\
direct_peer,direct_peer_name,image_width,image_height";

/// Thumbnail columns appended to every record SELECT (see `RECORDS`).
const THUMBNAIL_COLUMNS: &str = "t.state,t.path,t.width,t.height,s.order_key";
/// Every record is read with its thumbnail row, if any.
const RECORDS: &str = "transfer_message LEFT JOIN transfer_thumbnail t ON t.record_id=transfer_message.id LEFT JOIN shared_order s ON s.record_id=transfer_message.id";
/// Thumbnail files live next to the attachment directories, below a name
/// no content id can take (content ids never contain a dot).
const THUMBNAIL_DIR: &str = ".thumbnails";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS transfer_message (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        created_ms INTEGER NOT NULL,
        sender_id TEXT NOT NULL,
        proxy_id TEXT,
        content_id TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL,
        display_name TEXT NOT NULL,
        mime TEXT,
        size INTEGER NOT NULL,
        text TEXT,
        rich_text_format TEXT,
        download_state TEXT NOT NULL,
        local_path TEXT,
        source_online INTEGER NOT NULL,
        source_availability TEXT NOT NULL,
        operation_id BLOB NOT NULL,
        message_id BLOB NOT NULL,
        item_id INTEGER NOT NULL,
        representation_id INTEGER NOT NULL,
        parent_id INTEGER REFERENCES transfer_message(id) ON DELETE CASCADE,
        item_count INTEGER,
        outgoing INTEGER NOT NULL,
        delivery_state TEXT,
        delivery_failure TEXT,
        delivery_detail TEXT,
        sensitive INTEGER NOT NULL,
        direct_peer TEXT,
        direct_peer_name TEXT,
        image_width INTEGER,
        image_height INTEGER
    );
    CREATE INDEX IF NOT EXISTS transfer_message_page
        ON transfer_message(created_ms DESC, id DESC) WHERE parent_id IS NULL;
    CREATE INDEX IF NOT EXISTS transfer_message_children ON transfer_message(parent_id);
    CREATE INDEX IF NOT EXISTS transfer_message_sender
        ON transfer_message(sender_id, created_ms DESC);
    CREATE INDEX IF NOT EXISTS transfer_message_outgoing
        ON transfer_message(operation_id) WHERE outgoing=1;
    CREATE TABLE IF NOT EXISTS transfer_thumbnail (
        record_id INTEGER PRIMARY KEY REFERENCES transfer_message(id) ON DELETE CASCADE,
        state TEXT NOT NULL,
        path TEXT,
        width INTEGER,
        height INTEGER
    );
    CREATE TABLE IF NOT EXISTS shared_order (record_id INTEGER PRIMARY KEY REFERENCES transfer_message(id) ON DELETE CASCADE,order_key TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS shared_unavailable (message_id INTEGER REFERENCES transfer_message(id) ON DELETE CASCADE, holder TEXT NOT NULL, rev INTEGER NOT NULL, PRIMARY KEY(message_id,holder));
    CREATE TABLE IF NOT EXISTS shared_source (message_id INTEGER PRIMARY KEY REFERENCES transfer_message(id) ON DELETE CASCADE, source TEXT);
    CREATE TABLE IF NOT EXISTS shared_history (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS shared_message (message_id INTEGER PRIMARY KEY REFERENCES transfer_message(id) ON DELETE CASCADE, record_key TEXT NOT NULL, permanent INTEGER NOT NULL);";

/// A preview received from the source PC (protocol-v2 `ContentThumbnail`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReceivedThumbnail<'a> {
    /// image/jpeg or image/png.
    pub mime: &'a str,
    pub width: u32,
    pub height: u32,
    pub bytes: &'a [u8],
    /// Encoded size of the original, which the offer may not have announced.
    pub source_bytes: Option<u64>,
}

/// A message this device sends, recorded before its first network request.
#[derive(Clone, Debug)]
pub(crate) struct OutgoingItem {
    pub sender_id: String,
    pub content_id: String,
    pub kind: AttachmentKind,
    pub display_name: String,
    pub mime: Option<String>,
    pub size: u64,
    pub text: Option<String>,
    pub rich_text_format: Option<RichTextFormat>,
    pub local_path: Option<PathBuf>,
    pub operation_id: Id16,
    pub message_id: Id16,
    pub item_id: u32,
    /// Displayed pixel size of an own image's original (`SendItem::Image`).
    pub image_size: Option<(u32, u32)>,
    /// The rich text kept next to an own text message (`SendItem::RichText`).
    pub rich: Option<OutgoingRich>,
}

/// The rich text of an own text message, already in its attachment
/// directory. It is recorded as the message's hidden child (see `rich_text`).
#[derive(Clone, Debug)]
pub(crate) struct OutgoingRich {
    pub content_id: String,
    pub format: RichTextFormat,
    pub path: PathBuf,
    pub size: u64,
}

/// Content id of the rich text child of own text message `content_id`.
pub(crate) fn rich_content_id(content_id: &str) -> String {
    format!("{content_id}-rich")
}

/// The rich text kept next to a text message (2026-11-12): its format and
/// the file holding it. A text message's rich text is a hidden child record
/// (kind `RichText`, `parent_id` = the message), downloaded like any body
/// and never shown in the page by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichText {
    pub format: RichTextFormat,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct TransferStore {
    db: Connection,
    files_root: PathBuf,
    attempts: HashMap<MessageId, u64>,
    next_attempt: u64,
    thumbnail_attempts: HashMap<MessageId, u64>,
    history_paging: bool,
}

/// SQLite is intentionally owned by this Rust store.  Native shells only see
/// records and fixed paths, so Android and iOS cannot diverge in pagination or
/// download state.
impl TransferStore {
    pub fn open(database: impl AsRef<Path>, files_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let mut db = Connection::open(database)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        let files_root = files_root.as_ref().to_path_buf();
        discard_unreadable_history(&mut db, &files_root)?;
        db.execute_batch(SCHEMA)?;
        fs::create_dir_all(&files_root)?;
        Ok(Self {
            db,
            files_root,
            attempts: HashMap::new(),
            next_attempt: 0,
            thumbnail_attempts: HashMap::new(),
            history_paging: false,
        })
    }

    pub(crate) fn staging_dir(&self) -> PathBuf {
        self.files_root.join(STAGING_DIR)
    }

    /// The attachment directory of `content_id` (not created).
    pub(crate) fn content_path(&self, content_id: &str) -> Result<PathBuf, StoreError> {
        self.content_dir(content_id)
    }

    /// A new session owns the store (the session lease guarantees
    /// exclusivity), so nothing of the previous process is still running:
    /// its unfinished sends and downloads failed, no source is online until
    /// its link is ready again (`set_peer_online`), and staging files and
    /// attachment directories no record owns are removed.
    pub(crate) fn take_over(&self) -> Result<(), StoreError> {
        self.fail_interrupted_sends()?;
        // A phone-to-phone receive cannot be fetched again: its sender
        // streamed it once and keeps nothing to serve.
        self.db.execute(
            "UPDATE transfer_message SET source_online=0,source_availability=?1,
             delivery_failure=?2,delivery_detail='the app session ended before the transfer finished'
             WHERE outgoing=0 AND download_state='downloading'
               AND direct_peer IS NOT NULL",
            params![
                SourceAvailability::NoSuchItem.as_str(),
                DeliveryFailure::Interrupted.as_str()
            ],
        )?;
        self.db.execute(
            "UPDATE transfer_message SET download_state='failed' WHERE download_state='downloading'",
            [],
        )?;
        self.db.execute(
            "UPDATE transfer_message SET source_online=0,source_availability=?1
             WHERE outgoing=0 AND source_availability=?2",
            params![
                SourceAvailability::SourceOffline.as_str(),
                SourceAvailability::Ready.as_str()
            ],
        )?;
        let staging = self.staging_dir();
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        self.remove_unowned()
    }

    /// Removes everything under the attachment root that no record owns:
    /// directories of deleted or never-recorded messages and loose files
    /// (folder archives an older build staged directly in the root).
    fn remove_unowned(&self) -> Result<(), StoreError> {
        let owned: std::collections::HashSet<std::ffi::OsString> = {
            let mut stmt = self.db.prepare("SELECT content_id FROM transfer_message")?;
            let ids: Vec<String> = stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
            ids.iter()
                .filter_map(|id| self.content_dir(id).ok()?.file_name().map(ToOwned::to_owned))
                .collect()
        };
        for entry in fs::read_dir(&self.files_root)? {
            let entry = entry?;
            let name = entry.file_name();
            // Thumbnails are history like the records; `delete_message`
            // removes a record's own.
            if name == STAGING_DIR || name == THUMBNAIL_DIR || owned.contains(&name) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    pub fn insert_announcement(
        &mut self,
        now_ms: i64,
        item: IncomingAnnouncement,
    ) -> Result<MessageRecord, StoreError> {
        insert_incoming(&self.db, now_ms, item, None)
    }

    /// Records a file inside folder message `parent`. The child shares the
    /// download machinery of top-level messages but never enters the page.
    pub(crate) fn insert_directory_file(
        &mut self,
        now_ms: i64,
        parent: MessageId,
        item: IncomingAnnouncement,
    ) -> Result<MessageRecord, StoreError> {
        insert_incoming(&self.db, now_ms, item, Some(parent))
    }

    pub fn folder_children(&self, parent: MessageId) -> Result<Vec<MessageRecord>, StoreError> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE parent_id=?1 ORDER BY id"
        ))?;
        stmt.query_map(params![parent], row_to_record)?
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Records the rich text announced with received text message `parent`
    /// as its hidden child (see `RichText`); it is downloaded like a body.
    pub(crate) fn insert_rich_child(
        &mut self,
        now_ms: i64,
        parent: MessageId,
        item: IncomingAnnouncement,
    ) -> Result<MessageRecord, StoreError> {
        self.mark_rich(parent, item.rich_text_format)?;
        insert_incoming(&self.db, now_ms, item, Some(parent))
    }

    /// Records rich text that arrived with its message (a phone's broadcast
    /// text) as the complete hidden child of `parent`.
    pub(crate) fn insert_rich_body(
        &mut self,
        now_ms: i64,
        parent: MessageId,
        item: IncomingAnnouncement,
        body: &[u8],
    ) -> Result<MessageRecord, StoreError> {
        self.mark_rich(parent, item.rich_text_format)?;
        let record = insert_incoming(&self.db, now_ms, item, Some(parent))?;
        if record.download != DownloadState::Complete {
            let path = self.begin_download(record.id)?;
            if let Err(error) = fs::write(&path, body) {
                self.fail_download(record.id)?;
                return Err(error.into());
            }
            self.complete_download(record.id)?;
        }
        self.by_id(record.id)?.ok_or(StoreError::MessageNotFound(record.id))
    }

    /// A text message's `rich_text_format` names the format of its kept rich
    /// text (`MessageRecord::rich_text_format`).
    fn mark_rich(&self, parent: MessageId, format: Option<RichTextFormat>) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET rich_text_format=?1 WHERE id=?2",
            params![format.unwrap_or(RichTextFormat::Html).as_str(), parent],
        )?;
        Ok(())
    }

    /// The rich-text child of text message `parent`, if it has one.
    pub fn rich_child(&self, parent: MessageId) -> Result<Option<MessageRecord>, StoreError> {
        self.db
            .query_row(
                &format!(
                    "SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE parent_id=?1 AND kind=?2 ORDER BY id LIMIT 1"
                ),
                params![parent, AttachmentKind::RichText.as_str()],
                row_to_record,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The rich text of text message `parent` once it is on this phone.
    pub fn rich_text(&self, parent: MessageId) -> Result<Option<RichText>, StoreError> {
        Ok(self.rich_child(parent)?.and_then(|child| {
            let path = PathBuf::from(child.local_path?);
            (child.download == DownloadState::Complete && path.is_file()).then(|| RichText {
                format: child.rich_text_format.unwrap_or(RichTextFormat::Html),
                path,
            })
        }))
    }

    pub(crate) fn remove_attachment_dir(&self, content_id: &str) {
        if let Ok(dir) = self.content_dir(content_id) {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_outgoing(
        &mut self,
        now_ms: i64,
        items: Vec<OutgoingItem>,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        self.insert_outgoing_sensitive(now_ms, items, false)
    }
    pub(crate) fn insert_outgoing_sensitive(
        &mut self,
        now_ms: i64,
        items: Vec<OutgoingItem>,
        sensitive: bool,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        let tx = self.db.transaction()?;
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            let content_id = validate_content_id(item.content_id)?;
            tx.execute(
                &format!(
                    "INSERT INTO transfer_message({COLUMNS})
                     VALUES(NULL,?1,?2,NULL,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12,?13,?14,?15,0,NULL,NULL,1,?16,NULL,NULL,0,NULL,NULL,?17,?18)"
                ),
                params![
                    now_ms,
                    item.sender_id,
                    content_id,
                    item.kind.as_str(),
                    item.display_name,
                    item.mime,
                    i64::try_from(item.size).unwrap_or(i64::MAX),
                    item.text,
                    item.rich_text_format.map(|f| f.as_str()),
                    DownloadState::Complete.as_str(),
                    item.local_path
                        .as_deref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    SourceAvailability::Ready.as_str(),
                    item.operation_id.to_vec(),
                    item.message_id.to_vec(),
                    item.item_id,
                    DeliveryState::Sending.as_str(),
                    item.image_size.map(|(width, _)| width),
                    item.image_size.map(|(_, height)| height),
                ],
            )?;
            let parent = tx.last_insert_rowid();
            ids.push(parent);
            if let Some(rich) = item.rich {
                // Its hidden child: complete, no delivery of its own.
                let content_id = validate_content_id(rich.content_id)?;
                tx.execute(
                    &format!(
                        "INSERT INTO transfer_message({COLUMNS})
                         VALUES(NULL,?1,?2,NULL,?3,?4,'rich text',?5,?6,NULL,?7,?8,?9,1,?10,?11,?12,?13,1,?14,NULL,1,NULL,NULL,NULL,0,NULL,NULL,NULL,NULL)"
                    ),
                    params![
                        now_ms,
                        item.sender_id,
                        content_id,
                        AttachmentKind::RichText.as_str(),
                        rich_mime(rich.format),
                        i64::try_from(rich.size).unwrap_or(i64::MAX),
                        rich.format.as_str(),
                        DownloadState::Complete.as_str(),
                        rich.path.to_string_lossy().into_owned(),
                        SourceAvailability::Ready.as_str(),
                        item.operation_id.to_vec(),
                        item.message_id.to_vec(),
                        item.item_id,
                        parent,
                    ],
                )?;
            }
        }
        // Commit the source classification with both parent and rich-text child.
        // A restart must never restore a password body as an ordinary permanent send.
        if sensitive {
            for id in &ids {
                tx.execute(
                    "UPDATE transfer_message SET sensitive=1 WHERE id=?1 OR parent_id=?1",
                    [id],
                )?;
            }
        }
        let records = ids
            .into_iter()
            .map(|id| find_by_id(&tx, id)?.ok_or(StoreError::MessageNotFound(id)))
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit()?;
        Ok(records)
    }

    pub(crate) fn outgoing_records(&self, operation_id: Id16) -> Result<Vec<MessageRecord>, StoreError> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE outgoing=1 AND operation_id=?1 AND parent_id IS NULL ORDER BY id"
        ))?;
        stmt.query_map(params![operation_id.to_vec()], row_to_record)?
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    pub(crate) fn set_delivery(
        &self,
        ids: &[MessageId],
        state: DeliveryState,
        failure: Option<(DeliveryFailure, &str)>,
    ) -> Result<(), StoreError> {
        for id in ids {
            self.db.execute(
                "UPDATE transfer_message SET delivery_state=?1,delivery_failure=?2,delivery_detail=?3
                 WHERE id=?4 AND outgoing=1",
                params![
                    state.as_str(),
                    failure.map(|(reason, _)| reason.as_str()),
                    failure.map(|(_, detail)| detail),
                    id
                ],
            )?;
        }
        Ok(())
    }

    pub(crate) fn set_outgoing_proxy(&self, ids: &[MessageId], proxy: &str) -> Result<(), StoreError> {
        for id in ids {
            self.db.execute(
                "UPDATE transfer_message SET proxy_id=?1 WHERE id=?2 AND outgoing=1",
                params![proxy, id],
            )?;
        }
        Ok(())
    }

    /// Marks records as one phone-to-phone transfer with `peer` (the target
    /// of own records, the sender of received ones).
    pub(crate) fn set_direct(&self, ids: &[MessageId], peer: &str, peer_name: Option<&str>) -> Result<(), StoreError> {
        for id in ids {
            self.db.execute(
                "UPDATE transfer_message SET direct_peer=?1,direct_peer_name=?2 WHERE id=?3",
                params![peer, peer_name, id],
            )?;
        }
        Ok(())
    }

    /// The received records of one phone-to-phone transfer from `peer`.
    pub(crate) fn direct_received(&self, peer: &str, operation_id: Id16) -> Result<Vec<MessageRecord>, StoreError> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS}
             WHERE outgoing=0 AND direct_peer=?1 AND operation_id=?2 ORDER BY id"
        ))?;
        stmt.query_map(params![peer, operation_id.to_vec()], row_to_record)?
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// A received folder is unpacked: the record points at the directory.
    pub(crate) fn complete_folder(&mut self, id: MessageId, directory: &Path, files: u32) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET download_state='complete',local_path=?1,item_count=?2,mime=NULL WHERE id=?3",
            params![directory.to_string_lossy(), files, id],
        )?;
        self.attempts.remove(&id);
        Ok(())
    }

    /// A phone-to-phone receive ended without its content. It cannot be
    /// downloaded again (the sender keeps nothing to serve), so the source
    /// is gone for good; `failure` says why it ended.
    pub(crate) fn fail_direct(
        &mut self,
        id: MessageId,
        failure: DeliveryFailure,
        detail: &str,
    ) -> Result<(), StoreError> {
        let record = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        if let Some(path) = record.local_path.filter(|_| record.download != DownloadState::Complete) {
            let _ = fs::remove_file(path);
        }
        self.db.execute(
            "UPDATE transfer_message SET download_state='failed',local_path=NULL,source_online=0,
             source_availability=?1,delivery_failure=?2,delivery_detail=?3 WHERE id=?4",
            params![SourceAvailability::NoSuchItem.as_str(), failure.as_str(), detail, id],
        )?;
        self.attempts.remove(&id);
        Ok(())
    }

    /// A record whose body is already on this phone (a file of a folder
    /// received phone to phone).
    pub(crate) fn set_local_body(&self, id: MessageId, path: &Path) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET download_state='complete',local_path=?1 WHERE id=?2",
            params![path.to_string_lossy(), id],
        )?;
        Ok(())
    }

    /// The original's displayed size of an image received phone to phone,
    /// as its sender announced it (`ContentOffer` keys 15/16).
    pub(crate) fn set_image_size(&self, id: MessageId, (width, height): (u32, u32)) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET image_width=?1,image_height=?2 WHERE id=?3",
            params![width, height, id],
        )?;
        Ok(())
    }

    pub(crate) fn set_item_count(&self, id: MessageId, count: u32) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET item_count=?1 WHERE id=?2",
            params![count, id],
        )?;
        Ok(())
    }

    /// A share cannot outlive the session that sent it: its Low stream and
    /// pending result died with the previous process. Called once when a new
    /// session owns the store (the session lease guarantees exclusivity).
    pub(crate) fn fail_interrupted_sends(&self) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET delivery_state=?1,delivery_failure=?2,
             delivery_detail='the app session ended before the share finished'
             WHERE outgoing=1 AND delivery_state=?3",
            params![
                DeliveryState::Failed.as_str(),
                DeliveryFailure::Interrupted.as_str(),
                DeliveryState::Sending.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn page(&self, cursor: Option<PageCursor>, limit: usize) -> Result<Page, StoreError> {
        if !(1..=200).contains(&limit) {
            return Err(StoreError::InvalidPageSize);
        }
        let history = self.load_history()?;
        let mut floor = None;
        let times = history.bodies.display_times();
        for entry in &history.index {
            if history.bodies.get(entry.id).is_none() {
                floor = Some(floor.unwrap_or(i64::MAX));
                break;
            }
            floor = times.iter().find(|(id, _)| *id == entry.id).map(|(_, time)| *time);
        }
        if !self.history_paging || history.index.iter().all(|e| history.bodies.get(e.id).is_some()) {
            floor = None;
        }
        let floor_key = history
            .index
            .iter()
            .take_while(|e| history.bodies.get(e.id).is_some())
            .last()
            .map(|e| history::order_key(e.key));
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE parent_id IS NULL
             AND (?4 IS NULL OR created_ms > ?4 OR (created_ms=?4 AND COALESCE(s.order_key,'') >= COALESCE(?5,'')))
             AND (?1 IS NULL OR created_ms < ?1 OR (created_ms = ?1 AND (COALESCE(s.order_key,'') < COALESCE((SELECT order_key FROM shared_order WHERE record_id=?2),'')
                 OR (COALESCE(s.order_key,'') = COALESCE((SELECT order_key FROM shared_order WHERE record_id=?2),'') AND id < ?2))))
             ORDER BY created_ms DESC,COALESCE(s.order_key,'') DESC,id DESC LIMIT ?3"
        ))?;
        // Loading an older body may clamp already displayed shared timestamps.
        // Resolve the cursor row again so the old native timestamp cannot skip that page.
        let cursor_time = cursor
            .as_ref()
            .map(|c| {
                self.db
                    .query_row("SELECT created_ms FROM transfer_message WHERE id=?1", [c.id], |r| {
                        r.get::<_, i64>(0)
                    })
                    .optional()
                    .map(|time| time.unwrap_or(c.created_ms))
            })
            .transpose()?;
        let mut rows = stmt.query(params![
            cursor_time,
            cursor.as_ref().map(|c| c.id),
            limit as i64,
            floor,
            floor_key
        ])?;
        let mut messages = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            messages.push(row_to_record(row)?);
        }
        let next = messages.last().map(|m| PageCursor {
            created_ms: m.created_ms,
            id: m.id,
        });
        Ok(Page { messages, next })
    }

    /// Opens the fixed attachment path for one download attempt. The path
    /// keeps the content's real file name and extension so hosts can hand
    /// it to QuickLook / FileProvider / MediaStore directly.
    pub(crate) fn begin_download(&mut self, id: MessageId) -> Result<PathBuf, StoreError> {
        let rec = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        if rec.outgoing {
            return Err(StoreError::OwnMessage(id));
        }
        if rec.source_availability != SourceAvailability::Ready {
            return Err(if rec.source_availability == SourceAvailability::SourceOffline {
                StoreError::SourceOffline
            } else {
                StoreError::SourceUnavailable(rec.source_availability)
            });
        }
        if self.attempts.contains_key(&id) {
            return Err(StoreError::DownloadInProgress);
        }
        let dir = self.content_dir(&rec.content_id)?;
        fs::create_dir_all(&dir)?;
        let path = dir.join(stored_file_name(
            rec.kind,
            &rec.display_name,
            rec.mime.as_deref(),
            rec.rich_text_format,
        ));
        fs::File::create(&path)?;
        self.db.execute(
            "UPDATE transfer_message SET download_state='downloading',local_path=?1 WHERE id=?2",
            params![path.to_string_lossy(), id],
        )?;
        self.next_attempt = self.next_attempt.wrapping_add(1);
        self.attempts.insert(id, self.next_attempt);
        Ok(path)
    }

    pub(crate) fn set_peer_online(&self, peer: &str, online: bool) -> Result<(), StoreError> {
        let (before, after) = if online {
            (SourceAvailability::SourceOffline, SourceAvailability::Ready)
        } else {
            (SourceAvailability::Ready, SourceAvailability::SourceOffline)
        };
        self.db.execute(
            "UPDATE transfer_message SET source_online=?1,source_availability=?2
             WHERE outgoing=0 AND COALESCE(proxy_id,sender_id)=?3 AND source_availability=?4",
            params![online, after.as_str(), peer, before.as_str()],
        )?;
        Ok(())
    }

    pub(crate) fn attempt(&self, id: MessageId) -> Option<u64> {
        self.attempts.get(&id).copied()
    }

    pub(crate) fn complete_download(&mut self, id: MessageId) -> Result<(), StoreError> {
        self.keep_download(id)?;
        if self.by_id(id)?.is_none() {
            return Err(StoreError::MessageNotFound(id));
        }
        self.db.execute(
            "UPDATE transfer_message SET download_state='complete' WHERE id=?1",
            params![id],
        )?;
        self.attempts.remove(&id);
        Ok(())
    }

    pub(crate) fn complete_text(&mut self, id: MessageId, text: &str) -> Result<(), StoreError> {
        self.keep_download(id)?;
        let record = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        if let Some(path) = record.local_path {
            fs::remove_file(path)?;
        }
        self.db.execute(
            "UPDATE transfer_message SET download_state='complete',text=?1,local_path=NULL WHERE id=?2",
            params![text, id],
        )?;
        self.attempts.remove(&id);
        Ok(())
    }

    pub(crate) fn by_wire_content(
        &self,
        key: &str,
        legacy_key: &str,
        item: u32,
        representation: u32,
    ) -> Result<Option<MessageRecord>, StoreError> {
        if let Some(record) = self.by_content_id(key)? {
            return Ok(Some(record));
        }
        Ok(self
            .by_content_id(legacy_key)?
            .filter(|record| record.item_id == item && record.representation_id == representation))
    }

    /// Append one contiguous v2 content chunk to the fixed path allocated by
    /// `begin_download`. The offset check makes retries and reordered chunks
    /// fail closed instead of corrupting a completed transfer.
    pub(crate) fn append_download(&mut self, id: MessageId, offset: u64, bytes: &[u8]) -> Result<(), StoreError> {
        let record = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        let path = record.local_path.ok_or(StoreError::MessageNotFound(id))?;
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        let current = file.seek(SeekFrom::End(0))?;
        if current != offset {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "non-contiguous content chunk",
            )));
        }
        file.write_all(bytes)?;
        Ok(())
    }

    pub(crate) fn fail_download(&mut self, id: MessageId) -> Result<(), StoreError> {
        if self.by_id(id)?.is_none() {
            return Err(StoreError::MessageNotFound(id));
        }
        self.db.execute(
            "UPDATE transfer_message SET download_state='failed' WHERE id=?1",
            params![id],
        )?;
        self.attempts.remove(&id);
        Ok(())
    }

    /// The source answered that this content no longer exists or was
    /// replaced: the record keeps that reason (not retryable, unlike a
    /// failed download) until the content is announced again.
    pub(crate) fn mark_unavailable(&mut self, id: MessageId, reason: SourceAvailability) -> Result<(), StoreError> {
        if self.by_id(id)?.is_none() {
            return Err(StoreError::MessageNotFound(id));
        }
        self.db.execute(
            "UPDATE transfer_message SET download_state='failed',source_online=0,source_availability=?1 WHERE id=?2",
            params![reason.as_str(), id],
        )?;
        self.attempts.remove(&id);
        Ok(())
    }

    /// Deletes one conversation message, its folder children and every file
    /// they downloaded (D16).
    pub fn delete_message(&mut self, id: MessageId) -> Result<(), StoreError> {
        let rec = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        let children = self.folder_children(id)?;
        let tx = self.db.unchecked_transaction()?;
        self.hide_shared(id)?;
        self.db
            .execute("DELETE FROM transfer_message WHERE id=?1", params![id])?;
        tx.commit()?;
        for record in children.iter().chain(std::iter::once(&rec)) {
            self.attempts.remove(&record.id);
            self.thumbnail_attempts.remove(&record.id);
            self.remove_attachment_dir(&record.content_id);
            if let Some(path) = &record.thumbnail_path {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }

    pub fn clear_cached_files(&mut self) -> Result<(), StoreError> {
        let mut stmt = self
            .db
            .prepare("SELECT content_id FROM transfer_message WHERE local_path IS NOT NULL")?;
        let ids: Vec<String> = stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
        drop(stmt);
        for content_id in ids {
            let dir = self.content_dir(&content_id)?;
            if dir.exists() {
                fs::remove_dir_all(dir)?;
            }
        }
        // Own-sent attachments have no remote source to download from again,
        // so their content is reported as evicted rather than as a placeholder
        // that could be fetched.
        self.db.execute(
            "UPDATE transfer_message SET local_path=NULL,download_state='placeholder',
             source_online=0,source_availability='evicted'
             WHERE outgoing=1 AND local_path IS NOT NULL",
            [],
        )?;
        // A phone-to-phone message cannot be fetched again: its sender
        // streamed it once and keeps nothing to serve.
        self.db.execute(
            "UPDATE transfer_message SET source_online=0,source_availability='evicted'
             WHERE outgoing=0 AND direct_peer IS NOT NULL AND kind <> 'text'",
            [],
        )?;
        self.db.execute(
            "UPDATE transfer_message SET local_path=NULL,download_state='placeholder'
             WHERE outgoing=0 AND (kind <> 'text' OR download_state='downloading')",
            [],
        )?;
        self.attempts.clear();
        self.remove_unowned()
    }

    /// Size of the attachment directory, i.e. what `clear_cached_files`
    /// frees (received downloads and copies of own sent attachments).
    /// Thumbnails belong to the history, like the records: they are kept by
    /// "清理缓存" and are not counted here.
    pub fn cached_bytes(&self) -> Result<u64, StoreError> {
        let mut total = 0u64;
        let staging = self.staging_dir();
        let mut directories = vec![self.files_root.clone()];
        let thumbnails = self.thumbnail_dir();
        while let Some(directory) = directories.pop() {
            if directory == thumbnails || directory == staging {
                continue;
            }
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    directories.push(entry.path());
                } else if kind.is_file() {
                    total = total.saturating_add(entry.metadata()?.len());
                }
            }
        }
        Ok(total)
    }

    fn thumbnail_dir(&self) -> PathBuf {
        self.files_root.join(THUMBNAIL_DIR)
    }

    fn set_thumbnail(
        &self,
        id: MessageId,
        state: ThumbnailState,
        preview: Option<(&Path, u32, u32)>,
    ) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO transfer_thumbnail(record_id,state,path,width,height) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(record_id) DO UPDATE SET state=excluded.state,path=excluded.path,
                 width=excluded.width,height=excluded.height",
            params![
                id,
                state.as_str(),
                preview.map(|(path, _, _)| path.to_string_lossy().into_owned()),
                preview.map(|(_, width, _)| width),
                preview.map(|(_, _, height)| height),
            ],
        )?;
        Ok(())
    }

    /// Starts one thumbnail request: the record shows `Loading` until
    /// `complete_thumbnail` or `finish_thumbnail` with the returned attempt.
    pub(crate) fn begin_thumbnail(&mut self, id: MessageId) -> Result<u64, StoreError> {
        if self.thumbnail_attempts.contains_key(&id) {
            return Err(StoreError::DownloadInProgress);
        }
        let record = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        let preview = record
            .thumbnail_path
            .as_deref()
            .map(Path::new)
            .zip(record.thumbnail_width.zip(record.thumbnail_height))
            .map(|(p, (w, h))| (p, w, h));
        self.set_thumbnail(id, ThumbnailState::Loading, preview)?;
        self.next_attempt = self.next_attempt.wrapping_add(1);
        self.thumbnail_attempts.insert(id, self.next_attempt);
        Ok(self.next_attempt)
    }

    pub(crate) fn thumbnail_attempt(&self, id: MessageId) -> Option<u64> {
        self.thumbnail_attempts.get(&id).copied()
    }

    /// Stores a received preview (`mime` is image/jpeg or image/png) as
    /// `.thumbnails/<record>.<ext>`, written atomically.
    pub(crate) fn complete_thumbnail(
        &mut self,
        id: MessageId,
        attempt: u64,
        received: ReceivedThumbnail<'_>,
    ) -> Result<(), StoreError> {
        let ReceivedThumbnail {
            mime,
            width,
            height,
            bytes,
            source_bytes,
        } = received;
        if self.thumbnail_attempts.get(&id) != Some(&attempt) {
            return Err(StoreError::MessageNotFound(id));
        }
        let previous = self.by_id(id)?.and_then(|record| record.thumbnail_path);
        let extension = match mime {
            "image/png" => "png",
            _ => "jpg",
        };
        let dir = self.thumbnail_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{id}.{extension}"));
        let temporary = dir.join(format!("{id}.{extension}.partial"));
        fs::write(&temporary, bytes)?;
        fs::rename(&temporary, &path)?;
        self.set_thumbnail(id, ThumbnailState::Ready, Some((path.as_path(), width, height)))?;
        if let Some(previous) = previous.filter(|previous| Path::new(previous) != path.as_path()) {
            let _ = fs::remove_file(previous);
        }
        // An offer may announce a bitmap of unknown size; the PC tells it here.
        if let Some(size) = source_bytes.filter(|size| *size > 0) {
            self.db.execute(
                "UPDATE transfer_message SET size=?1 WHERE id=?2 AND size=0",
                params![i64::try_from(size).unwrap_or(i64::MAX), id],
            )?;
        }
        self.thumbnail_attempts.remove(&id);
        Ok(())
    }

    /// Ends a thumbnail request without a preview. `Expired` also marks the
    /// record's source as no longer having the content, so opening it does
    /// not try to download what is gone.
    pub(crate) fn finish_thumbnail(
        &mut self,
        id: MessageId,
        attempt: u64,
        state: ThumbnailState,
    ) -> Result<(), StoreError> {
        if self.thumbnail_attempts.get(&id) != Some(&attempt) {
            return Err(StoreError::MessageNotFound(id));
        }
        self.thumbnail_attempts.remove(&id);
        let record = self.by_id(id)?.ok_or(StoreError::MessageNotFound(id))?;
        let preview = record
            .thumbnail_path
            .as_deref()
            .map(Path::new)
            .zip(record.thumbnail_width.zip(record.thumbnail_height))
            .map(|(p, (w, h))| (p, w, h));
        self.set_thumbnail(id, state, preview)?;
        if state == ThumbnailState::Expired {
            self.expire(id)?;
        }
        Ok(())
    }

    /// The source reported that the original no longer exists (24-hour
    /// retention over, source restarted without it, cache cleared): the
    /// record is no longer downloadable. Kept across reconnects.
    pub(crate) fn expire(&self, id: MessageId) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_message SET source_online=0,source_availability=?1 WHERE id=?2",
            params![SourceAvailability::NoSuchItem.as_str(), id],
        )?;
        Ok(())
    }

    /// A request cannot outlive the session that sent it (see
    /// `fail_interrupted_sends`): `Loading` becomes `None` so the next
    /// visibility asks again.
    pub(crate) fn reset_interrupted_thumbnails(&self) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE transfer_thumbnail SET state=?1 WHERE state=?2",
            params![ThumbnailState::None.as_str(), ThumbnailState::Loading.as_str()],
        )?;
        Ok(())
    }

    pub fn by_id(&self, id: MessageId) -> Result<Option<MessageRecord>, StoreError> {
        find_by_id(&self.db, id)
    }

    pub(crate) fn by_content_id(&self, id: &str) -> Result<Option<MessageRecord>, StoreError> {
        find_content_id(&self.db, id)
    }

    fn content_dir(&self, id: &str) -> Result<PathBuf, StoreError> {
        let id = validate_content_id(id.to_owned())?;
        let directory = id
            .strip_prefix("v2:")
            .map_or_else(|| id.clone(), |value| format!("v2_{}", value.replace(':', "_")));
        Ok(self.files_root.join(directory))
    }
}

/// The schema stays at version 1 until release (no migrations). A history
/// written by an older development build is unreadable by this one, so it is
/// discarded together with its attachments instead of being converted.
fn discard_unreadable_history(db: &mut Connection, files_root: &Path) -> Result<(), StoreError> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let columns: Vec<String> = {
        let mut stmt = tx.prepare("SELECT name FROM pragma_table_info('transfer_message')")?;
        stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?
    };
    if columns.is_empty() || columns.iter().map(String::as_str).eq(COLUMNS.split(',')) {
        return Ok(());
    }
    tracing::warn!(
        columns = columns.len(),
        "discarding transfer history written by an incompatible development build"
    );
    tx.execute_batch(
        "DROP TABLE IF EXISTS shared_order;
         DROP TABLE IF EXISTS shared_unavailable;
         DROP TABLE IF EXISTS shared_source;
         DROP TABLE IF EXISTS shared_message;
         DROP TABLE IF EXISTS shared_history;
         DROP TABLE IF EXISTS shared_text;
         DROP TABLE IF EXISTS transfer_thumbnail;
         DROP TABLE IF EXISTS transfer_message;",
    )?;
    tx.commit()?;
    if files_root.exists() {
        fs::remove_dir_all(files_root)?;
    }
    Ok(())
}

fn insert_incoming(
    db: &Connection,
    now_ms: i64,
    item: IncomingAnnouncement,
    parent: Option<MessageId>,
) -> Result<MessageRecord, StoreError> {
    let kind = item.kind;
    let content_id = validate_content_id(item.content_id)?;
    // Announcements contain metadata only.  Even an auto-download
    // candidate stays a placeholder until the content stream completes.
    let download = if kind == AttachmentKind::Text && item.text.is_some() {
        DownloadState::Complete
    } else {
        DownloadState::Placeholder
    };
    // A history page can supply inline content while Current's Low read is
    // still active. That attempt owns the body/state until its final reply;
    // completing it here would invalidate its later chunks and finalization.
    db.execute(
        &format!(
            "INSERT INTO transfer_message({COLUMNS})
             VALUES(NULL,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,?12,?13,?14,?15,?16,?17,?18,NULL,0,NULL,NULL,NULL,?19,NULL,NULL,NULL,NULL)
             ON CONFLICT(content_id) DO UPDATE SET source_online=excluded.source_online,
                 source_availability=excluded.source_availability,
                 text=CASE WHEN transfer_message.download_state='downloading' THEN transfer_message.text ELSE COALESCE(transfer_message.text,excluded.text) END,
                 download_state=CASE WHEN transfer_message.download_state='downloading' THEN 'downloading' WHEN excluded.text IS NOT NULL THEN 'complete' ELSE transfer_message.download_state END"
        ),
        params![
            now_ms,
            item.sender_id,
            item.proxy_id,
            content_id,
            kind.as_str(),
            item.display_name,
            item.mime,
            i64::try_from(item.size).unwrap_or(i64::MAX),
            item.text,
            item.rich_text_format.map(|f| f.as_str()),
            download.as_str(),
            item.source.is_online() as i64,
            item.source.availability.as_str(),
            item.operation_id.to_vec(),
            item.message_id.to_vec(),
            item.item_id,
            item.representation_id,
            parent,
            item.sensitive as i64,
        ],
    )?;
    find_content_id(db, &content_id)?.ok_or(StoreError::MessageNotFound(0))
}

fn find_by_id(db: &Connection, id: MessageId) -> Result<Option<MessageRecord>, StoreError> {
    db.query_row(
        &format!("SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE id=?1"),
        params![id],
        row_to_record,
    )
    .optional()
    .map_err(Into::into)
}

fn find_content_id(db: &Connection, id: &str) -> Result<Option<MessageRecord>, StoreError> {
    db.query_row(
        &format!("SELECT {COLUMNS},{THUMBNAIL_COLUMNS} FROM {RECORDS} WHERE content_id=?1"),
        params![id],
        row_to_record,
    )
    .optional()
    .map_err(Into::into)
}

fn validate_content_id(id: String) -> Result<String, StoreError> {
    let safe = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    let v2 = id.strip_prefix("v2:").and_then(|value| {
        let fields: Vec<_> = value.split(':').collect();
        let integer = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        let suffix = match fields.get(3..) {
            Some([]) => true,
            Some([kind, a, b]) => matches!(*kind, "r" | "f") && integer(a) && integer(b),
            _ => false,
        };
        (suffix
            && fields[0].len() == 64
            && fields[0].bytes().all(|b| b.is_ascii_hexdigit())
            && integer(fields[1])
            && integer(fields[2]))
        .then_some(())
    });
    if id.is_empty() || id.len() > 160 || (!safe && v2.is_none()) {
        return Err(StoreError::InvalidContentId);
    }
    Ok(id)
}

/// Image MIME type for a file name, used to present image files as images.
pub(crate) fn image_mime_for_name(name: &str) -> Option<&'static str> {
    let (_, extension) = name.rsplit_once('.')?;
    Some(match extension.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

pub(crate) fn extension_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime.split(';').next()?.trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "text/html" => "html",
        "application/rtf" | "text/rtf" => "rtf",
        "text/plain" => "txt",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        _ => return None,
    })
}

pub(crate) fn rich_mime(format: RichTextFormat) -> &'static str {
    match format {
        RichTextFormat::Html => "text/html",
        RichTextFormat::Rtf => "application/rtf",
    }
}

/// File name of a stored attachment: rich text is `content.html` /
/// `content.rtf` (D15), everything else keeps its sanitized display name and
/// gains the MIME extension when the name has none.
pub(crate) fn stored_file_name(
    kind: AttachmentKind,
    display_name: &str,
    mime: Option<&str>,
    rich_text_format: Option<RichTextFormat>,
) -> String {
    match kind {
        AttachmentKind::RichText => match rich_text_format {
            Some(RichTextFormat::Rtf) => "content.rtf".into(),
            _ => "content.html".into(),
        },
        AttachmentKind::Text => "content.txt".into(),
        _ => {
            let mut name = sanitize_file_name(display_name);
            let has_extension = name
                .rsplit_once('.')
                .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty());
            if !has_extension && let Some(extension) = mime.and_then(extension_for_mime) {
                name = format!("{name}.{extension}");
            }
            name
        }
    }
}

fn sanitize_file_name(name: &str) -> String {
    const MAX: usize = 200;
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_owned();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        return "file".into();
    }
    if cleaned.len() <= MAX {
        return cleaned;
    }
    // Keep the extension when shortening an over-long name.
    let (stem, extension) = match cleaned.rsplit_once('.') {
        Some((stem, extension)) if extension.len() <= 16 && !stem.is_empty() => (stem, Some(extension)),
        _ => (cleaned.as_str(), None),
    };
    let budget = MAX - extension.map_or(0, |e| e.len() + 1);
    let mut end = budget.min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    match extension {
        Some(extension) => format!("{}.{extension}", &stem[..end]),
        None => stem[..end].to_owned(),
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    let invalid = |index: usize, name: &str, ty| rusqlite::Error::InvalidColumnType(index, name.into(), ty);
    use rusqlite::types::Type;
    let kind =
        AttachmentKind::parse(row.get::<_, String>(5)?.as_str()).ok_or_else(|| invalid(5, "kind", Type::Text))?;
    let download = DownloadState::parse(row.get::<_, String>(11)?.as_str())
        .ok_or_else(|| invalid(11, "download_state", Type::Text))?;
    let rich_text_format = row.get::<_, Option<String>>(10)?.map(|f| {
        if f == "html" {
            RichTextFormat::Html
        } else {
            RichTextFormat::Rtf
        }
    });
    let source_availability = SourceAvailability::parse(row.get::<_, String>(14)?.as_str())
        .ok_or_else(|| invalid(14, "source_availability", Type::Text))?;
    let operation_id = row
        .get::<_, Vec<u8>>(15)?
        .try_into()
        .map_err(|_| invalid(15, "operation_id", Type::Blob))?;
    let message_id = row
        .get::<_, Vec<u8>>(16)?
        .try_into()
        .map_err(|_| invalid(16, "message_id", Type::Blob))?;
    let delivery = row
        .get::<_, Option<String>>(22)?
        .map(|value| DeliveryState::parse(&value).ok_or_else(|| invalid(22, "delivery_state", Type::Text)))
        .transpose()?;
    let delivery_failure = row
        .get::<_, Option<String>>(23)?
        .map(|value| DeliveryFailure::parse(&value).ok_or_else(|| invalid(23, "delivery_failure", Type::Text)))
        .transpose()?;
    Ok(MessageRecord {
        history_order: row.get(34)?,
        id: row.get(0)?,
        created_ms: row.get(1)?,
        sender_id: row.get(2)?,
        proxy_id: row.get(3)?,
        content_id: row.get(4)?,
        kind,
        display_name: row.get(6)?,
        mime: row.get(7)?,
        size: row.get::<_, i64>(8)?.try_into().unwrap_or(0),
        text: row.get(9)?,
        rich_text_format,
        download,
        local_path: row.get(12)?,
        source_online: row.get::<_, i64>(13)? != 0,
        source_availability,
        operation_id,
        message_id,
        item_id: row.get::<_, i64>(17)?.try_into().unwrap_or(0),
        representation_id: row.get::<_, i64>(18)?.try_into().unwrap_or(0),
        parent_id: row.get(19)?,
        item_count: row.get::<_, Option<i64>>(20)?.and_then(|count| count.try_into().ok()),
        outgoing: row.get::<_, i64>(21)? != 0,
        delivery,
        delivery_failure,
        delivery_detail: row.get(24)?,
        sensitive: row.get::<_, i64>(25)? != 0,
        thumbnail: row
            .get::<_, Option<String>>(30)?
            .map(|value| ThumbnailState::parse(&value).ok_or_else(|| invalid(30, "thumbnail_state", Type::Text)))
            .transpose()?
            .unwrap_or_default(),
        thumbnail_path: row.get(31)?,
        thumbnail_width: row.get::<_, Option<i64>>(32)?.and_then(|value| value.try_into().ok()),
        thumbnail_height: row.get::<_, Option<i64>>(33)?.and_then(|value| value.try_into().ok()),
        direct_peer: row.get(26)?,
        direct_peer_name: row.get(27)?,
        image_width: row.get::<_, Option<i64>>(28)?.and_then(|value| value.try_into().ok()),
        image_height: row.get::<_, Option<i64>>(29)?.and_then(|value| value.try_into().ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IncomingAnnouncement, SourceState};

    fn item(id: &str, created: u64, kind: AttachmentKind, online: bool) -> IncomingAnnouncement {
        IncomingAnnouncement {
            sender_id: "sender".into(),
            proxy_id: None,
            content_id: id.into(),
            kind,
            display_name: id.into(),
            mime: None,
            size: 1,
            text: (kind == AttachmentKind::Text).then(|| id.into()),
            rich_text_format: None,
            source: SourceState {
                source_id: "sender".into(),
                availability: if online {
                    SourceAvailability::Ready
                } else {
                    SourceAvailability::SourceOffline
                },
            },
            operation_id: [created as u8; 16],
            message_id: [created as u8 + 1; 16],
            item_id: 1,
            representation_id: 1,
            sensitive: false,
        }
    }

    fn open() -> (tempfile::TempDir, TransferStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TransferStore::open(dir.path().join("history.sqlite"), dir.path().join("files")).unwrap();
        (dir, store)
    }

    fn thumb(bytes: &[u8], source_bytes: Option<u64>) -> ReceivedThumbnail<'_> {
        ReceivedThumbnail {
            mime: "image/jpeg",
            width: 512,
            height: 288,
            bytes,
            source_bytes,
        }
    }

    fn large_image(id: &str, created: u64) -> IncomingAnnouncement {
        let mut image = item(id, created, AttachmentKind::Image, true);
        image.mime = Some("image/png".into());
        image.size = 20 * 1024 * 1024;
        image
    }

    #[test]
    fn thumbnail_is_kept_with_the_record_through_cache_clear_and_deleted_with_it() {
        use crate::model::ImagePreview;
        let (_dir, mut store) = open();
        let record = store.insert_announcement(1, large_image("big", 1)).unwrap();
        assert_eq!(record.thumbnail, ThumbnailState::None);
        assert_eq!(record.image_preview(), Some(ImagePreview::Placeholder));
        let attempt = store.begin_thumbnail(record.id).unwrap();
        assert!(matches!(
            store.begin_thumbnail(record.id),
            Err(StoreError::DownloadInProgress)
        ));
        let loading = store.by_id(record.id).unwrap().unwrap();
        assert_eq!(loading.thumbnail, ThumbnailState::Loading);
        assert_eq!(loading.image_preview(), Some(ImagePreview::Loading));
        // A stale attempt cannot complete.
        assert!(
            store
                .complete_thumbnail(record.id, attempt + 1, thumb(b"jpeg", None))
                .is_err()
        );
        store
            .complete_thumbnail(record.id, attempt, thumb(b"jpeg bytes", Some(9)))
            .unwrap();
        let ready = store.by_id(record.id).unwrap().unwrap();
        assert_eq!(ready.thumbnail, ThumbnailState::Ready);
        assert_eq!((ready.thumbnail_width, ready.thumbnail_height), (Some(512), Some(288)));
        // A received image is shown at its thumbnail's ratio (the original's).
        assert_eq!(record.image_size(), None);
        assert_eq!(ready.image_size(), Some((512, 288)));
        let path = PathBuf::from(ready.thumbnail_path.clone().unwrap());
        assert_eq!(path.extension().unwrap(), "jpg");
        assert_eq!(fs::read(&path).unwrap(), b"jpeg bytes");
        assert_eq!(ready.image_preview(), Some(ImagePreview::Thumbnail));
        assert_eq!(
            store.page(None, 10).unwrap().messages[0].thumbnail,
            ThumbnailState::Ready
        );
        // The thumbnail is history, not cache.
        assert_eq!(store.cached_bytes().unwrap(), 0);
        store.clear_cached_files().unwrap();
        assert!(path.exists());
        // While the original downloads the thumbnail stays visible.
        store.begin_download(record.id).unwrap();
        let downloading = store.by_id(record.id).unwrap().unwrap();
        assert_eq!(downloading.image_preview(), Some(ImagePreview::Thumbnail));
        store.delete_message(record.id).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn expired_original_is_final_and_failures_and_interruptions_can_be_retried() {
        use crate::model::ImagePreview;
        let (_dir, mut store) = open();
        let gone = store.insert_announcement(1, large_image("gone", 1)).unwrap();
        let attempt = store.begin_thumbnail(gone.id).unwrap();
        store
            .finish_thumbnail(gone.id, attempt, ThumbnailState::Expired)
            .unwrap();
        let gone = store.by_id(gone.id).unwrap().unwrap();
        assert_eq!(gone.source_availability, SourceAvailability::NoSuchItem);
        assert_eq!(gone.image_preview(), Some(ImagePreview::Expired));
        assert!(matches!(
            store.begin_download(gone.id),
            Err(StoreError::SourceUnavailable(SourceAvailability::NoSuchItem))
        ));
        // Reconnecting the source does not revive it.
        store.set_peer_online("sender", false).unwrap();
        store.set_peer_online("sender", true).unwrap();
        assert_eq!(
            store.by_id(gone.id).unwrap().unwrap().image_preview(),
            Some(ImagePreview::Expired)
        );

        let failed = store.insert_announcement(2, large_image("failed", 2)).unwrap();
        let attempt = store.begin_thumbnail(failed.id).unwrap();
        store
            .finish_thumbnail(failed.id, attempt, ThumbnailState::Failed)
            .unwrap();
        let record = store.by_id(failed.id).unwrap().unwrap();
        assert_eq!(record.image_preview(), Some(ImagePreview::Failed));
        assert_eq!(record.source_availability, SourceAvailability::Ready);
        store.begin_thumbnail(failed.id).unwrap();
        // A new session finds the request interrupted and asks again.
        store.reset_interrupted_thumbnails().unwrap();
        assert_eq!(store.by_id(failed.id).unwrap().unwrap().thumbnail, ThumbnailState::None);

        let offline = store.insert_announcement(3, large_image("offline", 3)).unwrap();
        store.set_peer_online("sender", false).unwrap();
        assert_eq!(
            store.by_id(offline.id).unwrap().unwrap().image_preview(),
            Some(ImagePreview::Offline)
        );
    }

    #[test]
    fn cursor_pagination_is_stable_and_content_ids_are_safe() {
        let (_dir, mut store) = open();
        store
            .insert_announcement(1, item("one", 1, AttachmentKind::File, true))
            .unwrap();
        store
            .insert_announcement(2, item("two", 2, AttachmentKind::File, true))
            .unwrap();
        let first = store.page(None, 1).unwrap();
        assert_eq!(first.messages[0].content_id, "two");
        let second = store.page(first.next, 1).unwrap();
        assert_eq!(second.messages[0].content_id, "one");
        assert!(store.content_dir("../escape").is_err());
    }

    #[test]
    fn offline_content_cannot_start_download_and_delete_removes_file() {
        let (_dir, mut store) = open();
        let offline = store
            .insert_announcement(1, item("offline", 1, AttachmentKind::File, false))
            .unwrap();
        assert!(matches!(
            store.begin_download(offline.id),
            Err(StoreError::SourceOffline)
        ));
        let online = store
            .insert_announcement(2, item("online", 2, AttachmentKind::File, true))
            .unwrap();
        let path = store.begin_download(online.id).unwrap();
        std::fs::write(&path, b"payload").unwrap();
        store.complete_download(online.id).unwrap();
        assert!(path.exists());
        store.delete_message(online.id).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn downloads_keep_real_names_and_extensions() {
        let (_dir, mut store) = open();
        let mut file = item("named", 1, AttachmentKind::File, true);
        file.display_name = "Report 2026.pdf".into();
        let file = store.insert_announcement(1, file).unwrap();
        let path = store.begin_download(file.id).unwrap();
        assert_eq!(path.file_name().unwrap(), "Report 2026.pdf");

        let mut image = item("bitmap", 2, AttachmentKind::Image, true);
        image.display_name = "image".into();
        image.mime = Some("image/png".into());
        let image = store.insert_announcement(2, image).unwrap();
        assert_eq!(
            store.begin_download(image.id).unwrap().file_name().unwrap(),
            "image.png"
        );

        let mut html = item("html", 3, AttachmentKind::RichText, true);
        html.rich_text_format = Some(RichTextFormat::Html);
        let html = store.insert_announcement(3, html).unwrap();
        assert_eq!(
            store.begin_download(html.id).unwrap().file_name().unwrap(),
            "content.html"
        );

        assert_eq!(sanitize_file_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_file_name(".."), "file");
        let long = format!("{}.jpeg", "x".repeat(300));
        let short = sanitize_file_name(&long);
        assert!(short.len() <= 200 && short.ends_with(".jpeg"));
    }

    #[test]
    fn folder_children_stay_out_of_the_page_and_are_deleted_with_the_folder() {
        let (_dir, mut store) = open();
        let folder = store
            .insert_announcement(1, item("folder", 1, AttachmentKind::Folder, true))
            .unwrap();
        let child = store
            .insert_directory_file(2, folder.id, item("child", 2, AttachmentKind::File, true))
            .unwrap();
        assert_eq!(child.parent_id, Some(folder.id));
        let page = store.page(None, 10).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, folder.id);
        let path = store.begin_download(child.id).unwrap();
        std::fs::write(&path, b"child").unwrap();
        store.complete_download(child.id).unwrap();
        assert_eq!(store.folder_children(folder.id).unwrap().len(), 1);
        assert_eq!(store.cached_bytes().unwrap(), 5);
        store.delete_message(folder.id).unwrap();
        assert!(store.by_id(child.id).unwrap().is_none());
        assert!(!path.exists());
        assert_eq!(store.cached_bytes().unwrap(), 0);
    }

    /// Own images keep the original's displayed size the host gave, one per
    /// item of a share, so both apps show them at their own ratio
    /// (spec 2026-11-11 「图片尺寸」); other own items have none.
    #[test]
    fn own_images_keep_the_original_size_per_item() {
        let (_dir, mut store) = open();
        let own = |index: u32, kind: AttachmentKind, image_size: Option<(u32, u32)>| OutgoingItem {
            sender_id: "me".into(),
            content_id: format!("out-7-{index}"),
            kind,
            display_name: format!("item {index}"),
            mime: None,
            size: 3,
            text: None,
            rich_text_format: None,
            local_path: None,
            operation_id: [7; 16],
            message_id: [8; 16],
            item_id: index,
            image_size,
            rich: None,
        };
        let records = store
            .insert_outgoing(
                5,
                vec![
                    own(0, AttachmentKind::Image, Some((3024, 4032))),
                    own(1, AttachmentKind::Image, Some((1920, 1080))),
                    own(2, AttachmentKind::File, None),
                ],
            )
            .unwrap();
        let sizes: Vec<_> = records
            .iter()
            .map(|record| store.by_id(record.id).unwrap().unwrap().image_size())
            .collect();
        assert_eq!(sizes, vec![Some((3024, 4032)), Some((1920, 1080)), None]);
        let portrait = store.by_id(records[0].id).unwrap().unwrap();
        assert_eq!((portrait.image_width, portrait.image_height), (Some(3024), Some(4032)));
        assert_eq!(portrait.download_status(), None, "own messages have no download button");
        // Received records never carry an own size.
        let received = store.insert_announcement(6, large_image("in", 6)).unwrap();
        assert_eq!((received.image_width, received.image_height), (None, None));
    }

    /// The download button tells a failed download from a PC (retryable)
    /// from a phone-to-phone receive that failed or was cancelled (never),
    /// and both from expired content and an offline source.
    #[test]
    fn download_status_tells_a_retryable_download_from_a_failed_phone_receive() {
        use crate::model::DownloadStatus;
        let (_dir, mut store) = open();
        let status = |store: &TransferStore, id| store.by_id(id).unwrap().unwrap().download_status();
        let file = store
            .insert_announcement(1, item("pc-file", 1, AttachmentKind::File, true))
            .unwrap();
        assert_eq!(status(&store, file.id), Some(DownloadStatus::NotDownloaded));
        store.begin_download(file.id).unwrap();
        assert_eq!(status(&store, file.id), Some(DownloadStatus::Downloading));
        store.fail_download(file.id).unwrap();
        assert_eq!(status(&store, file.id), Some(DownloadStatus::DownloadFailed));
        // Offline wins while the source is away; back online it is retryable again.
        store.set_peer_online("sender", false).unwrap();
        assert_eq!(status(&store, file.id), Some(DownloadStatus::SourceOffline));
        store.set_peer_online("sender", true).unwrap();
        assert_eq!(status(&store, file.id), Some(DownloadStatus::DownloadFailed));
        store.begin_download(file.id).unwrap();
        store.complete_download(file.id).unwrap();
        assert_eq!(status(&store, file.id), Some(DownloadStatus::Downloaded));

        let gone = store
            .insert_announcement(2, item("pc-gone", 2, AttachmentKind::File, true))
            .unwrap();
        store.begin_download(gone.id).unwrap();
        store.mark_unavailable(gone.id, SourceAvailability::NoSuchItem).unwrap();
        assert_eq!(status(&store, gone.id), Some(DownloadStatus::Expired));

        let direct = |store: &mut TransferStore, id: &str, created: u64| {
            let record = store
                .insert_announcement(created as i64, item(id, created, AttachmentKind::File, true))
                .unwrap();
            store.set_direct(&[record.id], "tablet", None).unwrap();
            store.begin_download(record.id).unwrap();
            record.id
        };
        let lost = direct(&mut store, "p2p-lost", 3);
        assert_eq!(status(&store, lost), Some(DownloadStatus::Downloading));
        store
            .fail_direct(lost, DeliveryFailure::PeerDisconnected, "link lost")
            .unwrap();
        assert_eq!(status(&store, lost), Some(DownloadStatus::ReceiveFailed));
        let cancelled = direct(&mut store, "p2p-cancel", 4);
        store
            .fail_direct(cancelled, DeliveryFailure::Cancelled, "cancelled")
            .unwrap();
        assert_eq!(status(&store, cancelled), Some(DownloadStatus::Cancelled));
        // Neither can be fetched again.
        assert!(store.begin_download(lost).is_err());
        assert!(store.begin_download(cancelled).is_err());
    }

    #[test]
    fn outgoing_records_track_delivery_and_cannot_be_downloaded() {
        let (_dir, mut store) = open();
        let records = store
            .insert_outgoing(
                5,
                vec![OutgoingItem {
                    sender_id: "me".into(),
                    content_id: "out-1".into(),
                    kind: AttachmentKind::Text,
                    display_name: "Text".into(),
                    mime: Some("text/plain".into()),
                    size: 2,
                    text: Some("hi".into()),
                    rich_text_format: None,
                    local_path: None,
                    operation_id: [9; 16],
                    message_id: [8; 16],
                    item_id: 0,
                    image_size: None,
                    rich: None,
                }],
            )
            .unwrap();
        let id = records[0].id;
        assert!(records[0].outgoing);
        assert_eq!(records[0].delivery, Some(DeliveryState::Sending));
        assert!(matches!(store.begin_download(id), Err(StoreError::OwnMessage(_))));
        store
            .set_delivery(
                &[id],
                DeliveryState::Failed,
                Some((DeliveryFailure::Rejected, "Unsupported")),
            )
            .unwrap();
        let failed = store.by_id(id).unwrap().unwrap();
        assert_eq!(failed.delivery_failure, Some(DeliveryFailure::Rejected));
        assert_eq!(failed.delivery_detail.as_deref(), Some("Unsupported"));
        assert_eq!(store.outgoing_records([9; 16]).unwrap().len(), 1);
    }

    #[test]
    fn interrupted_sends_fail_when_a_new_session_owns_the_store() {
        let (_dir, mut store) = open();
        let id = store
            .insert_outgoing(
                1,
                vec![OutgoingItem {
                    sender_id: "me".into(),
                    content_id: "out-2".into(),
                    kind: AttachmentKind::Folder,
                    display_name: "Photos".into(),
                    mime: None,
                    size: 0,
                    text: None,
                    rich_text_format: None,
                    local_path: None,
                    operation_id: [1; 16],
                    message_id: [2; 16],
                    item_id: 0,
                    image_size: None,
                    rich: None,
                }],
            )
            .unwrap()[0]
            .id;
        store.fail_interrupted_sends().unwrap();
        let record = store.by_id(id).unwrap().unwrap();
        assert_eq!(record.delivery, Some(DeliveryState::Failed));
        assert_eq!(record.delivery_failure, Some(DeliveryFailure::Interrupted));
    }

    #[test]
    fn history_from_an_incompatible_build_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.sqlite");
        let files = dir.path().join("files");
        std::fs::create_dir_all(files.join("old")).unwrap();
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE transfer_message (id INTEGER PRIMARY KEY, created_ms INTEGER);")
            .unwrap();
        let mut store = TransferStore::open(&db, &files).unwrap();
        assert!(!files.join("old").exists());
        store
            .insert_announcement(1, item("fresh", 1, AttachmentKind::File, true))
            .unwrap();
        assert_eq!(store.page(None, 10).unwrap().messages.len(), 1);
    }
}
