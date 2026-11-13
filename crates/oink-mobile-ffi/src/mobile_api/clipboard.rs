use super::*;
use oink_foundation::{
    DeviceId,
    clipboard_burst::Burst,
    clipboard_dedupe::{self as dedupe, Fingerprint, Identity, Source},
    clipboard_marker,
    content::ClipId,
};
use oink_mobile_core::{
    ClipboardRich, FreshClipboardObserver, MobileNetworkLink, ProxyChoice, SendItem, SendOrigin, SharePayload,
};
use std::sync::Weak;
use std::time::Instant;

const AUTOMATIC_ID: u64 = 1 << 63;
const MAX_DIRECT_TEXT_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct Capture {
    helper: Option<[u8; 16]>,
    revision: u64,
    text: Option<String>,
    html: Option<String>,
}

impl Capture {
    /// True when this helper reports a newer clipboard revision. Whether that
    /// clipboard is a new copy is decided by its content identity.
    fn observe(&mut self, helper: [u8; 16], revision: u64, text: Option<String>, html: Option<String>) -> bool {
        let same_helper = self.helper == Some(helper);
        if same_helper && revision < self.revision {
            return false;
        }
        let changed = same_helper && revision > self.revision;
        *self = Self {
            helper: Some(helper),
            revision,
            text,
            html,
        };
        changed
    }
}

/// The content identity of this phone's clipboard and the clip it belongs
/// to (the PC's `Held`): the clip Oink wrote, None for a local copy or a
/// direct share.
#[derive(Clone)]
struct Held {
    identity: Identity,
    clip: Option<ClipId>,
}

#[derive(Default)]
struct NativeClipboard {
    revision: u64,
    next_ticket: u64,
    automatic: bool,
    native_realtime: bool,
    broadcast: bool,
    ticket: Option<(u64, u64)>,
    capture: Capture,
    proxy: Option<String>,
    /// Content identity of the native clipboard (`clipboard_dedupe`). Known
    /// only while the privileged helper reports clipboard text in real time;
    /// otherwise this phone cannot read its clipboard and never dedupes.
    held: Option<Held>,
    /// The group has a newer clip than the held one (a fresh offer or a
    /// broadcast from another device that this phone did not write): copying
    /// the held content again is a new copy (D11).
    newer: bool,
    /// The announcement debounce of helper-reported changes (the PC's,
    /// `oink_foundation::clipboard_burst`): reads wait for 250 ms of quiet,
    /// at most 1 s after a burst's first change.
    burst: Burst,
    /// Text last handed to the host for an automatic write, with its source.
    delivered: Option<(Fingerprint, DeviceId)>,
    /// Content Oink itself just wrote: the helper's next report of it is our
    /// own write, never announced (independent of the deduplication
    /// setting). Covers writes without a clip marker (direct shares).
    own_write: Option<Fingerprint>,
    /// "Clipboard deduplication" (PC setting of the same name). The phone has
    /// no settings UI for it yet: always the default, on.
    dedupe_off: bool,
}

impl NativeClipboard {
    fn cancel(&mut self) {
        self.ticket = None;
    }
}

/// Where clipboard events go as they happen: the session's listener queue.
pub(super) type ClipboardEventSink = Arc<dyn Fn(ClipboardEvent) + Send + Sync>;

#[derive(uniffi::Object)]
pub struct MobileClipboard {
    native: Mutex<NativeClipboard>,
    transfer: Weak<MobileV2Session<MobileNetworkLink>>,
    me: DeviceId,
    sink: Mutex<Option<ClipboardEventSink>>,
}

impl std::fmt::Debug for MobileClipboard {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("MobileClipboard").finish_non_exhaustive()
    }
}

/// The phone's content fingerprint: plain text with its HTML (the formats
/// the helper reads), like a PC's text item with its rich text.
fn fingerprint(text: &str, html: Option<&str>) -> Fingerprint {
    Fingerprint::rich_text(text, html, None)
}

impl MobileClipboard {
    pub(super) fn new(transfer: &Arc<MobileV2Session<MobileNetworkLink>>) -> Self {
        Self {
            native: Mutex::new(NativeClipboard::default()),
            transfer: Arc::downgrade(transfer),
            me: transfer.device_id(),
            sink: Mutex::new(None),
        }
    }

