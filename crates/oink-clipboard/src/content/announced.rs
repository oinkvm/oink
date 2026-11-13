//! Real files this device announced on its clipboard, recorded by path and
//! disk identity, so a phone can fetch them for as long as the original path
//! holds the same file: after the clipboard moved on, after sharing was
//! paused, after Oink restarted (docs/design/clipboard.md「手机的文件保留」).
//! Metadata only; file bytes are never copied. Phone uploads use the separate
//! `tracked_file` / `Local::received_from` identity-following references; this
//! store deliberately keeps the PC's own files bound to their announced paths.
//!
//! The live registration serves while it exists. Without it, a phone's read
//! is served by a namespace rebuilt over the recorded roots. A root whose path
//! is gone, or now names another file (another volume and inode, or file
//! index), is left out; a record left without roots is removed. Each read is
//! still bound to the version the phone was given (size, modification and
//! creation time), so a file edited after it was announced is reported as
//! superseded and never served as the announced version.
use super::{
    Unavailable,
    namespace::{Local, Namespace, local::inspect},
    unavailable,
};
use crate::model::NodeId;
use oink_foundation::content::OfferId;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Directory below the agent data root: metadata the OS never purges and
/// "清理缓存" does not clear (it holds no cached bytes).
pub const DIR: &str = "announced-files";
/// Offers recorded; beyond it the oldest record goes. A record is a few
/// hundred bytes per copied root.
pub const MAX_OFFERS: usize = 4096;
/// Rebuilt namespaces kept in memory, so the node IDs a phone got from a
/// folder listing stay the same for its following reads.
const RESTORED: usize = 64;
/// Records written between two prunings. Recording runs on the clipboard
/// owner before the offer is announced, so it stays a small unsynced write;
/// the directory is scanned only now and then (the bound is exceeded by less
/// than this meanwhile).
const PRUNE_EVERY: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u8,
    roots: Vec<Root>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    node: u64,
    path: PathBuf,
    identity: [u64; 2],
}

pub struct AnnouncedFiles {
    root: PathBuf,
    policy: crate::policy::SharingPolicy,
    max_offers: usize,
    prune_every: usize,
    recorded: AtomicUsize,
    restored: Mutex<VecDeque<(OfferId, Arc<dyn Namespace>)>>,
}
impl std::fmt::Debug for AnnouncedFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnouncedFiles").finish_non_exhaustive()
    }
}

