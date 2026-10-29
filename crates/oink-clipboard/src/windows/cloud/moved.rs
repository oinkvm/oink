//! Target-local MOVE completion never authorizes deleting a source or target path.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    /// Native MOVE report authorizes source-copy deletion.
    Pending,
    /// Optimized MOVE: converge absent/moved identities without deleting a surviving copy.
    Observed,
    Removed,
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
        if matches!(previous, Some(State::Removed | State::Pending)) || previous == Some(next) {
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
            .filter(|(_, state)| **state != State::Removed)
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
            if journal.cancelled.contains(&id) || journal.cleanup_inflight.iter().any(|(owner, _)| *owner == id) {
                return Ok(());
            }
            let nodes = journal.deliveries.get(&id).ok_or(io::ErrorKind::NotFound)?.clone();
            journal.cleanup_inflight.insert((id, usize::MAX));
            (
                nodes.into_iter().enumerate().collect::<Vec<_>>(),
                Admission { service: self, id },
            )
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
            let result = if relocated {
                Ok(())
            } else {
                match protection::Protected::open(
                    self.api.clone(),
                    &path,
                    CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_DELETE_ACCESS | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
                ) {
                    Ok(file) => {
                        let (owner, slot, _) = self.handle_identity(file.handle, 0)?;
                        if owner != id || slot != index {
                            // A replacement has independent ownership. Forget this
                            // old identity; never unlink the object at its old path.
                            Ok(())
                        } else if intent == State::Observed {
                            Err(io::ErrorKind::WouldBlock.into())
                        } else {
                            // User decision 2026-10-29: MOVE is the deletion
                            // authority. No content, in-sync, residency or consumer
                            // proof is required after that native report.
                            file.delete()?;
                            crate::cache::changed(&self.root);
                            Ok(())
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                }
            };
            result?;
            let mut journal = self.received.lock().unwrap();
            if let Some(current) = journal.deliveries.get_mut(&id).and_then(|nodes| nodes.get_mut(index)) {
                if current.path != node.path {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                current.deleted = true;
            }
            if let Some(delivery) = &delivery {
                delivery.file(index).deleted.store(true, Ordering::Release);
            }
            journal.save(&self.root)?;
        }
        let mut journal = self.received.lock().unwrap();
        if journal
            .deliveries
            .get(&id)
            .is_some_and(|nodes| nodes.iter().all(|node| node.deleted))
        {
            let previous = journal.moves.insert(id, State::Removed);
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