    pub(super) fn set_event_sink(&self, sink: ClipboardEventSink) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    pub(super) fn set_proxy(&self, proxy: Option<String>) {
        self.native.lock().unwrap().proxy = proxy;
    }

    /// Called without the native lock held.
    fn emit(&self, event: ClipboardEvent) {
        let sink = self.sink.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink(event);
        }
    }

    /// Text (with its HTML) to announce for a helper-reported clipboard, or
    /// None. The PC's sending-side rules (clipboard.md §4): a marker naming
    /// the clip this phone's clipboard holds is our own write; a marker of
    /// an earlier clip counts as none (a clipboard manager re-selecting an
    /// entry is a new copy, D6; Android has no Universal Clipboard to bring
    /// one back); then content Oink just wrote; then the shared content rule
    /// (`clipboard_dedupe::duplicate`: format subset, unless deduplication
    /// or receiving is off or the group has a newer clip, D11).
    fn capture_decision(
        &self,
        helper: [u8; 16],
        revision: u64,
        text: Option<String>,
        html: Option<String>,
        fresh: bool,
        marker: Option<ClipId>,
    ) -> Option<(String, Option<String>)> {
        let mut native = self.native.lock().unwrap();
        if !native.native_realtime {
            return None;
        }
        // Rich text only travels with its plain text.
        let html = html.filter(|_| text.is_some());
        let changed = native.capture.observe(helper, revision, text.clone(), html.clone());
        let local = Source::Device(self.me);
        let new = text.as_deref().map(|text| fingerprint(text, html.as_deref()));
        if !changed {
            if native.held.is_none() {
                native.held = new.map(|fingerprint| Held {
                    identity: Identity {
                        fingerprint,
                        source: local,
                    },
                    clip: None,
                });
            }
            return None;
        }
        let own = native.own_write.take();
        let held_clip = native.held.as_ref().and_then(|held| held.clip);
        let rules = dedupe::Rules {
            dedupe: !native.dedupe_off,
            receive: native.automatic,
        };
        let comparison = match (&new, &native.held) {
            _ if marker.is_some() && marker == held_clip => Some("marker"),
            (Some(new), _) if own.as_ref() == Some(new) => Some("own-write"),
            (Some(new), Some(held)) => match dedupe::duplicate(new, &held.identity, rules, !native.newer) {
                Ok(()) => Some("subset"),
                Err(reason) => {
                    tracing::trace!(direction = "local", comparison = ?reason, "clipboard change is a new copy");
                    None
                }
            },
            _ => None,
        };
        if let Some(comparison) = comparison {
            // Sending-side deduplication: the identity stays the original.
            tracing::debug!(
                direction = "local",
                new_source = ?local,
                current_source = ?native.held.as_ref().map(|held| held.identity.source),
                comparison,
                "clipboard duplicate ignored: not announced"
            );
            return None;
        }
        native.held = new.map(|fingerprint| Held {
            identity: Identity {
                fingerprint,
                source: local,
            },
            clip: None,
        });
        // This copy is now the group's newest content.
        native.newer = false;
        (fresh && native.broadcast)
            .then(|| {
                native
                    .capture
                    .text
                    .clone()
                    .map(|text| (text, native.capture.html.clone()))
            })
            .flatten()
    }

    /// A helper-reported change at `now`: the delay until the clipboard is
    /// read (`native_capture_due`).
    fn change_seen_at(&self, now: Instant) -> u64 {
        let deadline = self.native.lock().unwrap().burst.change(now);
        deadline.saturating_duration_since(now).as_millis() as u64
    }

    fn capture_due_at(&self, now: Instant) -> bool {
        self.native.lock().unwrap().burst.due(now)
    }
}

impl FreshClipboardObserver for MobileClipboard {
    fn begin(&self, source: &str) -> Option<u64> {
        let (sequence, ticket) = {
            let mut native = self.native.lock().unwrap();
            native.next_ticket = native.next_ticket.saturating_add(1);
            let sequence = native.next_ticket;
            native.cancel();
            let ticket = (native.automatic && sequence < AUTOMATIC_ID).then(|| {
                let operation = AUTOMATIC_ID | sequence;
                native.ticket = Some((operation, native.revision));
                operation
            });
            (sequence, ticket)
        };
        self.emit(ClipboardEvent::Metadata {
            source: source.into(),
            clock: sequence,
            fresh_copy: true,
            automatic_eligible: true,
        });
        ticket
    }

