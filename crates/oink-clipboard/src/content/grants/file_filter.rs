//! A peer's directory view. Root selections are never filtered.
use super::*;

pub(super) fn hidden(nodes: &BTreeMap<u64, Node>, mut id: u64) -> bool {
    while let Some(node) = nodes.get(&id) {
        let Some(parent) = node.entry.parent else {
            return false;
        };
        if node.entry.name == ".DS_Store" {
            return true;
        }
        id = parent.0;
    }
    false
}

#[cfg(all(test, debug_assertions))]
pub(super) fn page(
    omit: bool,
    id: NodeId,
    version: u64,
    entries: &[FileNode],
    cursor: Option<DirectoryCursor>,
) -> io::Result<DirectoryPage> {
    page_with_links(omit, true, id, version, entries, cursor)
}
#[cfg(all(test, debug_assertions))]
pub(super) fn page_with_links(
    omit: bool,
    links: bool,
    id: NodeId,
    version: u64,
    entries: &[FileNode],
    cursor: Option<DirectoryCursor>,
) -> io::Result<DirectoryPage> {
    page_for(omit, links, true, id, version, entries, cursor)
}
pub(super) fn project(mut entry: FileNode, exec: bool) -> FileNode {
    if !exec {
        entry.exec_bits = None;
    }
    entry
}
pub(super) fn page_for(
    omit: bool,
    links: bool,
    exec: bool,
    id: NodeId,
    version: u64,
    entries: &[FileNode],
    cursor: Option<DirectoryCursor>,
) -> io::Result<DirectoryPage> {
    // Filter before deriving offsets/counts; never return a short intermediate
    // page or change the source metadata version just because an entry is hidden.
    let entries: Vec<_> = entries
        .iter()
        .filter(|entry| (!omit || entry.name != ".DS_Store") && (links || entry.kind.symlink_target().is_none()))
        .cloned()
        .map(|entry| project(entry, exec))
        .collect();
    namespace::page(id, version, &entries, cursor)
}
