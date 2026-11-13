//! Transfer history access through the running session's own store.
//!
//! `MobileTransferStore` opens a second connection to the same database. That
//! is fine without a session, but while a session runs its store owns the
//! in-memory download attempts and the auto-open slot; deleting or clearing
//! through another instance leaves those alive, so a deleted record could keep
//! receiving bytes. Hosts with a session use these methods instead.

use super::{MobileError, MobileSession};
use crate::{MobileMessage, MobilePage};
use oink_mobile_core::{MobileNetworkLink, MobileV2Session, PageCursor, StoreError, TransferStore};
use std::sync::Arc;

fn store_error(error: StoreError) -> MobileError {
    match error {
        StoreError::MessageNotFound(_) => MobileError::InvalidArgument {
            detail: error.to_string(),
        },
        error => MobileError::Worker {
            detail: error.to_string(),
        },
    }
}

impl MobileSession {
    fn transfer_session(&self) -> Result<Arc<MobileV2Session<MobileNetworkLink>>, MobileError> {
        self.transfer
            .lock()
            .map_err(|_| MobileError::Worker {
                detail: "transfer mutex poisoned".into(),
            })?
            .clone()
            .ok_or(MobileError::Stopped)
    }

    fn history<R>(&self, f: impl FnOnce(&mut TransferStore) -> Result<R, StoreError>) -> Result<R, MobileError> {
        self.transfer_session()?.with_store(f).map_err(store_error)
    }
}

#[uniffi::export]
impl MobileSession {
    /// App body visibility only; share extensions and Android services leave this false.
    pub fn transfer_set_foreground(&self, foreground: bool) -> Result<(), MobileError> {
        self.transfer_session()?
            .set_history_foreground(foreground)
            .map_err(super::transfer_error)
    }
    pub fn transfer_history_boundary_ms(&self) -> Result<Option<i64>, MobileError> {
        self.history(|store| store.history_boundary_ms())
    }
    pub fn transfer_history_has_more(&self) -> Result<bool, MobileError> {
        Ok(self.transfer_session()?.history_has_more())
    }
    pub async fn transfer_load_older_history(&self) -> Result<(), MobileError> {
        self.transfer_session()?
            .load_older_history()
            .await
            .map_err(super::transfer_error)
    }
    /// One page of the conversation, newest first (same as
    /// `MobileTransferStore.page`). Folder children never appear here.
    pub fn transfer_page(
        &self,
        cursor_created_ms: Option<i64>,
        cursor_id: Option<i64>,
        limit: u32,
    ) -> Result<MobilePage, MobileError> {
        let cursor = match (cursor_created_ms, cursor_id) {
            (None, None) => None,
            (Some(created_ms), Some(id)) => Some(PageCursor { created_ms, id }),
            _ => {
                return Err(MobileError::InvalidArgument {
                    detail: "cursor requires created_ms and id".into(),
                });
            }
        };
        let page = self.history(|store| store.page(cursor, limit as usize))?;
        let (next_created_ms, next_id) = page.next.map_or((None, None), |c| (Some(c.created_ms), Some(c.id)));
        Ok(MobilePage {
            messages: page.messages.into_iter().map(Into::into).collect(),
            next_created_ms,
            next_id,
        })
    }

    /// One record by id (a conversation message or a folder child); `None`
    /// after it was deleted. Reload a record with this on
    /// `TransferAnnouncement` / `DeliveryEvent` instead of reloading a page.
    pub fn transfer_message(&self, message_id: i64) -> Result<Option<MobileMessage>, MobileError> {
        Ok(self.history(|store| store.by_id(message_id))?.map(Into::into))
    }

    /// The rich text kept with text message `message_id` (2026-11-12): the
    /// sanitized markup the bubble renders and the HTML / RTF that 复制
    /// (copy rich) writes next to the plain text. `None` when the message
    /// has no rich text on this phone (yet: it arrives after the text and a
    /// `TransferAnnouncement` of the message follows).
    pub fn transfer_rich_text(&self, message_id: i64) -> Result<Option<crate::MobileRichText>, MobileError> {
        Ok(self
            .history(|store| store.rich_text(message_id))?
            .as_ref()
            .and_then(crate::MobileRichText::read))
    }

    /// Files downloaded from inside folder message `folder_message_id`;
    /// match `directory_node` with the listing entry id.
    pub fn transfer_folder_children(&self, folder_message_id: i64) -> Result<Vec<MobileMessage>, MobileError> {
        Ok(self
            .history(|store| store.folder_children(folder_message_id))?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Deletes one message, its folder children and their files (D16), and
    /// stops any download still writing to them.
    pub fn transfer_delete_message(&self, message_id: i64) -> Result<(), MobileError> {
        self.history(|store| store.delete_message(message_id))
    }

    /// "清理缓存": deletes every attachment file and stops running downloads;
    /// message metadata stays.
    pub fn transfer_clear_cached_files(&self) -> Result<(), MobileError> {
        self.history(|store| store.clear_cached_files())
    }

    /// Bytes `transfer_clear_cached_files` would free.
    pub fn transfer_cached_bytes(&self) -> Result<u64, MobileError> {
        self.history(|store| store.cached_bytes())
    }

    /// "清理传输记录": deletes every finished record of this phone and its
    /// files (downloads, own copies, thumbnails, folder children). Transfers
    /// still sending or receiving keep their records and end normally. The
    /// deleted conversation records arrive as one `TransferRemoved`.
    pub fn transfer_clear_history(&self) -> Result<(), MobileError> {
        self.transfer_session()?.clear_history().map(drop).map_err(store_error)
    }
}