    /// Received text is always written (deduplication is sending-side only).
    fn complete(&self, ticket: u64, source: &str, text: &str, rich: Option<ClipboardRich>, marker: Option<Vec<u8>>) {
        let received = {
            let mut native = self.native.lock().unwrap();
            if native.ticket != Some((ticket, native.revision)) {
                return;
            }
            let html = rich.as_ref().map(|rich| rich.html.clone());
            native.delivered = parse_device(source).map(|sender| (fingerprint(text, html.as_deref()), sender));
            ReceiveResult {
                operation_id: ticket,
                native_revision: native.revision,
                kind: ReceiveKind::Text,
                source: source.into(),
                text: Some(text.into()),
                files: vec![],
                sensitive: false,
                empty_reason: None,
                marker,
                html,
                rtf: rich.and_then(|rich| rich.rtf),
            }
        };
        self.emit(ClipboardEvent::Received(received));
    }

    fn group_changed(&self, marker: Option<Vec<u8>>) {
        let clip = marker.as_deref().and_then(clipboard_marker::parse).map(|id| id.clip);
        let mut native = self.native.lock().unwrap();
        // The offer of the clip this phone already holds (sent again on a new
        // link) is not a newer one.
        if clip.is_none() || native.held.as_ref().and_then(|held| held.clip) != clip {
            native.newer = true;
        }
    }
}

#[uniffi::export]
impl MobileClipboard {
    pub fn set_native_revision(&self, revision: u64) {
        let mut native = self.native.lock().unwrap();
        if revision > native.revision {
            native.revision = revision;
            native.cancel();
        }
    }

    pub fn set_auto_receive(&self, enabled: bool) {
        let mut native = self.native.lock().unwrap();
        native.automatic = enabled;
        if !enabled {
            native.cancel();
        }
    }

    pub fn validate_received(&self, operation_id: u64) -> bool {
        if operation_id & AUTOMATIC_ID == 0 {
            // Explicit copies are fenced by their native revision at the host.
            return self.transfer.strong_count() != 0;
        }
        let native = self.native.lock().unwrap();
        native.ticket == Some((operation_id, native.revision))
    }

    pub fn cancel_all(&self) {
        self.native.lock().unwrap().cancel();
    }

    pub fn set_broadcast_enabled(&self, enabled: bool) {
        self.native.lock().unwrap().broadcast = enabled;
    }

    pub fn set_native_realtime(&self, enabled: bool) {
        let mut native = self.native.lock().unwrap();
        if native.native_realtime != enabled {
            native.capture = Capture::default();
            native.native_realtime = enabled;
            native.held = None;
            native.newer = false;
            native.burst.clear();
        }
    }

    /// The privileged helper reported a clipboard change. Returns the delay
    /// in milliseconds after which the host calls `native_capture_due` and,
    /// when it answers true, reads the clipboard (`capture_native_text`).
    /// Each change restarts the 250 ms quiet period, capped at 1 s after the
    /// burst's first change, like a PC's announcement debounce: a clipboard
    /// written several times in quick succession is read and announced
    /// once, in its last state.
    pub fn native_change_seen(&self) -> u64 {
        self.change_seen_at(Instant::now())
    }

    /// True once the quiet period of the current burst is over; that read
    /// ends the burst. False for earlier timers of the same burst.
    pub fn native_capture_due(&self) -> bool {
        self.capture_due_at(Instant::now())
    }

