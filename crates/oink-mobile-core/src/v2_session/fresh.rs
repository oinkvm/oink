//! A fresh remote text notice captures a host-side clipboard revision before
//! downloading. A history/current query never creates a native write ticket.
use crate::{ClipboardRich, MessageId};
use std::sync::{Arc, Mutex};

pub trait FreshClipboardObserver: Send + Sync {
    fn begin(&self, source: &str) -> Option<u64>;
    /// `rich`: the text's rich text, written next to it (2026-11-12).
    /// `marker`: Oink clip marker bytes the host writes next to the text
    /// (`oink_foundation::clipboard_marker`); None for a direct share, which
    /// has no clip identity.
    fn complete(&self, ticket: u64, source: &str, text: &str, rich: Option<ClipboardRich>, marker: Option<Vec<u8>>);
    /// The group has a newer clip that is not this phone's own copy: a fresh
    /// offer from another device (`marker` names it) or a broadcast text
    /// (None). Whatever this phone's clipboard holds is then no longer the
    /// group's current content (clipboard.md §4, D11).
    fn group_changed(&self, _marker: Option<Vec<u8>>) {}
}

#[derive(Default)]
struct State {
    observer: Option<Arc<dyn FreshClipboardObserver>>,
    latest_content: Option<String>,
    pending: Option<(MessageId, u64)>,
}

#[derive(Default)]
pub(super) struct FreshClipboard(Mutex<State>);

impl FreshClipboard {
    pub(super) fn attach(&self, observer: Arc<dyn FreshClipboardObserver>) {
        self.0.lock().unwrap().observer = Some(observer);
    }

    pub(super) fn begin(&self, content_id: &str, source: &str) -> Option<u64> {
        let mut state = self.0.lock().unwrap();
        if state.latest_content.as_deref() == Some(content_id) {
            return None;
        }
        state.latest_content = Some(content_id.to_owned());
        state.pending = None;
        state.observer.as_ref()?.begin(source)
    }

    pub(super) fn group_changed(&self, marker: Option<Vec<u8>>) {
        let observer = self.0.lock().unwrap().observer.clone();
        if let Some(observer) = observer {
            observer.group_changed(marker);
        }
    }

    pub(super) fn waiting(&self, content_id: &str, record: MessageId, ticket: u64) {
        let mut state = self.0.lock().unwrap();
        if state.latest_content.as_deref() == Some(content_id) {
            state.pending = Some((record, ticket));
        }
    }

    pub(super) fn complete(
        &self,
        ticket: u64,
        source: &str,
        text: &str,
        rich: Option<ClipboardRich>,
        marker: Option<Vec<u8>>,
    ) {
        let observer = self.0.lock().unwrap().observer.clone();
        if let Some(observer) = observer {
            observer.complete(ticket, source, text, rich, marker);
        }
    }

    /// Text message `record` (and its rich text, if any) is on this phone.
    pub(super) fn downloaded(
        &self,
        record: MessageId,
        source: &str,
        text: &str,
        rich: Option<ClipboardRich>,
        marker: Option<Vec<u8>>,
    ) {
        let ticket = {
            let mut state = self.0.lock().unwrap();
            if !state.pending.is_some_and(|(pending, _)| pending == record) {
                return;
            }
            state.pending.take().map(|(_, ticket)| ticket)
        };
        if let Some(ticket) = ticket {
            self.complete(ticket, source, text, rich, marker);
        }
    }

    pub(super) fn failed(&self, record: MessageId) {
        let mut state = self.0.lock().unwrap();
        if state.pending.is_some_and(|(pending, _)| pending == record) {
            state.pending = None;
        }
    }
}
