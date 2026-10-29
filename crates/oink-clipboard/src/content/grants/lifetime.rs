//! Each receiver owns a bounded fetch independently of the current clipboard.
//! Completion and failure release non-clipboard caches; no receiver ACK exists.
use super::*;

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn superseded(journal: &mut Journal) {
    let retained: HashSet<_> = journal
        .clipboard
        .iter()
        .flat_map(|current| &current.roots)
        .map(|root| root.snapshot.id)
        .collect();
    for grant in &mut journal.grants {
        if grant.clipboard {
            for snapshot in &mut grant.snapshots {
                if !snapshot.retired && !retained.contains(&snapshot.id) {
                    snapshot.retired = true;
                }
            }
        }
    }
}
impl SourceGrants {
    /// Called after the transport accepted a complete file or final directory
    /// page, never from Reader::read (which may only have filled a send buffer).
    pub fn sent(&self, peer: DeviceId, token: LinkToken, node: NodeId) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        let grant = Self::admitted(&state, peer, token)?;
        if !grant.active {
            return Err(unavailable(Unavailable::Permission));
        }
        let root = grant
            .nodes
            .get(&node.0)
            .ok_or_else(|| unavailable(Unavailable::Permission))?
            .root;
        if !grant
            .snapshots
            .iter()
            .any(|snapshot| snapshot.root == root && !snapshot.retired)
        {
            return Ok(());
        }
        state.sent.insert((token, node));
        let grant = Self::admitted(&state, peer, token)?;
        let complete = grant.nodes.values().filter(|entry| entry.root == root).all(|entry| {
            (grant.omit_ds_store && file_filter::hidden(&grant.nodes, entry.entry.id.0))
                || entry.entry.kind.symlink_target().is_some()
                || state.sent.contains(&(token, entry.entry.id))
        });
        if complete {
            // Old hover hosts retain the same bytes only while the source drag
            // is live. A successful send ends that root's source obligation.
            retire_pending(&mut state.journal, token, Some(root));
            let grant = state
                .journal
                .grants
                .iter_mut()
                .find(|grant| grant.token == token)
                .unwrap();
            grant
                .snapshots
                .iter_mut()
                .find(|snapshot| snapshot.root == root)
                .unwrap()
                .retired = true;
            state
                .progress
                .retain(|(previous, previous_root, _, _), _| *previous != token || *previous_root != root);
            let finished: HashSet<_> = state
                .journal
                .grants
                .iter()
                .find(|grant| grant.token == token)
                .unwrap()
                .nodes
                .values()
                .filter(|node| node.root == root)
                .map(|node| node.entry.id)
                .collect();
            state
                .sent
                .retain(|(previous, node)| *previous != token || !finished.contains(node));
            // CurrentClipboard remains an independent owner until replacement.
            // No F1 send state is persisted and no receiver ACK can delay this.
            if let Ok(_fulfilment) = self.0.fulfilment.try_lock() {
                fulfilled::collect(&self.0.path, &state.journal)?;
            }
        }
        Ok(())
    }
    pub(super) fn now_ms(&self) -> u64 {
        self.0.clock_epoch_ms.saturating_add(
            self.0
                .clock_started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        )
    }
    pub(super) fn expire_locked(&self, state: &mut State, now: u64) -> io::Result<()> {
        let mut journal = state.journal.clone();
        let mut changed = false;
        let mut expired = Vec::new();
        for grant in &mut journal.grants {
            for snapshot in &mut grant.snapshots {
                if grant.active && !snapshot.retired && snapshot.deadline_ms <= now {
                    snapshot.retired = true;
                    expired.push((grant.token, snapshot.root));
                    changed = true;
                }
            }
        }
        for (token, root) in expired {
            retire_pending(&mut journal, token, Some(root));
        }
        if changed {
            self.persist(&journal)?;
            state.journal = journal;
            let live: HashSet<_> = state
                .journal
                .grants
                .iter()
                .flat_map(|grant| {
                    grant
                        .snapshots
                        .iter()
                        .filter(|snapshot| !snapshot.retired)
                        .map(|snapshot| (grant.token, snapshot.root))
                })
                .collect();
            state
                .progress
                .retain(|(token, root, _, _), _| live.contains(&(*token, *root)));
        }
        Ok(())
    }
    pub(super) fn fetch_progress_at(
        &self,
        peer: DeviceId,
        token: LinkToken,
        id: NodeId,
        kind: u8,
        through: u64,
        now: u64,
    ) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        let grant = Self::admitted(&state, peer, token)?;
        let root = grant
            .nodes
            .get(&id.0)
            .ok_or_else(|| unavailable(Unavailable::Permission))?
            .root;
        let Some(snapshot) = grant.snapshots.iter().find(|snapshot| snapshot.root == root) else {
            return Ok(());
        };
        if !grant.active {
            return Ok(());
        }
        if snapshot.retired {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        if snapshot.deadline_ms <= now {
            return Err(unavailable(Unavailable::NoProgress));
        }
        let key = (token, root, kind, id.0);
        let progress = through > state.progress.get(&key).copied().unwrap_or(0);
        // The first read is durable before returning its handle. Later high-water
        // progress checkpoints are coalesced (at most once per 10 seconds/root).
        // Repeating reads of the same range never renews the deadline.
        if !snapshot.started
            || (progress
                && now.saturating_add(crate::model::NO_PROGRESS_MS) >= snapshot.deadline_ms.saturating_add(10_000))
        {
            let mut journal = state.journal.clone();
            let snapshot = journal
                .grants
                .iter_mut()
                .find(|grant| grant.token == token)
                .unwrap()
                .snapshots
                .iter_mut()
                .find(|snapshot| snapshot.root == root)
                .unwrap();
            snapshot.started = true;
            snapshot.deadline_ms = now.saturating_add(crate::model::NO_PROGRESS_MS);
            self.persist(&journal)?;
            state.journal = journal;
        }
        if progress {
            state.progress.insert(key, through);
        }
        Ok(())
    }
    pub(super) fn fetch_progress(
        &self,
        peer: DeviceId,
        token: LinkToken,
        id: NodeId,
        kind: u8,
        through: u64,
    ) -> io::Result<()> {
        self.fetch_progress_at(peer, token, id, kind, through, self.now_ms())
    }
    /// Leaving a hover host cancels its context, not the source drag. The source
    /// lease discards provisional grants when that drag ends. After Drop, an
    /// explicit failure retires its one-shot roots; real links are unchanged.
    pub fn abandon(&self, peer: DeviceId, token: LinkToken) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        if !Self::admitted(&state, peer, token)?.active {
            return Ok(());
        }
        let mut journal = state.journal.clone();
        retire_pending(&mut journal, token, None);
        let grant = journal.grants.iter_mut().find(|grant| grant.token == token).unwrap();
        for snapshot in &mut grant.snapshots {
            snapshot.retired = true;
        }
        self.persist(&journal)?;
        state.journal = journal;
        state.progress.retain(|(previous, _, _, _), _| *previous != token);
        drop(state);
        if let Ok(_fulfilment) = self.0.fulfilment.try_lock() {
            fulfilled::collect(&self.0.path, &self.0.state.lock().unwrap().journal)?;
        }
        Ok(())
    }
}

/// Retire only provisional references to the completed/failed sender snapshot.
/// Other active receivers (and the independent clipboard owner) keep theirs.
fn retire_pending(journal: &mut Journal, token: LinkToken, root: Option<u64>) {
    let grant = journal.grants.iter().find(|grant| grant.token == token).unwrap();
    let offered = grant.offered;
    let snapshots: HashSet<_> = grant
        .snapshots
        .iter()
        .filter(|snapshot| root.is_none_or(|root| snapshot.root == root))
        .map(|snapshot| snapshot.id)
        .collect();
    for grant in journal
        .grants
        .iter_mut()
        .filter(|grant| !grant.active && grant.offered == offered)
    {
        for snapshot in &mut grant.snapshots {
            if snapshots.contains(&snapshot.id) {
                snapshot.retired = true;
            }
        }
    }
}
