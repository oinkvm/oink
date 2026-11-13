//! Announced real files read by a phone by their recorded paths
//! (clipboard.md「手机的文件保留」).
use super::*;
use crate::content::namespace::{Local, Namespace};
use crate::model::NodeId;
use crate::sync::files::BoxFuture;
use std::{fs, path::Path};
const PHONE: DeviceId = DeviceId([5; 32]);

#[derive(Default)]
struct Replies(Vec<ContentMsg>);
impl BulkChannel for Replies {
    fn send<'a>(&'a mut self, message: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.0.push(message.clone());
            Ok(())
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async { Ok(None) })
    }
    fn finish(&mut self) {}
}

/// A clipboard registry recording into `data`, as the agent's.
fn recorded(data: &Path) -> Arc<Registry> {
    let registry = Arc::new(Registry::default());
    registry.set_announced_files(Arc::new(announced::AnnouncedFiles::new(
        data,
        crate::policy::SharingPolicy::new(true),
    )));
    registry
}

/// A local copy of `path`; its offer and the root's announced version.
fn announce(registry: &Registry, path: &Path, seq: u64) -> (SourceLease, OfferId, u64) {
    let files = Local::clipboard(vec![(NodeId(1), path.to_owned())]).unwrap();
    let version = files.roots()[0].version;
    let mut id = super::super::tests::fixture().inventory().id;
    id.clip.seq = seq;
    id.generation = seq;
    let provider = identified(with_files(Arc::new(super::super::tests::fixture()), files), id);
    (registry.register_clipboard(provider).unwrap(), id, version)
}

async fn read(registry: &Registry, offer: OfferId, version: u64) -> Result<Vec<u8>, Unavailable> {
    let mut replies = Replies::default();
    let request = ContentMsg::ReadFile {
        offer,
        node: NodeId(1),
        version,
        offset: 0,
        demand: Demand::Clipboard,
    };
    registry.serve_namespace(PHONE, request, &mut replies).await.unwrap();
    let mut bytes = Vec::new();
    for message in replies.0 {
        match message {
            ContentMsg::FileBlock { bytes: block, .. } => bytes.extend(block),
            ContentMsg::FileEnd { .. } => return Ok(bytes),
            ContentMsg::FileFailed { reason, .. } => return Err(reason),
            other => panic!("unexpected reply {other:?}"),
        }
    }
    panic!("no terminal reply")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_reads_an_announced_file_while_its_path_holds_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.pdf");
    fs::write(&path, b"original").unwrap();
    let registry = recorded(dir.path());
    let (lease, offer, version) = announce(&registry, &path, 1);
    // The clipboard moved on to another copy.
    drop(lease);
    let other = dir.path().join("other.txt");
    fs::write(&other, b"other").unwrap();
    let (_next, next, _) = announce(&registry, &other, 2);
    assert_ne!(next, offer);
    assert_eq!(read(&registry, offer, version).await, Ok(b"original".to_vec()));
    // Sharing paused and resumed: every registration was cancelled.
    registry.cancel_all();
    assert_eq!(read(&registry, offer, version).await, Ok(b"original".to_vec()));
    // Oink restarted: only the record on disk is left.
    let restarted = recorded(dir.path());
    assert_eq!(read(&restarted, offer, version).await, Ok(b"original".to_vec()));
    // An offer that was never announced is unknown, as before.
    let mut never = offer;
    never.clip.seq = 99;
    assert_eq!(read(&restarted, never, version).await, Err(Unavailable::NoSuchItem));
    // Edited after it was announced: no longer the announced version.
    fs::write(&path, b"edited afterwards").unwrap();
    assert_eq!(
        read(&recorded(dir.path()), offer, version).await,
        Err(Unavailable::Superseded)
    );
    // Moved away: gone for good.
    fs::rename(&path, dir.path().join("moved.pdf")).unwrap();
    assert_eq!(
        read(&recorded(dir.path()), offer, version).await,
        Err(Unavailable::NoSuchItem)
    );
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn an_announced_image_file_has_a_thumbnail_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("photo.png");
    fs::write(&path, super::super::tests::test_png(800, 600)).unwrap();
    let registry = recorded(dir.path());
    let (lease, offer, version) = announce(&registry, &path, 1);
    drop(lease);
    registry.cancel_all();
    let restarted = recorded(dir.path());
    let target = crate::retained::Target::File { node: 1, version };
    let ThumbnailOutcome::Ready(thumbnail) = restarted.thumbnail(offer, target, 512, 128 * 1024, None) else {
        panic!("thumbnail of the recorded file");
    };
    assert_eq!((thumbnail.width, thumbnail.height), (512, 384));
    fs::remove_file(&path).unwrap();
    assert_eq!(
        recorded(dir.path()).thumbnail(offer, target, 512, 128 * 1024, None),
        ThumbnailOutcome::Expired
    );
}

#[test]
fn only_real_clipboard_files_are_recorded_and_only_by_a_recording_registry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"x").unwrap();
    // A registry without a record store (drag and drop) writes nothing.
    let (_lease, _, _) = announce(&Registry::default(), &path, 1);
    assert!(!dir.path().join(announced::DIR).exists());
    let files = Local::clipboard(vec![(NodeId(1), path.clone())]).unwrap();
    assert_eq!(files.announced_roots().len(), 1);
    // A namespace that is not the real clipboard records nothing.
    let files = Local::new(vec![(NodeId(1), path)]).unwrap();
    assert!(files.announced_roots().is_empty());
}
