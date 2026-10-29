//! Sender-owned immutable fulfilments. These are independent of the transient
//! native source batch and are never receiver backing files. Journal commit
//! precedes publication; clipboard ownership or send completion/failure owns deletion.
use super::*;
use crate::model::{ContentSize, NodeKind};
use std::io::Write;

pub(super) const DIRECTORY: &str = "fulfilled-sources-v1";

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub id: [u8; 16],
    pub root: u64,
    #[serde(default)]
    pub retired: bool,
    #[serde(default)]
    pub started: bool,
    #[serde(default)]
    pub deadline_ms: u64,
}
impl Snapshot {
    pub fn path(&self, journal: &Path) -> PathBuf {
        journal.parent().unwrap().join(DIRECTORY).join(name(self.id))
    }
}
fn name(id: [u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) struct Build {
    pub snapshot: Snapshot,
    pub nodes: BTreeMap<u64, Node>,
    path: Option<PathBuf>,
}
impl Drop for Build {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_dir_all(path);
            crate::cache::changed(path.parent().unwrap());
        }
    }
}
impl Build {
    pub fn commit(&mut self) {
        self.path = None;
    }
    pub fn capture(journal: &Path, source: &dyn Namespace, root: FileNode) -> io::Result<Self> {
        if root.parent.is_some() {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| io::Error::other("snapshot identity unavailable"))?;
        let snapshot = Snapshot {
            id,
            root: root.id.0,
            retired: false,
            started: false,
            deadline_ms: 0,
        };
        let path = snapshot.path(journal);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::create_dir(&path)?;
        let mut build = Self {
            snapshot,
            nodes: BTreeMap::new(),
            path: Some(path.clone()),
        };
        let mut pending = vec![(root, path.join("root"), 0usize)];
        let mut visited = HashSet::new();
        let mut entries = BTreeMap::new();
        let mut paths = BTreeMap::new();
        let mut views = BTreeMap::new();
        let mut bytes = 0usize;
        while let Some((mut entry, target, depth)) = pending.pop() {
            if depth >= 256
                || !entry.valid()
                || !visited.insert(entry.id.0)
                || visited.len() > crate::model::MAX_DISCOVERED_NODES
            {
                return Err(unavailable(Unavailable::InvalidData));
            }
            bytes = bytes
                .checked_add(entry.name.len() + entry.kind.symlink_target().map_or(0, str::len) + 512)
                .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
            if bytes as u64 > JOURNAL_BYTES {
                return Err(unavailable(Unavailable::LimitExceeded));
            }
            if entry.kind == NodeKind::Directory {
                fs::create_dir(&target)?;
                let mut cursor = None;
                let mut children = Vec::new();
                let mut names = HashSet::new();
                let version;
                loop {
                    let page = source.list(entry.id, cursor)?;
                    page.validate(cursor).map_err(unavailable)?;
                    if page.directory != entry.id {
                        return Err(unavailable(Unavailable::InvalidData));
                    }
                    for child in page.entries {
                        if child.parent != Some(entry.id) || !names.insert(child.name.clone()) {
                            return Err(unavailable(Unavailable::InvalidData));
                        }
                        if pending.len() + visited.len() >= crate::model::MAX_DISCOVERED_NODES {
                            return Err(unavailable(Unavailable::LimitExceeded));
                        }
                        pending.push((child.clone(), target.join(&child.name), depth + 1));
                        children.push(child);
                    }
                    cursor = page.next;
                    if cursor.is_none() {
                        version = page.version;
                        break;
                    }
                }
                entry.version = version;
                views.insert(entry.id.0, (version, children));
            } else if let Some(link) = entry.kind.symlink_target() {
                if !oink_foundation::content::safe_symlink_target(link, depth) {
                    return Err(unavailable(Unavailable::InvalidData));
                }
                #[cfg(target_os = "macos")]
                std::os::unix::fs::symlink(link, &target)?;
                #[cfg(not(target_os = "macos"))]
                return Err(unavailable(Unavailable::Unsupported));
            } else {
                let selected = source.select(entry.id)?;
                if selected.id != entry.id
                    || selected.parent != entry.parent
                    || selected.name != entry.name
                    || selected.kind != NodeKind::File
                    || !selected.valid()
                {
                    return Err(unavailable(Unavailable::Superseded));
                }
                entry = selected;
                let ContentSize::Known(expected) = entry.size else {
                    return Err(unavailable(Unavailable::InvalidData));
                };
                let mut input = source.open(entry.id, entry.version, 0)?;
                let mut output = File::options().write(true).create_new(true).open(&target)?;
                let mut buffer = vec![0; 1024 * 1024];
                let mut total = 0u64;
                loop {
                    let count = input.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    total = total
                        .checked_add(count as u64)
                        .filter(|size| *size <= expected)
                        .ok_or_else(|| unavailable(Unavailable::Superseded))?;
                    output.write_all(&buffer[..count])?;
                }
                if total != expected {
                    return Err(unavailable(Unavailable::Superseded));
                }
                #[cfg(target_os = "macos")]
                crate::received::apply_exec_bits(&output, entry.exec_bits)?;
                output.sync_all()?;
            }
            paths.insert(entry.id.0, target);
            entries.insert(entry.id.0, entry);
        }
        // Capture identities only after all children are installed, so directory
        // change times cannot invalidate our own just-created immutable tree.
        for (id, path) in &paths {
            let entry = entries[id].clone();
            if entry.kind == NodeKind::Directory {
                crate::store::sync_directory(path)?;
            }
            let view = views.remove(id).map(|(version, children)| View {
                version,
                entries: children.iter().map(|child| entries[&child.id.0].clone()).collect(),
            });
            build.nodes.insert(
                *id,
                Node {
                    root: build.snapshot.root,
                    entry,
                    location: Location::capture(path)?,
                    view,
                },
            );
        }
        crate::store::sync_directory(&path)?;
        crate::store::sync_directory(path.parent().unwrap())?;
        crate::cache::changed(path.parent().unwrap());
        Ok(build)
    }
}

pub(super) fn collect(journal: &Path, state: &Journal) -> io::Result<()> {
    let directory = journal.parent().unwrap().join(DIRECTORY);
    let retained: HashSet<_> = retained(state).map(|snapshot| name(snapshot.id)).collect();
    let children = match fs::read_dir(&directory) {
        Ok(children) => children,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for child in children {
        let child = child?;
        let name = child.file_name();
        let name = name.to_string_lossy();
        if name.len() == 32
            && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            && child.file_type()?.is_dir()
            && !retained.contains(name.as_ref())
        {
            let _mutation = crate::cache::Mutation(&directory);
            fs::remove_dir_all(child.path())?;
        }
    }
    crate::store::sync_directory(&directory)?;
    Ok(())
}

pub(super) fn remove_unreferenced(journal: &Path, candidates: &[Snapshot], state: &Journal) -> io::Result<()> {
    for snapshot in candidates {
        if retained(state).any(|other| other.id == snapshot.id) {
            continue;
        }
        let directory = journal.parent().unwrap().join(DIRECTORY);
        let _mutation = crate::cache::Mutation(&directory);
        match fs::remove_dir_all(snapshot.path(journal)) {
            Ok(()) => {
                crate::store::sync_directory(snapshot.path(journal).parent().unwrap())?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn retained(state: &Journal) -> impl Iterator<Item = &Snapshot> {
    state
        .grants
        .iter()
        .flat_map(|grant| &grant.snapshots)
        .filter(|snapshot| !snapshot.retired)
        .chain(
            state
                .clipboard
                .iter()
                .flat_map(|current| current.roots.iter().map(|root| &root.snapshot)),
        )
}
