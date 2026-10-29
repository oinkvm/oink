use super::*;
use crate::content::Provider;
fn node(id: u64, parent: Option<u64>, name: &str, kind: NodeKind) -> FileNode {
    FileNode {
        id: NodeId(id),
        parent: parent.map(NodeId),
        name: name.into(),
        kind,
        size: ContentSize::Known(0),
        version: 1,
        modified_ms: None,
        exec_bits: None,
    }
}
#[test]
fn arrival_commits_directory_and_empty_children_without_native_or_source_reads() {
    let entries = vec![
        node(1, None, "tree", NodeKind::Directory),
        node(2, Some(1), "empty", NodeKind::Directory),
        node(3, Some(1), "zero", NodeKind::File),
    ];
    let real = node(4, None, "linked", NodeKind::File);
    let root = FulfilledRoot {
        node: NodeId(1),
        id: [9; 16],
    };
    let descriptor = Descriptor {
        offer: crate::content::tests::fixture().inventory().id,
        token: super::super::super::grants::LinkToken([1; 32]),
        provisional: false,
        roots: vec![entries[0].clone(), real.clone()],
        fulfilled: vec![root.clone()],
        native: Default::default(),
    };
    let mut link = Some(descriptor);
    let mut local = FinalLocal::default();
    local.commit(&mut link, &root, &entries).unwrap();
    assert_eq!(link.as_ref().unwrap().roots, [real]);
    let bytes = serde_json::to_vec(&(local, link)).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("pending"));
    let (local, link): (FinalLocal, Option<Descriptor>) = serde_json::from_slice(&bytes).unwrap();
    let namespace = local.restore(link, &Connector::default()).unwrap();
    assert_eq!(namespace.list(NodeId(1), None).unwrap().entries.len(), 2);
    assert!(namespace.list(NodeId(2), None).unwrap().entries.is_empty());
    assert!(
        namespace.open(NodeId(3), 1, 0).is_err(),
        "FinalLocal never reopens the sender"
    );
    assert_eq!(
        namespace.select(NodeId(4)).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}