    /// `marker`: the Oink clip marker the helper read from the clipboard, if
    /// any (bytes as the host wrote them). `html`: the clip's HTML next to
    /// its plain text (`ClipData.Item.getHtmlText`), if any.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_native_text(
        &self,
        token: Vec<u8>,
        revision: u64,
        text: Option<String>,
        fresh: bool,
        sensitive: bool,
        marker: Option<Vec<u8>>,
        html: Option<String>,
    ) {
        let Ok(helper) = token.try_into() else { return };
        let text = text.filter(|text| text.len() <= MAX_DIRECT_TEXT_BYTES);
        let html = html.filter(|html| !html.is_empty());
        let marker = marker.as_deref().and_then(clipboard_marker::parse).map(|id| id.clip);
        let publish = self.capture_decision(helper, revision, text, html, fresh, marker);
        if let Some(((text, html), transfer)) = publish.zip(self.transfer.upgrade()) {
            let choice = self
                .native
                .lock()
                .unwrap()
                .proxy
                .clone()
                .map_or(ProxyChoice::Automatic, ProxyChoice::Device);
            let _ = transfer.send_share(
                SharePayload {
                    sensitivity: if sensitive {
                        oink_foundation::clipboard::Sensitivity::Concealed
                    } else {
                        oink_foundation::clipboard::Sensitivity::Normal
                    },
                    items: vec![SendItem::text(text, html, None)],
                    origin: SendOrigin::AndroidShare,
                    operation_id: rand::random(),
                    message_id: rand::random(),
                },
                choice,
            );
        }
    }

    /// `marker`: the clip marker written with the text (the received
    /// content's `marker`), if any. `html`: the HTML written with it.
    pub fn native_text_written(
        &self,
        token: Vec<u8>,
        revision: u64,
        text: String,
        sensitive: bool,
        marker: Option<Vec<u8>>,
        html: Option<String>,
    ) {
        let Ok(helper) = token.try_into() else { return };
        let mut native = self.native.lock().unwrap();
        if native.native_realtime {
            let clip = marker.as_deref().and_then(clipboard_marker::parse).map(|id| id.clip);
            let fingerprint = fingerprint(&text, html.as_deref());
            // Diagnostics only: the sender of an automatic write; an explicit
            // copy from history has no recorded sender.
            let source = match native.delivered.take() {
                Some((delivered, sender)) if delivered == fingerprint => Source::Device(sender),
                _ => Source::External,
            };
            native.own_write = Some(fingerprint.clone());
            native.held = (!sensitive).then_some(Held {
                identity: Identity { fingerprint, source },
                clip,
            });
            // The phone now holds what it accepted last.
            native.newer = false;
            // Changes before the write are gone with it (the PC's `written`).
            native.burst.clear();
            native.capture.observe(
                helper,
                revision,
                (!sensitive).then_some(text),
                html.filter(|_| !sensitive),
            );
        }
    }

    pub fn report_publish(&self, operation_id: u64, _success: bool) {
        let mut native = self.native.lock().unwrap();
        if native.ticket.is_some_and(|(id, _)| id == operation_id) {
            native.cancel();
        }
    }
}

