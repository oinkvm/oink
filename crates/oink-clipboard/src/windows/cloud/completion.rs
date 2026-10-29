//! A one-shot becomes FinalLocal from receive completion events. No ACK,
//! residency proof, second read, or source metadata request gates completion.
use super::*;
use crate::content::{
    linked::completed,
    namespace::{Files, NativeFiles},
};
use crate::model::NodeKind;

fn covered(ranges: &[CF_FILE_RANGE], length: i64) -> bool {
    let mut end = 0i64;
    for range in ranges {
        if range.StartingOffset < 0 || range.Length <= 0 || range.StartingOffset > end {
            return false;
        }
        let Some(next) = range.StartingOffset.checked_add(range.Length) else {
            return false;
        };
        end = end.max(next);
    }
    end >= length
}
impl Service {
    pub(super) fn handle_fully_resident(&self, handle: HANDLE, length: u64) -> io::Result<bool> {
        self.handle_fully_resident_traced(handle, length, HydrationTrace::default())
    }
    pub(super) fn handle_fully_resident_traced(
        &self,
        handle: HANDLE,
        length: u64,
        trace: HydrationTrace,
    ) -> io::Result<bool> {
        let length = i64::try_from(length).map_err(|_| io::ErrorKind::InvalidData)?;
        if length == 0 {
            return Ok(true);
        }
        let mut ranges = vec![CF_FILE_RANGE::default(); 64];
        loop {
            let mut written = 0;
            // SAFETY: metadata query only, aligned bounded output and owned handle.
            let code = unsafe {
                (self.api.ranges)(
                    handle,
                    CF_PLACEHOLDER_RANGE_INFO_ONDISK,
                    0,
                    length,
                    ranges.as_mut_ptr().cast(),
                    std::mem::size_of_val(ranges.as_slice()) as u32,
                    &mut written,
                )
            };
            if code < 0 && (code & 0xffff) == ERROR_MORE_DATA as i32 && ranges.len() < 1_048_576 {
                ranges.resize(ranges.len() * 2, CF_FILE_RANGE::default());
                continue;
            }
            trace.hr(58, code)?;
            let bytes = written as usize;
            if !bytes.is_multiple_of(size_of::<CF_FILE_RANGE>()) || bytes > std::mem::size_of_val(ranges.as_slice()) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            return Ok(covered(&ranges[..bytes / size_of::<CF_FILE_RANGE>()], length));
        }
    }
}
impl Delivery {
    pub(super) fn restored_contents(&self, source: &received::Source) -> io::Result<Arc<dyn FileContents>> {
        Files::restore_table(
            source
                .settlement
                .restore(source.descriptor.clone(), crate::content::linked::native_connector())?,
            crate::sync::files::TargetOs::Windows,
            source.nodes.clone(),
            self.entries(),
        )
        .map(|files| files as Arc<dyn FileContents>)
    }
    pub(super) fn replace_contents(&self, contents: Arc<dyn FileContents>) -> io::Result<()> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        if let Some(delivery) = service.deliveries.lock().unwrap().get(&self.id).cloned() {
            let native: Arc<dyn NativeFiles> = delivery;
            if let Some(namespace) = contents.namespace() {
                namespace.bind_native(Arc::downgrade(&native))?;
            }
        }
        *self.contents.lock().unwrap() = contents;
        Ok(())
    }
}

impl Delivery {
    pub(super) fn complete_received(&self) -> io::Result<()> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let mut journal = service.received.lock().unwrap();
        if journal.cancelled.contains(&self.id) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let previous = journal.sources.get(&self.id).cloned().ok_or(io::ErrorKind::NotFound)?;
        let Some(descriptor) = previous.descriptor.as_ref().filter(|source| !source.provisional) else {
            return Ok(());
        };
        let mut next = previous.clone();
        let mut count = 0;
        for root in &descriptor.fulfilled {
            if next.failed.contains(&root.node) {
                continue;
            }
            let entries = completed::subtree(root.node, &next.nodes)?;
            let arrived = entries.iter().all(|entry| {
                if entry.kind == NodeKind::Directory {
                    return !next.pending.contains(&entry.id);
                }
                next.nodes
                    .iter()
                    .position(|node| node.id == entry.id)
                    .and_then(|index| journal.deliveries.get(&self.id)?.get(index))
                    .is_some_and(|item| item.handed_off && !item.deleted && !item.withdrawn)
            });
            if arrived {
                next.settlement.commit(&mut next.descriptor, root, &next.nodes)?;
                count += 1;
            }
        }
        if count == 0 {
            return Ok(());
        }
        let contents = self.restored_contents(&next)?;
        journal.sources.insert(self.id, next);
        if let Err(error) = journal.save(&service.root) {
            journal.sources.insert(self.id, previous);
            return Err(error);
        }
        drop(journal);
        self.replace_contents(contents)?;
        tracing::debug!(
            roots = count,
            "one-shot receive complete; FinalLocal without verification"
        );
        Ok(())
    }
    pub(super) fn fail_received(&self, index: usize) -> io::Result<()> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let mut journal = service.received.lock().unwrap();
        let Some(source) = journal.sources.get_mut(&self.id) else {
            return Ok(());
        };
        let Some(node) = source.nodes.get(index) else {
            return Ok(());
        };
        let Some(root) = source
            .descriptor
            .as_ref()
            .and_then(|descriptor| {
                descriptor.fulfilled.iter().find(|root| {
                    completed::subtree(root.node, &source.nodes)
                        .is_ok_and(|entries| entries.iter().any(|entry| entry.id == node.id))
                })
            })
            .map(|root| root.node)
        else {
            return Ok(());
        };
        if !source.failed.insert(root) {
            return Ok(());
        }
        let ids: HashSet<_> = completed::subtree(root, &source.nodes)?
            .iter()
            .map(|entry| entry.id)
            .collect();
        let indices: Vec<_> = source
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| ids.contains(&node.id))
            .map(|(index, _)| index)
            .collect();
        for index in &indices {
            if let Some(node) = journal
                .deliveries
                .get_mut(&self.id)
                .and_then(|nodes| nodes.get_mut(*index))
            {
                node.withdrawn = true;
            }
            self.file(*index).withdrawn.store(true, Ordering::Release);
        }
        journal.save(&service.root)?;
        drop(journal);
        self.remove_withdrawn(&service);
        tracing::debug!(items = indices.len(), "failed one-shot receive withdrawn");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resident_ranges_reject_holes_even_when_total_allocation_is_large() {
        let range = |start, length| CF_FILE_RANGE {
            StartingOffset: start,
            Length: length,
        };
        assert!(covered(&[range(0, 2), range(2, 1)], 3));
        assert!(!covered(&[range(0, 1), range(2, 4096)], 3));
        assert!(!covered(&[range(1, 4096)], 3));
        assert!(!covered(&[range(0, 2)], 3));
        assert!(!covered(&[range(0, i64::MAX), range(i64::MAX, 1)], 3));
    }
}
