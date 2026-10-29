//! A current clipboard offer owns immutable bytes independently of receivers.
//! This is session state: sender restart discards every fulfilment.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CurrentClipboard {
    pub offer: oink_foundation::content::OfferId,
    pub key: Option<[u8; 32]>,
    pub roots: Vec<RetainedRoot>,
    #[serde(default, skip_serializing_if = "NativeFileMetadata::is_empty")]
    pub native: NativeFileMetadata,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetainedRoot {
    pub snapshot: Snapshot,
    pub nodes: BTreeMap<u64, Node>,
}
impl CurrentClipboard {
    pub fn validate(&self) -> io::Result<()> {
        let invalid = || unavailable(Unavailable::InvalidData);
        let mut roots = HashSet::new();
        let mut ids = HashSet::new();
        for root in &self.roots {
            let snapshot = &root.snapshot;
            if snapshot.retired
                || snapshot.started
                || snapshot.deadline_ms != 0
                || snapshot.id == [0; 16]
                || !roots.insert(snapshot.root)
                || root.nodes.is_empty()
            {
                return Err(invalid());
            }
            for (id, node) in &root.nodes {
                if node.root != snapshot.root
                    || node.entry.id.0 != *id
                    || !node.entry.valid()
                    || !ids.insert(*id)
                    || ids.len() > crate::model::MAX_DISCOVERED_NODES
                {
                    return Err(invalid());
                }
                let mut ancestor = *id;
                for depth in 0..256 {
                    let current = root.nodes.get(&ancestor).ok_or_else(invalid)?;
                    match current.entry.parent {
                        Some(parent) if depth < 255 => ancestor = parent.0,
                        None if ancestor == snapshot.root => break,
                        _ => return Err(invalid()),
                    }
                }
                if node.location.fingerprint.directory {
                    let view = node.view.as_ref().ok_or_else(invalid)?;
                    if node.entry.kind != crate::model::NodeKind::Directory
                        || view
                            .entries
                            .iter()
                            .any(|entry| entry.parent != Some(node.entry.id) || !root.nodes.contains_key(&entry.id.0))
                    {
                        return Err(invalid());
                    }
                } else if node.entry.kind != crate::model::NodeKind::File
                    || node.entry.size != crate::model::ContentSize::Known(node.location.fingerprint.size)
                {
                    return Err(invalid());
                }
            }
        }
        self.native.validate(
            &self
                .roots
                .iter()
                .map(|root| NodeId(root.snapshot.root))
                .collect::<Vec<_>>(),
        )?;
        Ok(())
    }
    pub fn retain<'a>(
        &mut self,
        snapshots: impl IntoIterator<Item = &'a Snapshot>,
        all_nodes: &BTreeMap<u64, Node>,
        native: &NativeFileMetadata,
    ) -> io::Result<()> {
        for snapshot in snapshots {
            if self.roots.iter().any(|root| root.snapshot.root == snapshot.root) {
                continue;
            }
            let mut snapshot = snapshot.clone();
            snapshot.retired = false;
            snapshot.started = false;
            snapshot.deadline_ms = 0;
            let nodes = all_nodes
                .iter()
                .filter(|(_, node)| node.root == snapshot.root)
                .map(|(id, node)| (*id, node.clone()))
                .collect();
            self.roots.push(RetainedRoot { snapshot, nodes });
        }
        self.native.extend(
            &native.restricted(
                &self
                    .roots
                    .iter()
                    .map(|root| NodeId(root.snapshot.root))
                    .collect::<Vec<_>>(),
            ),
        )
    }
}
impl SourceGrants {
    /// Recheck the real system slot without rendering payloads. Hold fulfilment
    /// exclusion across the probe and commit: Windows delayed rendering advances
    /// the sequence before issue_for can persist its final key. A busy fulfilment
    /// defers this pass, never blocks the clipboard observer.
    pub fn check_clipboard(&self, probe: impl FnOnce() -> io::Result<[u8; 32]>) -> io::Result<()> {
        self.check_clipboard_with(|_| probe().map(|key| (key, false)))
    }
    pub(in crate::content) fn check_clipboard_with(
        &self,
        probe: impl FnOnce(oink_foundation::content::OfferId) -> io::Result<([u8; 32], bool)>,
    ) -> io::Result<()> {
        let Ok(_fulfilment) = self.0.fulfilment.try_lock() else {
            return Ok(());
        };
        let mut state = self.0.state.lock().unwrap();
        if !state.healthy {
            return Err(unavailable(Unavailable::Permission));
        }
        let observed = state.journal.clipboard.as_ref().map(|current| probe(current.offer));
        if let (Some(current), Some(Ok((key, same_live_source)))) = (&state.journal.clipboard, &observed) {
            let key = *key;
            if current.key == Some(key) {
                state.clipboard_confirmed = true;
            } else {
                let mut journal = state.journal.clone();
                if *same_live_source && state.clipboard_confirmed {
                    // Other lazy representations can render after file fulfilment.
                    // The original native identity may validate that same owner;
                    // checkpoint its advanced sequence without losing ownership.
                    journal.clipboard.as_mut().unwrap().key = Some(key);
                } else {
                    journal.clipboard = None;
                    lifetime::superseded(&mut journal);
                }
                self.persist(&journal)?;
                state.journal = journal;
                state.clipboard_confirmed = true;
            }
        }
        self.expire_locked(&mut state, self.now_ms())?;
        fulfilled::collect(&self.0.path, &state.journal)?;
        observed.transpose().map(|_| ())
    }
    /// Metadata from the source's actual clipboard. A nonzero stable native key
    /// permits reuse across wire offer generations in this process. None means
    /// superseded; no key permits only the same live registration to retain it.
    pub fn observe_clipboard(&self, current: Option<ClipboardObservation>) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        if !state.healthy {
            return Err(unavailable(Unavailable::Permission));
        }
        let mut journal = state.journal.clone();
        let previous = journal.clipboard.take();
        journal.clipboard = current.map(|(offer, key)| {
            let retained = previous
                .filter(|old| (state.clipboard_confirmed && old.offer == offer) || (key.is_some() && key == old.key));
            let mut retained = retained.unwrap_or(CurrentClipboard {
                offer,
                key,
                roots: Vec::new(),
                native: Default::default(),
            });
            retained.offer = offer;
            retained.key = key;
            retained
        });
        lifetime::superseded(&mut journal);
        self.persist(&journal)?;
        state.journal = journal;
        state.clipboard_confirmed = true;
        drop(state);
        // Never wait for an in-progress native fulfilment in the clipboard
        // observer. A later send/observation/maintenance also performs collection.
        if let Ok(_fulfilment) = self.0.fulfilment.try_lock() {
            let state = self.0.state.lock().unwrap();
            fulfilled::collect(&self.0.path, &state.journal)?;
        }
        Ok(())
    }
}
