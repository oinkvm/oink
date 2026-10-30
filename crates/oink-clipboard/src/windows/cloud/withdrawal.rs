//! Source disappearance revokes source reads, not ownership of a local edit.
use super::*;

impl Delivery {
    pub(super) fn source_withdrawn(&self, service: &Service, index: usize) {
        self.file(index).withdrawn.store(true, Ordering::Release);
        let mut journal = service.received.lock().unwrap();
        if let Some(node) = journal
            .deliveries
            .get_mut(&self.id)
            .and_then(|nodes| nodes.get_mut(index))
        {
            node.withdrawn = true;
        }
        if let Err(error) = journal.save(&service.root) {
            tracing::warn!(%error, "cannot persist source withdrawal; retaining native file");
        }
        // The fetch worker still owns a reader here. Maintenance retries after
        // its failed IO and all retained native consumers have drained.
    }

    pub(super) fn remove_withdrawn(&self, service: &Service) {
        if self.readers.load(Ordering::Acquire) != 0 {
            return;
        }
        for (index, file) in self.files().iter().enumerate() {
            if service
                .received
                .lock()
                .unwrap()
                .cleanup_complete
                .contains(&(self.id, index))
            {
                continue;
            }
            let withdrawn = file.withdrawn.load(Ordering::Acquire);
            if !withdrawn && !file.deleted.load(Ordering::Acquire) {
                continue;
            }
            let path = self.file_path(index);
            let mut reserved = false;
            let result = received::exclusive_delete_handle(&service.api, &path, |handle| {
                {
                    let mut journal = service.received.lock().unwrap();
                    if self.readers.load(Ordering::Acquire) != 0 {
                        return false;
                    }
                    reserved = journal.cleanup_inflight.insert((self.id, index));
                    if !reserved {
                        return false;
                    }
                }
                // The admission marker survives all native calls and the final
                // unlink. Never hold the journal mutex through a provider API.
                let proof = (|| -> io::Result<bool> {
                    if !service
                        .handle_identity(handle, 0)
                        .is_ok_and(|(owner, slot, _)| owner == self.id && slot == index)
                    {
                        return Ok(false);
                    }
                    if !withdrawn {
                        return Ok(true);
                    }
                    let mut buffer = [0u64; 1024];
                    let mut written = 0;
                    // SAFETY: owned exclusive metadata handle and aligned output.
                    hr(unsafe {
                        (service.api.info)(
                            handle,
                            CF_PLACEHOLDER_INFO_STANDARD,
                            buffer.as_mut_ptr().cast(),
                            std::mem::size_of_val(&buffer) as u32,
                            &mut written,
                        )
                    })?;
                    // SAFETY: the successful query initialized standard info.
                    let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
                    let previous = service
                        .received
                        .lock()
                        .unwrap()
                        .deliveries
                        .get(&self.id)
                        .and_then(|nodes| nodes.get(index))
                        .cloned()
                        .ok_or(io::ErrorKind::NotFound)?;
                    let clean = info.ModifiedDataSize == 0
                        && info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC
                        && !previous.local_final
                        && !file.local_final.load(Ordering::Acquire);
                    let mut next = previous.clone();
                    if clean {
                        next.withdrawn = true;
                        next.deleted = true;
                    } else {
                        // Preserve edited bytes without sending a source receipt.
                        // Dirty state and zero truncation both detach this slot.
                        // SAFETY: the protected handle owns READ_DATA, sufficient for pinning.
                        let _ = hr(unsafe {
                            (service.api.pin)(handle, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, std::ptr::null_mut())
                        });
                        let mut length = 0i64;
                        // SAFETY: live exclusive handle and fixed-size output.
                        if unsafe { GetFileSizeEx(handle, &mut length) } == 0 || length < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        next.local_final = true;
                        next.withdrawn = false;
                        next.deleted = false;
                        next.length = length as u64;
                        next.life.materialized = service.handle_fully_resident(handle, length as u64).unwrap_or(false);
                        next.handed_off = true;
                    }
                    let mut journal = service.received.lock().unwrap();
                    journal.deliveries.get_mut(&self.id).ok_or(io::ErrorKind::NotFound)?[index] = next;
                    if let Err(error) = journal.save(&service.root) {
                        journal.deliveries.get_mut(&self.id).unwrap()[index] = previous;
                        return Err(error);
                    }
                    if !clean {
                        file.local_final.store(true, Ordering::Release);
                        file.deleted.store(false, Ordering::Release);
                        file.withdrawn.store(false, Ordering::Release);
                    }
                    Ok(clean)
                })();
                proof.unwrap_or(false)
            });
            if reserved {
                service
                    .received
                    .lock()
                    .unwrap()
                    .cleanup_inflight
                    .remove(&(self.id, index));
            }
            if received::cleanup_finished(&result) {
                file.deleted.store(true, Ordering::Release);
                service.cleanup_done(self.id, index, &path);
            }
        }
    }
}