fn parse_device(hex: &str) -> Option<DeviceId> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut id = [0; 32];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(DeviceId(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn native_revision_and_newer_remote_notice_revoke_automatic_copy() {
        let clipboard = clipboard();
        clipboard.set_auto_receive(true);
        let old = clipboard.begin("pc").unwrap();
        clipboard.set_native_revision(1);
        clipboard.complete(old, "pc", "superseded by local copy", None, None);
        assert!(!clipboard.validate_received(old));

        let older = clipboard.begin("pc").unwrap();
        let latest = clipboard.begin("pc").unwrap();
        clipboard.complete(older, "pc", "late remote copy", None, None);
        clipboard.complete(latest, "pc", "latest text", None, None);
        let events = clipboard.take_events();
        let received: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ClipboardEvent::Received(value) => Some(value),
                ClipboardEvent::Metadata { .. } => None,
                ClipboardEvent::Progress { .. } => None,
            })
            .collect();
        assert!(matches!(
            received.as_slice(),
            [value] if value.native_revision == 1 && value.text.as_deref() == Some("latest text")
        ));
        assert!(matches!(events.last(), Some(ClipboardEvent::Received(_))));
        clipboard.report_publish(latest, true);
        assert!(!clipboard.validate_received(latest));
    }

    std::thread_local! {
        static EVENTS: std::cell::RefCell<Vec<ClipboardEvent>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    impl MobileClipboard {
        /// Events this test thread's clipboard pushed so far.
        fn take_events(&self) -> Vec<ClipboardEvent> {
            EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
        }
    }

    fn clipboard() -> MobileClipboard {
        let clipboard = MobileClipboard {
            native: Mutex::new(NativeClipboard::default()),
            transfer: Weak::new(),
            me: DeviceId([7; 32]),
            sink: Mutex::new(None),
        };
        clipboard.set_event_sink(Arc::new(|event| EVENTS.with(|events| events.borrow_mut().push(event))));
        clipboard
    }
    const PC: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const OTHER_PC: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    #[test]
    fn helper_boot_baseline_and_stale_revisions_are_not_native_copies() {
        let mut capture = Capture::default();
        assert!(!capture.observe([1; 16], 5, Some("baseline".into()), None));
        assert!(!capture.observe([1; 16], 4, Some("stale".into()), None));
        assert!(capture.observe([1; 16], 6, Some("copy".into()), None));
        assert!(!capture.observe([2; 16], 1, Some("replacement helper".into()), None));
    }

    fn realtime(dedupe: bool) -> MobileClipboard {
        let clipboard = clipboard();
        clipboard.native.lock().unwrap().dedupe_off = !dedupe;
        clipboard.set_native_realtime(true);
        clipboard.set_broadcast_enabled(true);
        clipboard.set_auto_receive(true);
        assert_eq!(decide(&clipboard, 1, "baseline", None, None), None);
        clipboard
    }
    /// A helper report of plain text `text` (with `html`) at `revision`.
    fn decide(
        clipboard: &MobileClipboard,
        revision: u64,
        text: &str,
        html: Option<&str>,
        marker: Option<ClipId>,
    ) -> Option<(String, Option<String>)> {
        clipboard.capture_decision(
            [1; 16],
            revision,
            Some(text.into()),
            html.map(Into::into),
            revision > 1,
            marker,
        )
    }
    fn announced(text: &str) -> Option<(String, Option<String>)> {
        Some((text.into(), None))
    }
    fn received(clipboard: &MobileClipboard) -> Vec<ReceiveResult> {
        clipboard
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                ClipboardEvent::Received(value) => Some(value),
                _ => None,
            })
            .collect()
    }
    fn offered(clipboard: &MobileClipboard, source: &str, text: &str) -> bool {
        let ticket = clipboard.begin(source).unwrap();
        clipboard.complete(ticket, source, text, None, None);
        received(clipboard)
            .iter()
            .any(|value| value.text.as_deref() == Some(text))
    }
    fn write(clipboard: &MobileClipboard, source: &str, text: &str, revision: u64) {
        assert!(offered(clipboard, source, text), "received text is always written");
        clipboard.native_text_written([1; 16].to_vec(), revision, text.into(), false, None, None);
    }

    #[test]
    fn deduplication_on_announces_the_same_text_once_off_every_time() {
        let on = realtime(true);
        assert_eq!(decide(&on, 2, "again", None, None), announced("again"));
        assert_eq!(decide(&on, 3, "again\r\n", None, None), None);
        // A prefix is a new copy.
        assert_eq!(decide(&on, 4, "aga", None, None), announced("aga"));
        let off = realtime(false);
        assert_eq!(decide(&off, 2, "again", None, None), announced("again"));
        assert_eq!(decide(&off, 3, "again", None, None), announced("again"));
    }

    #[test]
    fn our_own_write_coming_back_is_never_announced() {
        for dedupe in [true, false] {
            let clipboard = realtime(dedupe);
            write(&clipboard, PC, "from pc\n", 2);
            assert_eq!(decide(&clipboard, 3, "from pc\n", None, None), None);
            // With deduplication on, a synchronizer's CRLF rewrite too.
            assert_eq!(decide(&clipboard, 4, "from pc\r\n", None, None).is_none(), dedupe);
        }
    }

    #[test]
    fn received_text_is_always_written() {
        let clipboard = realtime(true);
        assert_eq!(
            decide(&clipboard, 2, "copied here", None, None),
            announced("copied here")
        );
        assert!(offered(&clipboard, PC, "copied here"));
        assert!(offered(&clipboard, OTHER_PC, "copied here"));
        assert!(offered(&clipboard, PC, "copied here"));
    }

    fn marker(seq: u64) -> Vec<u8> {
        clipboard_marker::encode(oink_foundation::content::OfferId {
            clip: clip(seq),
            generation: 2,
        })
        .unwrap()
    }
    fn clip(seq: u64) -> ClipId {
        ClipId {
            origin: DeviceId([1; 32]),
            seq,
        }
    }

    #[test]
    fn received_content_carries_the_offer_marker_for_the_host_write() {
        let clipboard = clipboard();
        clipboard.set_auto_receive(true);
        let ticket = clipboard.begin(PC).unwrap();
        clipboard.complete(ticket, PC, "marked", None, Some(marker(5)));
        let [value] = received(&clipboard).try_into().unwrap();
        assert_eq!(value.marker, Some(marker(5)));
        assert_eq!(
            clipboard_marker::parse(value.marker.as_deref().unwrap()).map(|id| id.clip),
            Some(clip(5))
        );
        // A direct share has no clip identity.
        let ticket = clipboard.begin(PC).unwrap();
        clipboard.complete(ticket, PC, "shared", None, None);
        let [value] = received(&clipboard).try_into().unwrap();
        assert_eq!(value.marker, None);
    }

    #[test]
    fn helper_reports_carrying_our_marker_are_never_announced() {
        for dedupe in [true, false] {
            let clipboard = realtime(dedupe);
            let ticket = clipboard.begin(PC).unwrap();
            clipboard.complete(ticket, PC, "from pc", None, Some(marker(5)));
            assert_eq!(received(&clipboard).len(), 1);
            clipboard.native_text_written([1; 16].to_vec(), 2, "from pc".into(), false, Some(marker(5)), None);
            // Every report of the marked clipboard is our own write, not only
            // the first one after the write.
            for revision in 3..5 {
                clipboard.capture_native_text(
                    [1; 16].to_vec(),
                    revision,
                    Some("from pc".into()),
                    true,
                    false,
                    Some(marker(5)),
                    None,
                );
                assert_eq!(decide(&clipboard, revision + 10, "from pc", None, Some(clip(5))), None);
            }
            // A marker this phone did not write is ignored: the content rules
            // apply (a new text is announced).
            assert_eq!(decide(&clipboard, 20, "new", None, Some(clip(6))), announced("new"));
        }
    }

    #[test]
    fn without_realtime_capture_the_phone_never_dedupes() {
        let clipboard = clipboard();
        clipboard.set_auto_receive(true);
        assert!(offered(&clipboard, PC, "text"));
        assert!(offered(&clipboard, OTHER_PC, "text"));
    }

    /// 2026-11-12 (privileged auto-send): rich text Oink just wrote (marked
    /// or not) is never announced; the same rich text copied again is not
    /// announced either, but other formatting or plain text added later is
    /// compared like on a PC.
    #[test]
    fn rich_text_oink_wrote_is_not_announced_and_a_repeat_is_not_resent() {
        let clipboard = realtime(true);
        let ticket = clipboard.begin(PC).unwrap();
        let rich = ClipboardRich {
            html: "<b>hi</b>".into(),
            rtf: Some(r"{\rtf1 \b hi\b0}".into()),
        };
        clipboard.complete(ticket, PC, "hi", Some(rich), Some(marker(5)));
        let [value] = received(&clipboard).try_into().unwrap();
        assert_eq!(value.html.as_deref(), Some("<b>hi</b>"));
        assert_eq!(value.rtf.as_deref(), Some(r"{\rtf1 \b hi\b0}"));
        clipboard.native_text_written(
            [1; 16].to_vec(),
            2,
            "hi".into(),
            false,
            Some(marker(5)),
            Some("<b>hi</b>".into()),
        );
        // Read back with its marker, and after an app dropped the marker.
        assert_eq!(decide(&clipboard, 3, "hi", Some("<b>hi</b>"), Some(clip(5))), None);
        assert_eq!(decide(&clipboard, 4, "hi", Some("<b>hi</b>"), None), None);
        // Only its plain text: a format subset, not announced.
        assert_eq!(decide(&clipboard, 5, "hi", None, None), None);
        // Other formatting is a new copy, with its HTML.
        assert_eq!(
            decide(&clipboard, 6, "hi", Some("<i>hi</i>"), None),
            Some(("hi".into(), Some("<i>hi</i>".into())))
        );
        // Copying that again is not.
        assert_eq!(decide(&clipboard, 7, "hi", Some("<i>hi</i>"), None), None);
    }

    /// D11: the same content is a new copy once the group has a newer clip
    /// this phone did not write, and for a phone that does not receive.
    #[test]
    fn a_repeat_is_announced_when_the_group_moved_on_or_receiving_is_off() {
        let clipboard = realtime(true);
        assert_eq!(decide(&clipboard, 2, "mine", None, None), announced("mine"));
        assert_eq!(decide(&clipboard, 3, "mine", None, None), None);
        // A newer offer arrived (not written here).
        clipboard.group_changed(Some(marker(9)));
        assert_eq!(decide(&clipboard, 4, "mine", None, None), announced("mine"));
        assert_eq!(decide(&clipboard, 5, "mine", None, None), None);
        // A broadcast from another device too.
        clipboard.group_changed(None);
        assert_eq!(decide(&clipboard, 6, "mine", None, None), announced("mine"));
        // The offer of what this phone holds (sent again) is not newer.
        let ticket = clipboard.begin(PC).unwrap();
        clipboard.complete(ticket, PC, "from pc", None, Some(marker(5)));
        clipboard.native_text_written([1; 16].to_vec(), 7, "from pc".into(), false, Some(marker(5)), None);
        assert_eq!(decide(&clipboard, 8, "from pc", None, None), None, "own write");
        clipboard.group_changed(Some(marker(5)));
        assert_eq!(decide(&clipboard, 9, "from pc", None, None), None);
        // Receiving off: no content deduplication at all.
        clipboard.set_auto_receive(false);
        assert_eq!(decide(&clipboard, 10, "mine", None, None), announced("mine"));
        assert_eq!(decide(&clipboard, 11, "mine", None, None), announced("mine"));
    }

    /// D6: a clipboard manager putting back an entry Oink wrote earlier
    /// (with that clip's marker) is a new copy.
    #[test]
    fn an_earlier_clip_marker_is_a_new_copy() {
        let clipboard = realtime(true);
        for (seq, text, revision) in [(5, "first", 2), (6, "second", 3)] {
            let ticket = clipboard.begin(PC).unwrap();
            clipboard.complete(ticket, PC, text, None, Some(marker(seq)));
            clipboard.native_text_written([1; 16].to_vec(), revision, text.into(), false, Some(marker(seq)), None);
        }
        assert_eq!(decide(&clipboard, 4, "first", None, Some(clip(5))), announced("first"));
    }

    /// The announcement debounce: a burst of helper changes is read once,
    /// 250 ms after its last change and at most 1 s after its first.
    #[test]
    fn a_burst_of_changes_is_read_once() {
        let clipboard = realtime(true);
        let at = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(clipboard.change_seen_at(at), 250);
        assert_eq!(clipboard.change_seen_at(at + ms(100)), 250);
        assert_eq!(clipboard.change_seen_at(at + ms(180)), 250);
        // Earlier timers find the burst still quiet-pending.
        assert!(!clipboard.capture_due_at(at + ms(250)));
        assert!(!clipboard.capture_due_at(at + ms(350)));
        assert!(clipboard.capture_due_at(at + ms(430)));
        assert!(!clipboard.capture_due_at(at + ms(431)), "one read per burst");
        // A clipboard that keeps changing is read at the cap.
        let start = at + ms(5000);
        let mut delay = 0;
        for step in 0..5 {
            delay = clipboard.change_seen_at(start + ms(step * 200));
        }
        assert_eq!(delay, 200, "capped at 1 s after the first change");
        assert!(!clipboard.capture_due_at(start + ms(999)));
        assert!(clipboard.capture_due_at(start + ms(1000)));
        // Oink's own write drops a pending burst.
        clipboard.change_seen_at(start + ms(2000));
        clipboard.native_text_written([1; 16].to_vec(), 50, "written".into(), false, None, None);
        assert!(!clipboard.capture_due_at(start + ms(3000)));
    }
}
