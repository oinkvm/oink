//! Durable indirection only for files uploaded by phones.
use super::*;
use crate::tracked_file::TrackedFile;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    file: TrackedFile,
    offered: Fingerprint,
    relocated: bool,
}
pub(super) struct Root {
    source: PathBuf,
    sidecar: PathBuf,
    record: Mutex<Record>,
}
#[derive(Clone)]
pub(super) struct Location {
    root: Arc<Root>,
    tail: PathBuf,
}
impl Root {
    pub(super) fn load(source: PathBuf) -> io::Result<Arc<Self>> {
        let name = source
            .file_name()
            .ok_or(io::ErrorKind::InvalidInput)?
            .as_encoded_bytes();
        let sidecar = source.with_file_name(format!(".oink-reference-{}.json", blake3::hash(name).to_hex()));
        let record = match fs::read(&sidecar) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let record = Record {
                    file: TrackedFile::capture(&source)?,
                    offered: inspect(&source)?,
                    relocated: false,
                };
                oink_foundation::storage::atomic_write(&sidecar, &serde_json::to_vec(&record)?)?;
                record
            }
            Err(error) => return Err(error),
        };
        Ok(Arc::new(Self {
            source,
            sidecar,
            record: Mutex::new(record),
        }))
    }
    pub(super) fn path(&self) -> io::Result<PathBuf> {
        self.record
            .lock()
            .unwrap()
            .file
            .resolve()
            .map_err(crate::tracked_file::ResolveError::into_io)
    }
    pub(super) fn location(self: &Arc<Self>, tail: PathBuf) -> Location {
        Location {
            root: self.clone(),
            tail,
        }
    }
    pub(super) fn offered(&self) -> Fingerprint {
        self.record.lock().unwrap().offered.clone()
    }
    fn relocate(&self, target: &TrackedFile) -> io::Result<()> {
        let target = target.clone();
        let mut record = self.record.lock().unwrap();
        let (_, file) = target.open().map_err(crate::tracked_file::ResolveError::into_io)?;
        let current = file_fingerprint(&file)?;
        // Cloud Files publishes FILETIME from the source's millisecond metadata.
        // Validate that exact published version before giving up the proxy bytes.
        if current.directory != record.offered.directory
            || (!current.directory
                && (current.size != record.offered.size
                    || modified_ms(current.modified) != modified_ms(record.offered.modified)))
        {
            return Err(unavailable(Unavailable::Superseded));
        }
        let next = Record {
            file: target,
            offered: record.offered.clone(),
            relocated: true,
        };
        oink_foundation::storage::atomic_write(&self.sidecar, &serde_json::to_vec(&next)?)?;
        *record = next;
        // The durable reference owns the target before the old cache is removed.
        // Only unlink our original identity; never a replacement at the old name.
        if inspect(&self.source).is_ok_and(|file| file.identity == record.offered.identity) {
            if record.offered.directory {
                fs::remove_dir_all(&self.source)?;
            } else {
                fs::remove_file(&self.source)?;
            }
            crate::cache::changed(&self.source);
        }
        Ok(())
    }
}
impl Location {
    pub(super) fn path(&self) -> io::Result<PathBuf> {
        if self.tail.as_os_str().is_empty() {
            self.root.path()
        } else {
            Ok(self.root.path()?.join(&self.tail))
        }
    }
    pub(super) fn child(&self, name: &Path) -> Self {
        Self {
            root: self.root.clone(),
            tail: self.tail.join(name),
        }
    }
    pub(super) fn tracked(&self) -> Option<TrackedFile> {
        if self.tail.as_os_str().is_empty() {
            Some(
                self.root
                    .record
                    .lock()
                    .unwrap()
                    .file
                    .clone()
                    .with_record(self.root.sidecar.clone()),
            )
        } else {
            None
        }
    }
    pub(super) fn relocate(&self, target: &TrackedFile) -> io::Result<()> {
        if !self.tail.as_os_str().is_empty() {
            return Ok(());
        }
        self.root.relocate(target)
    }
    pub(super) fn normalize(&self, current: Fingerprint, offered: &Fingerprint) -> io::Result<Fingerprint> {
        let record = self.root.record.lock().unwrap();
        if current.directory != offered.directory
            || (!current.directory
                && (current.size != offered.size
                    || if record.relocated {
                        modified_ms(current.modified) != modified_ms(offered.modified)
                    } else {
                        current.modified != offered.modified
                    }))
        {
            return Err(unavailable(Unavailable::Superseded));
        }
        // Root identity is checked by TrackedFile. Descendant paths may only
        // follow the same moved root; inspect refuses links. A normal inode move
        // must keep every child's identity; a CF handoff creates new identities.
        if !record.relocated && current.identity != offered.identity {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        Ok(offered.clone())
    }
}

#[cfg(all(test, debug_assertions, any(windows, target_os = "macos")))]
mod tests {
    use super::*;
    use crate::content::{namespace::Namespace, read_all};
    fn phone(roots: Vec<(NodeId, PathBuf)>) -> Arc<Local> {
        Local::received_from(roots, oink_foundation::DeviceId([17; 32])).unwrap()
    }
    #[test]
    fn phone_file_survives_move_restart_and_old_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"phone bytes").unwrap();
        let files = phone(vec![(NodeId(1), source.clone())]);
        let version = files.roots()[0].version;
        fs::rename(&source, &target).unwrap();
        fs::write(&source, b"replacement").unwrap();
        assert_eq!(
            read_all(&mut files.open(NodeId(1), version, 0).unwrap()).unwrap(),
            b"phone bytes"
        );
        drop(files);
        let files = phone(vec![(NodeId(1), source)]);
        assert_eq!(
            read_all(&mut files.open(NodeId(1), version, 0).unwrap()).unwrap(),
            b"phone bytes"
        );
        fs::write(&target, b"edited").unwrap();
        assert_eq!(
            crate::content::reason(&files.open(NodeId(1), version, 0).err().unwrap()),
            Unavailable::Superseded
        );
    }
    #[test]
    fn cloud_handoff_persists_before_cache_removal_and_preserves_version() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"phone bytes").unwrap();
        let files = phone(vec![(NodeId(1), source.clone())]);
        let version = files.roots()[0].version;
        let saved = files.tracked_file(NodeId(1)).unwrap().unwrap();
        let saved: TrackedFile = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        fs::copy(&source, &target).unwrap();
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(fs::metadata(&source).unwrap().modified().unwrap()))
            .unwrap();
        files
            .proxy_file_moved(NodeId(1), &TrackedFile::capture(&target).unwrap())
            .unwrap();
        assert!(!source.exists());
        assert_eq!(saved.resolve().unwrap(), target.canonicalize().unwrap());
        assert_eq!(
            read_all(&mut files.open(NodeId(1), version, 0).unwrap()).unwrap(),
            b"phone bytes"
        );
        let restarted = phone(vec![(NodeId(1), source)]);
        assert_eq!(restarted.roots()[0].name, "source");
        assert_eq!(restarted.roots()[0].version, version);
        assert_eq!(
            read_all(&mut restarted.open(NodeId(1), version, 0).unwrap()).unwrap(),
            b"phone bytes"
        );
        let moved = dir.path().join("moved_again");
        fs::rename(&target, &moved).unwrap();
        assert_eq!(
            read_all(&mut files.open(NodeId(1), version, 0).unwrap()).unwrap(),
            b"phone bytes"
        );
    }
    #[test]
    fn folder_children_listed_before_move_remain_readable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("folder");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("child"), b"child bytes").unwrap();
        let files = phone(vec![(NodeId(1), source.clone())]);
        let child = files.list(NodeId(1), None).unwrap().entries[0].clone();
        let target = dir.path().join("moved");
        fs::rename(&source, &target).unwrap();
        assert_eq!(
            read_all(&mut files.open(child.id, child.version, 0).unwrap()).unwrap(),
            b"child bytes"
        );
    }
    #[test]
    fn cloud_folder_handoff_keeps_existing_child_ids_and_versions() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("child"), b"child bytes").unwrap();
        let files = phone(vec![(NodeId(1), source.clone())]);
        let child = files.list(NodeId(1), None).unwrap().entries[0].clone();
        fs::create_dir(&target).unwrap();
        fs::copy(source.join("child"), target.join("child")).unwrap();
        fs::File::options()
            .write(true)
            .open(target.join("child"))
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(fs::metadata(source.join("child")).unwrap().modified().unwrap()),
            )
            .unwrap();
        files
            .proxy_file_moved(NodeId(1), &TrackedFile::capture(&target).unwrap())
            .unwrap();
        assert!(!source.exists());
        assert_eq!(
            read_all(&mut files.open(child.id, child.version, 0).unwrap()).unwrap(),
            b"child bytes"
        );
        assert_eq!(files.list(NodeId(1), None).unwrap().entries[0].id, child.id);
    }
    #[test]
    fn ordinary_pc_readers_do_not_extend_the_retained_path_reference_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pc-file");
        fs::write(&path, b"pc bytes").unwrap();
        let files = Local::clipboard(vec![(NodeId(1), path)]).unwrap();
        // Retention transfers ownership out of the namespace to a FIFO. Keep
        // precisely that owner here, then revoke it with an already-open reader.
        let owner = {
            let mut state = files.state.lock().unwrap();
            let node = state.nodes.get_mut(&NodeId(1)).unwrap();
            let owner = node.location.get().unwrap();
            node.location = super::super::Location::Offered(Arc::downgrade(&owner));
            owner
        };
        let mut reader = files.open(NodeId(1), files.roots()[0].version, 0).unwrap();
        drop(owner);
        assert_eq!(
            crate::content::reason(&reader.read(&mut [0]).unwrap_err()),
            Unavailable::NoSuchItem
        );
    }
    #[test]
    fn edit_before_handoff_keeps_proxy_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"original").unwrap();
        let files = phone(vec![(NodeId(1), source.clone())]);
        fs::write(&target, b"wrong bytes").unwrap();
        assert!(
            files
                .proxy_file_moved(NodeId(1), &TrackedFile::capture(&target).unwrap())
                .is_err()
        );
        assert!(source.exists());
        assert_eq!(
            files.tracked_file(NodeId(1)).unwrap().unwrap().resolve().unwrap(),
            source.canonicalize().unwrap()
        );
    }
}
