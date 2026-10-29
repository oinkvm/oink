use super::*;
use crate::streams::{clipboard::ClipboardStreams, lifecycle_tests::Fixture};
use oink_clipboard::content::{self, namespace::Namespace};
use oink_clipboard::model::*;
use std::io::Read;

struct CapturedPromise(Arc<dyn Namespace>);
impl Namespace for CapturedPromise {
    fn roots(&self) -> Vec<FileNode> {
        self.0.roots()
    }
    fn one_shot_roots(&self) -> Vec<NodeId> {
        self.roots().iter().map(|root| root.id).collect()
    }
    fn native_file_metadata(&self) -> content::native_metadata::NativeFileMetadata {
        content::native_metadata::NativeFileMetadata {
            windows: vec![],
            promises: self
                .roots()
                .iter()
                .map(|root| content::native_metadata::MacPromiseGroup {
                    anchor: NodeId(root.id.0 + 100),
                    roots: vec![root.id],
                    indices: vec![0],
                    controls: vec![],
                })
                .collect(),
        }
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.0.list(node, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.0.select(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<oink_clipboard::FileReader> {
        self.0.open(node, version, offset)
    }
}

#[test]
fn returned_promise_catalogs_reopen_self_without_a_network_peer() {
    crate::test_watchdog::run(
        module_path!(),
        "returned_promise_catalogs_reopen_self_without_a_network_peer",
        Some(4),
        async {
            let fixture = Fixture::<ClipboardStreams>::start().await;
            let (source, config) = fixture.context(0);
            let me = source.id();
            assert!(source.links().by_peer(me).is_none());
            let grants = config.source_grants().unwrap();
            let links = FileLinks::new(source.clone(), StreamKind::Clipboard);
            links.pause_changed(true);
            links.set_local_source(me, Arc::new(content::Registry::with_source_grants(grants.clone())));
            assert!(links.clone().bind_persistent(me).unwrap().open(me).await.is_err());
            links.pause_changed(false);
            let connector = content::linked::Connector::default();
            let _connection = connector.connect(links.clone(), tokio::runtime::Handle::current());
            let root = tempfile::tempdir().unwrap();
            let mut last_descriptor = None;
            // These are the independently captured files after source fulfilment,
            // not fake AppKit promises. The native sender/writer cases are separate
            // device tests. Exercise the catalog's restored persistent read path.
            for (sequence, mode) in ["fast", "slow", "progressive", "folder", "mixed-image"]
                .into_iter()
                .enumerate()
            {
                let directory = root.path().join(mode);
                std::fs::create_dir(&directory).unwrap();
                let folder = mode == "folder";
                let count = if mode == "mixed-image" { 1 } else { 2 };
                let mut paths = Vec::new();
                let mut expected = Vec::new();
                for index in 0..count {
                    let bytes = if mode == "slow" || mode == "progressive" && index == 1 {
                        vec![b'A'; 40]
                    } else {
                        vec![b'A' + index as u8; 1024 * (index + 1)]
                    };
                    let path = directory.join(format!("promise-{index}"));
                    if folder {
                        std::fs::create_dir_all(path.join("empty")).unwrap();
                        std::fs::write(path.join("child.bin"), &bytes).unwrap();
                    } else {
                        std::fs::write(&path, &bytes).unwrap();
                    }
                    paths.push((NodeId(index as u64 + 1), path));
                    expected.push(bytes);
                }
                let files = CapturedPromise(content::namespace::Local::new(paths).unwrap());
                let ids: Vec<_> = files.roots().iter().map(|entry| entry.id).collect();
                let token = grants.issue(me, &files, &ids).unwrap();
                let durable = grants.open(me, token).unwrap();
                let native = durable.native_file_metadata();
                assert_eq!(native.promises.len(), count);
                let descriptor = content::linked::Descriptor {
                    offer: OfferId {
                        clip: ClipId {
                            origin: me,
                            seq: sequence as u64 + 1,
                        },
                        generation: 1,
                    },
                    token,
                    provisional: false,
                    roots: durable.roots(),
                    fulfilled: grants.fulfilled_roots(me, token).unwrap(),
                    native: native.clone(),
                };
                drop(files);
                drop(durable);
                std::fs::remove_dir_all(&directory).unwrap();
                // Recover metadata as File Provider does after persist_link. No
                // retained transient LocalLink or original source provider remains.
                let restored = connector.restore(descriptor.clone()).unwrap();
                last_descriptor = Some(descriptor.clone());
                assert_eq!(restored.native_file_metadata(), native);
                tokio::task::spawn_blocking(move || {
                    for (index, root) in restored.roots().into_iter().enumerate() {
                        let node = if folder {
                            let children = restored.list(root.id, None).unwrap();
                            let empty = children.entries.iter().find(|entry| entry.name == "empty").unwrap();
                            assert!(restored.list(empty.id, None).unwrap().entries.is_empty());
                            children
                                .entries
                                .into_iter()
                                .find(|entry| entry.name == "child.bin")
                                .unwrap()
                                .id
                        } else {
                            root.id
                        };
                        let entry = restored.select(node).unwrap();
                        let mut bytes = Vec::new();
                        restored
                            .open(node, entry.version, 0)
                            .unwrap()
                            .read_to_end(&mut bytes)
                            .unwrap();
                        assert_eq!(bytes, expected[index], "{mode} item {index}");
                    }
                })
                .await
                .unwrap();
                assert_eq!(descriptor.fulfilled.len(), count);
                // Every stream/page was sent; no Stored ACK retains F1 bytes.
            }
            let mut invalid = last_descriptor.unwrap();
            invalid.token = content::grants::LinkToken([0; 32]);
            let invalid = connector.restore(invalid).unwrap();
            tokio::task::spawn_blocking(move || {
                assert!(
                    invalid.select(invalid.roots()[0].id).is_err(),
                    "local reads still authenticate the token"
                );
            })
            .await
            .unwrap();
            // Pause retires old operations permanently, while resume may reopen
            // the same durable credentials. No self peer/transport is fabricated.
            let persistent = links.clone().bind_persistent(me).unwrap();
            let mut channel = persistent.open(me).await.unwrap();
            assert!(
                channel
                    .send(&ContentMsg::CloudLinkRequest {
                        offer: OfferId {
                            clip: ClipId { origin: me, seq: 99 },
                            generation: 1
                        },
                        roots: vec![NodeId(1)],
                        demand: Demand::Clipboard,
                    })
                    .await
                    .is_err(),
                "the persistent self route cannot mint authority"
            );
            let valid = channel.effect_validity();
            links.pause_changed(true);
            assert!(!valid());
            assert!(persistent.open(me).await.is_err());
            links.pause_changed(false);
            assert!(!valid());
            assert!(persistent.open(me).await.is_ok());
            drop(channel);
            let channel = persistent.open(me).await.unwrap();
            let valid = channel.effect_validity();
            links.set_local_source(me, Arc::new(content::Registry::with_source_grants(grants)));
            assert!(!valid(), "replacing the source owner retires its old operations");
            assert!(persistent.open(me).await.is_ok());
            drop(channel);
            assert!(source.links().by_peer(me).is_none());
            fixture.stop().await;
        },
    );
}
