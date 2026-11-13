//! Target-local MOVE never deletes a remote original or user-owned destination.
use super::*;
use serde::{Deserialize, Serialize};
pub(super) mod notification;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    /// Native MOVE report authorizes source-copy deletion.
    Pending,
    /// Optimized MOVE: converge absent/moved identities without deleting a surviving copy.
    Observed,
    Removed,
    /// Identity convergence finished with at least one surviving received copy.
    Retained,
}
impl Offer {
    pub(in crate::windows) fn target_consumed(&self, entry: usize) -> io::Result<()> {
        let delivery = self.prepared.lock().unwrap().clone().ok_or(io::ErrorKind::NotFound)?;
        let index = if let Some(nodes) = self.contents.descriptor_nodes()? {
            let node = nodes.get(entry).ok_or(io::ErrorKind::InvalidInput)?.id;
            self.service
                .received
                .lock()
                .unwrap()
                .sources
                .get(&delivery.id)
                .and_then(|source| source.nodes.iter().position(|entry| entry.id == node))
                .ok_or(io::ErrorKind::NotFound)?
        } else {
            entry
        };
        self.service.target_consumed(delivery.id, index)
    }

    pub(in crate::windows) fn move_completed(&self) -> io::Result<()> {
        let delivery = self.prepared.lock().unwrap().clone();
        let Some(delivery) = delivery else { return Ok(()) };
        self.service.mark_move(delivery.id)
    }
    pub(in crate::windows) fn optimized_move_completed(&self) -> io::Result<()> {
        let delivery = self.prepared.lock().unwrap().clone();
        if let Some(delivery) = delivery {
            self.service.mark_move_with(delivery.id, State::Observed)?;
        }
        Ok(())
    }
}
impl Service {
    fn proxy_root(
        &self,
        old: &Path,
    ) -> io::Result<Option<(Arc<dyn crate::content::namespace::Namespace>, crate::model::NodeId)>> {
        let deliveries: Vec<_> = self.deliveries.lock().unwrap().values().cloned().collect();
        for delivery in deliveries {
            let Some(namespace) = delivery.contents().namespace() else {
                continue;
            };
            let Some(nodes) = delivery.contents().descriptor_nodes()? else {
                continue;
            };
            let index =
                delivery.native_paths.lock().unwrap().iter().position(|path| {
                    received::relative_path(path, old).is_some_and(|tail| tail.as_os_str().is_empty())
                });
            if let Some(index) = index {
                let Some(node) = nodes.get(index) else { continue };
                if namespace.tracked_file(node.id)?.is_some() {
                    return Ok(Some((namespace, node.id)));
                }
            }
        }
        Ok(None)
    }
    pub(super) fn watch_proxy_move(self: &Arc<Self>, old: &Path, target: &Path, key: (u32, i64)) -> io::Result<()> {
        if self.proxy_root(old)?.is_none() {
            // Ordinary PC received files keep their existing lifecycle; only
            // phone roots need pending path/identity observations.
            self.received.lock().unwrap().renames.remove(&key);
            return Ok(());
        }
        let watch = notification::Watch::start(Arc::downgrade(self), old.to_owned(), target.to_owned(), key)?;
        let mut journal = self.received.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        journal.move_watches.insert(key, watch);
        journal.move_watches.get_mut(&key).unwrap().activate();
        Ok(())
    }
    /// Called only by CF completion or a real filesystem change notification.
    pub(super) fn track_proxy_move(&self, old: &Path, volume: u32, file: i64) -> io::Result<bool> {
        match std::fs::symlink_metadata(old) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Ok(false),
            Err(error) => return Err(error),
        }
        let Some((namespace, node)) = self.proxy_root(old)? else {
            return Ok(true);
        };
        // Resolve the exact old placeholder FRN, not the pathname which another
        // object could already have replaced. A second user move is also safe.
        let reference = crate::tracked_file::TrackedFile::capture_native(&self.root, volume, file)?;
        let current = reference
            .resolve()
            .map_err(crate::tracked_file::ResolveError::into_io)?;
        if received::relative_path(&self.root.canonicalize()?, &current).is_some() {
            return Ok(false);
        }
        namespace.proxy_file_moved(node, &reference)?;
        tracing::info!(target: "oink_file_diagnostics", file_stage = 63_u8,
            file_id = file, "phone proxy cache replaced by moved file reference");
        Ok(true)
    }
    fn target_consumed(&self, id: u64, index: usize) -> io::Result<()> {
        let mut journal = self.received.lock().unwrap();
        if journal.cancelled.contains(&id)
            || journal
                .deliveries
                .get(&id)
                .and_then(|nodes| nodes.get(index))
                .is_none_or(|node| node.deleted)
        {
            return Err(unavailable());
        }
        let previous = journal.consumed.get(&id).cloned();
        journal.consumed.entry(id).or_default().insert(index);
        let previous_move = journal.moves.get(&id).copied();
        if let Err(error) = journal.save(&self.root) {
            if let Some(previous) = previous {
                journal.consumed.insert(id, previous);
            } else {
                journal.consumed.remove(&id);
            }
            if let Some(previous) = previous_move {
                journal.moves.insert(id, previous);
            }
            return Err(error);
        }
        Ok(())
    }

    fn mark_move(&self, id: u64) -> io::Result<()> {
        self.mark_move_with(id, State::Pending)
    }
    fn mark_move_with(&self, id: u64, next: State) -> io::Result<()> {
        let mut journal = self.received.lock().unwrap();
        if journal.cancelled.contains(&id) || !journal.deliveries.contains_key(&id) {
            return Err(unavailable());
        }
        let previous = journal.moves.get(&id).copied();
        if matches!(previous, Some(State::Removed | State::Pending))
            || previous == Some(next)
            || (previous == Some(State::Retained) && next == State::Observed)
        {
            return Ok(());
        }
        journal.moves.insert(id, next);
        if let Err(error) = journal.save(&self.root) {
            if let Some(previous) = previous {
                journal.moves.insert(id, previous);
            } else {
                journal.moves.remove(&id);
            }
            return Err(error);
        }
        drop(journal);
        if let Err(error) = self.finish_move(id, next) {
            tracing::debug!(%error,"received MOVE deletion will retry");
        }
        Ok(())
    }
    pub(super) fn cleanup_moved(&self) {
        let pending: Vec<_> = self
            .received
            .lock()
            .unwrap()
            .moves
            .iter()
            .filter(|(_, state)| matches!(state, State::Observed | State::Pending))
            .map(|(id, state)| (*id, *state))
            .collect();
        for (id, state) in pending {
            if let Err(error) = self.finish_move(id, state) {
                tracing::debug!(%error, "received MOVE cleanup will retry");
            }
        }
    }
    fn finish_move(&self, id: u64, intent: State) -> io::Result<()> {
        let delivery = self.deliveries.lock().unwrap().get(&id).cloned();
        let (mut nodes, _admission) = {
            let mut journal = self.received.lock().unwrap();
            if !matches!(intent, State::Observed | State::Pending)
                || journal.moves.get(&id) != Some(&intent)
                || journal.cancelled.contains(&id)
                || journal.cleanup_inflight.iter().any(|(owner, _)| *owner == id)
            {
                return Ok(());
            }
            let nodes = journal.deliveries.get(&id).ok_or(io::ErrorKind::NotFound)?.clone();
            // Observed only converges identity. A deletion admission barrier
            // rejects concurrent ReaderPin/FETCH_DATA and cancels the target's
            // first hydration (Chrome on Windows 11, 2026-11-03). It must neither
            // close reader admission nor acquire exclusive/write/delete access.
            let admission = (intent == State::Pending).then(|| {
                journal.cleanup_inflight.insert((id, usize::MAX));
                Admission { service: self, id }
            });
            (nodes.into_iter().enumerate().collect::<Vec<_>>(), admission)
        };
        nodes.sort_by_key(|(_, node)| std::cmp::Reverse(node.path.components().count()));
        for (index, node) in nodes {
            if node.deleted {
                continue;
            }
            let path = self.root.join(&node.path);
            let base = {
                let journal = self.received.lock().unwrap();
                journal
                    .sources
                    .get(&id)
                    .map(|source| self.root.join(&source.base))
                    .or_else(|| delivery.as_ref().map(|delivery| delivery.path.clone()))
            };
            let relocated = base.as_ref().is_some_and(|base| !path.starts_with(base));
            let gone = if relocated {
                true
            } else if intent == State::Observed {
                // Shared FILE_READ_ATTRIBUTES + OPEN_REPARSE_POINT, without
                // recalling data or excluding hydration/consumer handles.
                match self.disk_identity(&path) {
                    Ok((owner, slot, _)) => owner != id || slot != index,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error),
                }
            } else {
                match protection::Protected::open(
                    self.api.clone(),
                    &path,
                    CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_DELETE_ACCESS | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
                ) {
                    Ok(file) => {
                        let (owner, slot, _) = self.handle_identity(file.handle, 0)?;
                        // A replacement has independent ownership. Forget this
                        // old identity; never unlink the object at its old path.
                        if owner == id && slot == index {
                            // User decision 2026-10-29: MOVE is the deletion
                            // authority. No content, in-sync, residency or consumer
                            // proof is required after that native report.
                            file.delete()?;
                            crate::cache::changed(&self.root);
                        }
                        true
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error),
                }
            };
            let mut journal = self.received.lock().unwrap();
            if journal.moves.get(&id) != Some(&intent) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if let Some(current) = journal.deliveries.get_mut(&id).and_then(|nodes| nodes.get_mut(index)) {
                if current.path != node.path {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                if !gone {
                    continue;
                }
                current.deleted = true;
            }
            if let Some(delivery) = &delivery {
                delivery.file(index).deleted.store(true, Ordering::Release);
            }
            journal.save(&self.root)?;
        }
        let mut journal = self.received.lock().unwrap();
        let removed = journal
            .deliveries
            .get(&id)
            .is_some_and(|nodes| nodes.iter().all(|node| node.deleted));
        // Retained ends only this MOVE observation, not the file's receive or
        // one-shot lifecycle. Later Pending evidence can still authorize unlink.
        let next = if removed {
            Some(State::Removed)
        } else if intent == State::Observed {
            Some(State::Retained)
        } else {
            None
        };
        if journal.moves.get(&id) == Some(&intent)
            && let Some(next) = next
        {
            let previous = journal.moves.insert(id, next);
            if let Err(error) = journal.save(&self.root) {
                if let Some(previous) = previous {
                    journal.moves.insert(id, previous);
                }
                return Err(error);
            }
        }
        Ok(())
    }
}
struct Admission<'a> {
    service: &'a Service,
    id: u64,
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.service
            .received
            .lock()
            .unwrap()
            .cleanup_inflight
            .remove(&(self.id, usize::MAX));
    }
}
