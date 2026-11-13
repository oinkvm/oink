//! 「清理传输记录」 (Settings › 传输, user 2026-11-11).

use super::{MobileV2Link, MobileV2Session, TransferEvent};
use crate::StoreError;
use std::collections::HashSet;

impl<L: MobileV2Link> MobileV2Session<L> {
    /// Deletes every finished record of this phone with its files
    /// (`TransferStore::clear_history`) and reports them as `Removed`.
    /// Transfers still moving keep their records and end normally.
    pub fn clear_history(&self) -> Result<Vec<i64>, StoreError> {
        let removed = {
            // The receive map stays locked while the store deletes: a running
            // phone-to-phone receive records each item in the store first and
            // in the map after it, so it cannot record and finish an item in
            // between. (Nothing takes the map lock while holding the store.)
            let direct = self.direct.lock().expect("direct receive mutex poisoned");
            let receiving: HashSet<i64> = direct
                .values()
                .flat_map(|receive| receive.records.iter().copied())
                .collect();
            self.with_store(|store| store.clear_history(&receiving))?
        };
        if !removed.is_empty() {
            self.transport.emit(TransferEvent::Removed {
                record_ids: removed.clone(),
            });
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ClipboardLowOperation, MobileV2Identity, MobileV2TransportError, boot, direct::DirectReceive};
    use super::*;
    use crate::{
        AttachmentKind, IncomingAnnouncement, SourceAvailability, SourceState, TransferStore, store::OutgoingItem,
    };
    use oink_foundation::{DeviceId, session::Boot};
    use std::sync::{Arc, Mutex};

    struct NoLink;
    impl MobileV2Link for NoLink {
        fn send_frame(&self, _peer_id: &str, _frame: Vec<u8>) -> Result<(), MobileV2TransportError> {
            Ok(())
        }
        fn send_low(&self, _peer_id: &str, _operation: ClipboardLowOperation) -> Result<(), MobileV2TransportError> {
            Ok(())
        }
    }

    fn incoming(id: &str, n: u8) -> IncomingAnnouncement {
        IncomingAnnouncement {
            sender_id: "peer".into(),
            proxy_id: None,
            content_id: id.into(),
            kind: AttachmentKind::File,
            display_name: format!("{id}.bin"),
            mime: None,
            size: 4,
            text: None,
            rich_text_format: None,
            source: SourceState {
                source_id: "peer".into(),
                availability: SourceAvailability::Ready,
            },
            operation_id: [n; 16],
            message_id: [n + 1; 16],
            item_id: 1,
            representation_id: 1,
            sensitive: false,
        }
    }

    /// A finished receive, an own share still sending, and the finished
    /// first item of a phone-to-phone receive still running: only the first
    /// goes, with its file, and the host hears it as `Removed`.
    #[test]
    fn clearing_removes_finished_records_and_keeps_running_transfers() {
        let dir = tempfile::tempdir().unwrap();
        let store = TransferStore::open(dir.path().join("db"), dir.path().join("files")).unwrap();
        let session = MobileV2Session::new(
            store,
            MobileV2Identity::from_parts([1; 32], [2; 16], [3; 16]),
            Arc::new(NoLink),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        session.set_event_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));

        let receive = |store: &mut TransferStore, id: &str, n: u8| {
            let record = store.insert_announcement(i64::from(n), incoming(id, n)).unwrap();
            let path = store.begin_download(record.id).unwrap();
            store.append_download(record.id, 0, b"body").unwrap();
            store.complete_download(record.id).unwrap();
            (record.id, path)
        };
        let ((finished, finished_path), sending, (direct_item, direct_path)) = session.with_store(|store| {
            let finished = receive(store, "finished", 1);
            let sending = store
                .insert_outgoing(
                    2,
                    vec![OutgoingItem {
                        sender_id: "self".into(),
                        content_id: "sending".into(),
                        kind: AttachmentKind::Text,
                        display_name: "text".into(),
                        mime: None,
                        size: 2,
                        text: Some("hi".into()),
                        rich_text_format: None,
                        local_path: None,
                        operation_id: [2; 16],
                        message_id: [3; 16],
                        item_id: 1,
                        image_size: None,
                        rich: None,
                    }],
                )
                .unwrap()[0]
                .id;
            let direct_item = receive(store, "direct", 3);
            (finished, sending, direct_item)
        });
        let source = boot(Boot {
            device_id: DeviceId([4; 32]),
            nonce: [5; 16],
        });
        session.direct.lock().unwrap().insert(
            ("04".repeat(32), [3; 16]),
            DirectReceive::running(source, vec![direct_item]),
        );

        assert_eq!(session.clear_history().unwrap(), vec![finished]);
        assert_eq!(
            *events.lock().unwrap(),
            vec![TransferEvent::Removed {
                record_ids: vec![finished]
            }]
        );
        assert!(!finished_path.exists() && direct_path.exists());
        session.with_store(|store| {
            assert!(store.by_id(finished).unwrap().is_none());
            assert!(store.by_id(sending).unwrap().is_some());
            assert!(store.by_id(direct_item).unwrap().is_some());
        });

        // Nothing finished is left: no event.
        events.lock().unwrap().clear();
        assert!(session.clear_history().unwrap().is_empty());
        assert!(events.lock().unwrap().is_empty());
    }
}