impl AnnouncedFiles {
    /// The record below `data_dir` (the agent data root). Rebuilt namespaces
    /// read under `policy`, like the live clipboard source.
    pub fn new(data_dir: &Path, policy: crate::policy::SharingPolicy) -> Self {
        Self {
            root: data_dir.join(DIR),
            policy,
            max_offers: MAX_OFFERS,
            prune_every: PRUNE_EVERY,
            recorded: AtomicUsize::new(0),
            restored: Mutex::default(),
        }
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn with_max_offers(mut self, max_offers: usize) -> Self {
        self.max_offers = max_offers;
        self.prune_every = 1;
        self
    }

    pub(crate) fn forget(&self, offer: OfferId) {
        self.restored.lock().unwrap().retain(|(id, _)| *id != offer);
        let _ = std::fs::remove_file(self.root.join(Self::name(offer)));
    }
    fn name(offer: OfferId) -> String {
        let origin: String = offer.clip.origin.0.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("{origin}-{}-{}.json", offer.clip.seq, offer.generation)
    }

    /// Records the real-file roots of an announced offer (nothing when it has
    /// none), and now and then drops the oldest records beyond the bound.
    /// Not synced: a record lost in a crash leaves its files readable only
    /// while their registration exists, as before records existed.
    pub(super) fn record(&self, offer: OfferId, roots: Vec<(NodeId, PathBuf, [u64; 2])>) -> io::Result<()> {
        if roots.is_empty() {
            return Ok(());
        }
        let record = Record {
            schema: 1,
            roots: roots
                .into_iter()
                .map(|(node, path, identity)| Root {
                    node: node.0,
                    path,
                    identity,
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&record).map_err(io::Error::other)?;
        fs::create_dir_all(&self.root)?;
        let name = Self::name(offer);
        let temporary = self.root.join(format!(".{name}.{}", std::process::id()));
        fs::write(&temporary, &bytes)
            .and_then(|()| fs::rename(&temporary, self.root.join(&name)))
            .inspect_err(|_| {
                let _ = fs::remove_file(&temporary);
            })?;
        if self
            .recorded
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.prune_every)
        {
            self.prune();
        }
        Ok(())
    }

    /// Oldest first by the time a record was (re)written: announcing the same
    /// offer again (sharing resumed) renews it.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        let mut records: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".json") && !name.starts_with('.'))
            })
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .collect();
        if records.len() <= self.max_offers {
            return;
        }
        records.sort();
        let excess = records.len() - self.max_offers;
        for (_, path) in records.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }

    /// The namespace serving `offer`'s recorded files, or `None` when there is
    /// no record or none of its files is still at its path. Blocking: reads
    /// the record and inspects each root.
    pub(super) fn namespace(&self, offer: OfferId) -> io::Result<Option<Arc<dyn Namespace>>> {
        if let Some(files) = self.cached(offer) {
            return Ok(Some(files));
        }
        let path = self.root.join(Self::name(offer));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(record) = serde_json::from_slice::<Record>(&bytes)
            .ok()
            .filter(|record| record.schema == 1)
        else {
            // Written whole by an atomic rename; an unreadable record can
            // never become readable.
            tracing::warn!("unreadable announced-files record removed");
            let _ = fs::remove_file(&path);
            return Ok(None);
        };
        let roots: Vec<(NodeId, PathBuf)> = record
            .roots
            .into_iter()
            .filter(|root| inspect(&root.path).is_ok_and(|current| current.identity == root.identity))
            .map(|root| (NodeId(root.node), root.path))
            .collect();
        let local = if roots.is_empty() {
            Err(unavailable(Unavailable::NoSuchItem))
        } else {
            Local::clipboard(roots)
        };
        let local = match local {
            Ok(local) => local,
            Err(error) if super::reason(&error) == Unavailable::NoSuchItem => {
                // Every file left its path or was replaced by another one.
                let _ = fs::remove_file(&path);
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let files = self.policy.files(local);
        let mut restored = self.restored.lock().unwrap();
        // A concurrent read may have rebuilt it meanwhile: keep one, so both
        // readers see the same node IDs.
        if let Some((_, existing)) = restored.iter().find(|(id, _)| *id == offer) {
            return Ok(Some(existing.clone()));
        }
        if restored.len() == RESTORED {
            restored.pop_front();
        }
        restored.push_back((offer, files.clone()));
        Ok(Some(files))
    }

    fn cached(&self, offer: OfferId) -> Option<Arc<dyn Namespace>> {
        self.restored
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| *id == offer)
            .map(|(_, files)| files.clone())
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::{FileReader, content::read_all};
    use oink_foundation::{DeviceId, content::ClipId};

    fn offer(seq: u64) -> OfferId {
        OfferId {
            clip: ClipId {
                origin: DeviceId([4; 32]),
                seq,
            },
            generation: seq,
        }
    }

    fn store(dir: &Path) -> AnnouncedFiles {
        AnnouncedFiles::new(dir, crate::policy::SharingPolicy::new(true))
    }

    fn announce(store: &AnnouncedFiles, seq: u64, roots: Vec<(NodeId, PathBuf)>) -> Arc<Local> {
        let files = Local::clipboard(roots).unwrap();
        store.record(offer(seq), files.announced_roots()).unwrap();
        files
    }

    fn read(files: &dyn Namespace, node: NodeId, version: u64) -> io::Result<Vec<u8>> {
        let mut reader: FileReader = files.open(node, version, 0)?;
        read_all(&mut reader)
    }

    #[test]
    fn a_file_is_served_by_its_path_after_a_restart_while_it_is_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photo.jpg");
        fs::write(&file, b"original").unwrap();
        let version = {
            let announced = store(dir.path());
            let files = announce(&announced, 1, vec![(NodeId(1), file.clone())]);
            files.roots()[0].version
        };
        // A new process: only the record on disk remains.
        let restarted = store(dir.path());
        let files = restarted.namespace(offer(1)).unwrap().expect("recorded");
        assert_eq!(files.roots()[0].id, NodeId(1));
        assert_eq!(read(files.as_ref(), NodeId(1), version).unwrap(), b"original");
        assert!(restarted.namespace(offer(2)).unwrap().is_none(), "never announced");
    }

    #[test]
    fn a_moved_or_replaced_file_is_gone_and_its_record_removed() {
        let dir = tempfile::tempdir().unwrap();
        let moved = dir.path().join("moved.txt");
        let replaced = dir.path().join("replaced.txt");
        fs::write(&moved, b"a").unwrap();
        fs::write(&replaced, b"b").unwrap();
        let announced = store(dir.path());
        announce(&announced, 1, vec![(NodeId(1), moved.clone())]);
        announce(&announced, 2, vec![(NodeId(1), replaced.clone())]);
        fs::rename(&moved, dir.path().join("elsewhere.txt")).unwrap();
        // Same path and bytes, another file: created while the original still
        // existed, so it has another inode / file index, then moved over it.
        let other = dir.path().join("other.txt");
        fs::write(&other, b"b").unwrap();
        fs::rename(&other, &replaced).unwrap();
        let restarted = store(dir.path());
        assert!(restarted.namespace(offer(1)).unwrap().is_none());
        assert!(restarted.namespace(offer(2)).unwrap().is_none());
        assert!(!restarted.root.join(AnnouncedFiles::name(offer(1))).exists());
        assert!(!restarted.root.join(AnnouncedFiles::name(offer(2))).exists());
    }

    #[test]
    fn an_edited_file_is_superseded_not_served_as_the_announced_version() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        fs::write(&file, b"announced").unwrap();
        let announced = store(dir.path());
        let version = announce(&announced, 1, vec![(NodeId(1), file.clone())]).roots()[0].version;
        fs::write(&file, b"edited afterwards").unwrap();
        let files = store(dir.path()).namespace(offer(1)).unwrap().expect("same file");
        assert_eq!(
            super::super::reason(&read(files.as_ref(), NodeId(1), version).unwrap_err()),
            Unavailable::Superseded
        );
    }

    #[test]
    fn folders_list_again_and_keep_their_node_ids_within_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("a.txt"), b"child").unwrap();
        let announced = store(dir.path());
        announce(&announced, 1, vec![(NodeId(1), folder.clone())]);
        let restarted = store(dir.path());
        let files = restarted.namespace(offer(1)).unwrap().unwrap();
        let child = files.list(NodeId(1), None).unwrap().entries[0].clone();
        assert_eq!(child.name, "a.txt");
        let again = restarted.namespace(offer(1)).unwrap().unwrap();
        assert!(Arc::ptr_eq(&files, &again), "the same rebuilt namespace");
        assert_eq!(read(again.as_ref(), child.id, child.version).unwrap(), b"child");
    }

    #[test]
    fn paused_sharing_refuses_reads_of_rebuilt_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let policy = crate::policy::SharingPolicy::new(true);
        let announced = AnnouncedFiles::new(dir.path(), policy.clone());
        let version = announce(&announced, 1, vec![(NodeId(1), file)]).roots()[0].version;
        policy.update(false);
        let files = announced.namespace(offer(1)).unwrap().unwrap();
        assert_eq!(
            super::super::reason(&read(files.as_ref(), NodeId(1), version).unwrap_err()),
            Unavailable::Permission
        );
    }

    #[test]
    fn the_oldest_records_go_beyond_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let announced = store(dir.path()).with_max_offers(2);
        for seq in 1..=3 {
            announce(&announced, seq, vec![(NodeId(1), file.clone())]);
            // Distinct modification times on coarse file systems.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let restarted = store(dir.path());
        assert!(restarted.namespace(offer(1)).unwrap().is_none());
        assert!(restarted.namespace(offer(2)).unwrap().is_some());
        assert!(restarted.namespace(offer(3)).unwrap().is_some());
    }
}
