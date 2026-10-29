//! Discover direct children on demand. The durable slot table precedes native
//! publication; retry and recovery never renumber an already exposed identity.
use super::*;
use crate::{
    content::namespace::{Files, Names},
    model::{ContentSize, NodeId, NodeKind},
};
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

/// A callback key is valid until its request is acknowledged. Weak registrations
/// let an active provider population borrow that key without blocking the native
/// callback on the population mutex or retaining it after its owner completes.
pub(super) struct Pending {
    transfer: i64,
    key: i64,
    completed: AtomicBool,
    fetch: Option<Arc<Request>>,
}
impl Pending {
    pub(super) fn register(
        delivery: &Delivery,
        index: usize,
        transfer: i64,
        key: i64,
        fetch: Option<Arc<Request>>,
    ) -> Arc<Self> {
        let pending = Arc::new(Self {
            transfer,
            key,
            completed: AtomicBool::new(false),
            fetch,
        });
        let mut requests = delivery.population_requests.lock().unwrap();
        let requests = requests.entry(index).or_default();
        requests.retain(|request| request.strong_count() != 0);
        requests.push(Arc::downgrade(&pending));
        pending
    }
    fn operation(&self, service: &Service) -> io::Result<CF_OPERATION_INFO> {
        let connection = service.connection.lock().unwrap().ok_or(io::ErrorKind::NotConnected)?;
        Ok(operation(connection, self.transfer, self.key))
    }
}

fn transfer(
    api: &Api,
    op: &CF_OPERATION_INFO,
    entries: &mut [CF_PLACEHOLDER_CREATE_INFO],
    status: NTSTATUS,
    total: usize,
    trace: HydrationTrace,
) -> io::Result<()> {
    let mut op = *op;
    op.Type = CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS;
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_4>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
                Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION
                    | CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_STOP_ON_ERROR,
                CompletionStatus: status,
                PlaceholderTotalCount: total as i64,
                // A Rust empty slice has a non-null dangling pointer; Cloud
                // Filter rejects that pointer even when PlaceholderCount is zero.
                PlaceholderArray: if entries.is_empty() {
                    std::ptr::null_mut()
                } else {
                    entries.as_mut_ptr()
                },
                PlaceholderCount: entries.len() as u32,
                ..Default::default()
            },
        },
    };
    // SAFETY: all names, identities and array elements remain live through execute.
    trace.hr(73, unsafe { (api.execute)(&op, &mut params) })?;
    if status == STATUS_SUCCESS {
        for entry in entries.iter() {
            trace.hr(74, entry.Result)?;
        }
        // SAFETY: execute initialized the selected transfer-placeholders arm.
        if unsafe { params.Anonymous.TransferPlaceholders.EntriesProcessed } as usize != entries.len() {
            return trace.io(75, Err(io::ErrorKind::InvalidData.into()));
        }
    }
    Ok(())
}

impl Delivery {
    pub(super) fn populate_directory(
        &self,
        node: NodeId,
        check: &(dyn Fn() -> io::Result<()> + Send + Sync),
    ) -> io::Result<()> {
        let service = self.service.upgrade().ok_or(io::ErrorKind::NotConnected)?;
        let index = {
            let journal = service.received.lock().unwrap();
            let source = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?;
            if !source.pending.contains(&node) {
                return Ok(());
            }
            source
                .nodes
                .iter()
                .position(|entry| entry.id == node)
                .ok_or(io::ErrorKind::NotFound)?
        };
        check()?;
        let trace = HydrationTrace {
            request: Some(0),
            index,
        };
        // CfGetTransferKey on a directory is rejected by Cloud Filter (380).
        // Provider-initiated population creates children directly, then marks
        // this identity-checked directory fully populated.
        self.populate(&service, index, None, check, trace)
    }

    fn populate(
        &self,
        service: &Service,
        index: usize,
        request: Option<&Pending>,
        check: &(dyn Fn() -> io::Result<()> + Send + Sync),
        trace: HydrationTrace,
    ) -> io::Result<()> {
        let _mutation = crate::cache::Mutation(&service.root);
        let _population = self.population.lock().unwrap();
        if request.is_some_and(|request| request.completed.load(Ordering::Acquire)) {
            return Ok(());
        }
        check()?;
        let (mut source, mut items) = {
            let journal = service.received.lock().unwrap();
            if journal.cancelled.contains(&self.id) || service.stopping.load(Ordering::Acquire) {
                return Err(unavailable());
            }
            let source = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?;
            if source.descriptor.as_ref().is_some_and(|source| source.provisional) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            (
                source.clone(),
                journal.deliveries.get(&self.id).ok_or(io::ErrorKind::NotFound)?.clone(),
            )
        };
        let directory = source.nodes.get(index).ok_or(io::ErrorKind::NotFound)?.clone();
        if directory.kind != NodeKind::Directory || items[index].deleted {
            return Err(io::ErrorKind::NotFound.into());
        }
        if !source.pending.contains(&directory.id) {
            if let Some(request) = request {
                transfer(
                    &service.api,
                    &request.operation(service)?,
                    &mut [],
                    STATUS_SUCCESS,
                    0,
                    trace,
                )?;
                request.completed.store(true, Ordering::Release);
            }
            return Ok(());
        }
        let namespace = self.contents().namespace().ok_or(io::ErrorKind::Unsupported)?;
        if !source.listed.contains(&directory.id) {
            let mut cursor = None;
            let mut children = Vec::new();
            let mut ids: HashSet<_> = source.nodes.iter().map(|node| node.id).collect();
            let mut version = None;
            loop {
                check()?;
                let page = trace.io(72, namespace.list(directory.id, cursor))?;
                page.validate(cursor).map_err(crate::content::unavailable)?;
                if page.directory != directory.id || version.is_some_and(|version| version != page.version) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                version = Some(page.version);
                for mut child in page.entries {
                    if child.parent != Some(directory.id)
                        || !ids.insert(child.id)
                        || ids.len() > crate::model::MAX_DISCOVERED_NODES
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    if child.kind == NodeKind::File
                        && matches!(child.size, ContentSize::Unknown | ContentSize::Known(0))
                    {
                        let selected = namespace.select(child.id)?;
                        if selected.id != child.id
                            || selected.parent != child.parent
                            || selected.kind != child.kind
                            || selected.name != child.name
                        {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                        child = selected;
                        if child.size == ContentSize::Known(0)
                            && namespace.open(child.id, child.version, 0)?.read(&mut [0; 1])? != 0
                        {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                    }
                    if child.kind == NodeKind::File && !matches!(child.size, ContentSize::Known(_)) {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    children.push(child);
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            // Bound nesting independently of the number of pages and children.
            let mut ancestor = directory.parent;
            let mut depth = 1;
            while let Some(id) = ancestor {
                depth += 1;
                if depth > 256 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                ancestor = source
                    .nodes
                    .iter()
                    .find(|node| node.id == id)
                    .ok_or(io::ErrorKind::InvalidData)?
                    .parent;
            }
            let mut names = Names::new(crate::sync::files::TargetOs::Windows);
            for node in &source.nodes {
                names.entry(node)?;
            }
            let mut entries = self.entries();
            let parent = self.file_path(index);
            for child in children {
                let entry = names.entry(&child)?;
                if entry.size > i64::MAX as u64 || entry.path.encode_utf16().count() > 32000 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let path = parent.join(entry.path.rsplit('/').next().unwrap());
                items.push(received::Node {
                    path: path.strip_prefix(&service.root).map_err(io::Error::other)?.into(),
                    life: crate::received::Item::new(crate::received::now()),
                    directory: entry.is_dir,
                    deleted: false,
                    withdrawn: false,
                    local_final: false,
                    renamed_locally: false,
                    handed_off: false,
                    length: entry.size,
                });
                if entry.is_dir {
                    source.pending.insert(child.id);
                }
                entries.push(entry);
                source.nodes.push(child);
            }
            source.nodes[index].version = version.ok_or(io::ErrorKind::InvalidData)?;
            source.listed.insert(directory.id);
            let contents = Files::restore_table(
                namespace,
                crate::sync::files::TargetOs::Windows,
                source.nodes.clone(),
                entries.clone(),
            )?;
            check()?;
            let mut journal = service.received.lock().unwrap();
            if journal.cancelled.contains(&self.id) {
                return Err(unavailable());
            }
            // Preserve concurrent receipt/activation state and observations.
            let previous = journal.sources.get(&self.id).ok_or(io::ErrorKind::NotFound)?.clone();
            source.descriptor = previous.descriptor.clone();
            source.settlement = previous.settlement.clone();
            let old_items = journal.deliveries.get(&self.id).ok_or(io::ErrorKind::NotFound)?.clone();
            if old_items[index].deleted {
                return Err(io::ErrorKind::NotFound.into());
            }
            items[..old_items.len()].clone_from_slice(&old_items);
            journal.sources.insert(self.id, source.clone());
            journal.deliveries.insert(self.id, items.clone());
            if let Err(error) = trace.io(76, journal.save(&service.root)) {
                journal.sources.insert(self.id, previous);
                journal.deliveries.insert(self.id, old_items);
                return Err(error);
            }
            // Publish the durable slots before native callbacks can reference them.
            let mut files = self.files.lock().unwrap();
            let mut paths = self.native_paths.lock().unwrap();
            for (slot, entry) in entries.into_iter().enumerate().skip(files.len()) {
                files.push(Arc::new(DeliveredFile::new(entry, false)));
                paths.push(service.root.join(&items[slot].path));
            }
            *self.contents.lock().unwrap() = contents;
        }
        check()?;
        if self.file(index).deleted.load(Ordering::Acquire) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let mut names = Vec::new();
        let mut identities = Vec::new();
        let mut slots = Vec::new();
        for (slot, node) in source
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.parent == Some(directory.id))
        {
            let path = self.file_path(slot);
            if items[slot].deleted {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(_) if service.native_identity_matches(&path, self.id, slot) => continue,
                Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            names.push(wide(path.file_name().ok_or(io::ErrorKind::InvalidData)?));
            let mut identity = [0; 16];
            identity[..8].copy_from_slice(&self.id.to_le_bytes());
            identity[8..].copy_from_slice(&(slot as u64).to_le_bytes());
            identities.push(identity);
            slots.push((slot, node));
        }
        let mut native: Vec<_> = slots
            .iter()
            .enumerate()
            .map(|(position, (slot, node))| {
                let entry = self.file(*slot);
                let time = node
                    .modified_ms
                    .map_or(0, |ms| super::super::files::unix_ms_to_filetime(ms) as i64);
                CF_PLACEHOLDER_CREATE_INFO {
                    RelativeFileName: names[position].as_ptr(),
                    FsMetadata: CF_FS_METADATA {
                        BasicInfo: FILE_BASIC_INFO {
                            CreationTime: time,
                            LastWriteTime: time,
                            ChangeTime: time,
                            FileAttributes: if entry.entry.is_dir {
                                FILE_ATTRIBUTE_DIRECTORY
                            } else {
                                FILE_ATTRIBUTE_NORMAL
                            },
                            ..Default::default()
                        },
                        FileSize: entry.entry.size as i64,
                    },
                    FileIdentity: identities[position].as_ptr().cast(),
                    FileIdentityLength: 16,
                    Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
                    ..Default::default()
                }
            })
            .collect();
        // An exclusive RH reference makes the short native publication window
        // precede a new consumer's open/rename. If a consumer is already inside
        // Cloud Filter, use its pending callback key instead. Waiting with the
        // population mutex is safe because callback registration never takes it.
        let started = Instant::now();
        let (borrowed, _exclusive) = loop {
            if request.is_some() {
                break (None, None);
            }
            let pending = self
                .population_requests
                .lock()
                .unwrap()
                .get(&index)
                .into_iter()
                .flatten()
                .filter_map(Weak::upgrade)
                .find(|request| {
                    !request.completed.load(Ordering::Acquire)
                        && request
                            .fetch
                            .as_ref()
                            .is_none_or(|fetch| !fetch.cancelled.load(Ordering::Acquire))
                });
            if pending.is_some() {
                break (pending, None);
            }
            check()?;
            match protection::Protected::open(service.api.clone(), &self.file_path(index), CF_OPEN_FILE_FLAG_EXCLUSIVE)
            {
                Ok(file) => break (None, Some(file)),
                Err(error)
                    if matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(
                            ERROR_SHARING_VIOLATION
                                | ERROR_LOCK_VIOLATION
                                | ERROR_OPLOCK_NOT_GRANTED
                                | ERROR_CLOUD_FILE_IN_USE
                        )
                    ) || error.kind() == io::ErrorKind::WouldBlock =>
                {
                    if started.elapsed().as_millis() >= crate::model::NO_PROGRESS_MS as u128 {
                        return Err(crate::content::unavailable(crate::model::Unavailable::NoProgress));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        };
        let request = request.or(borrowed.as_deref());
        if let Some(request) = request {
            transfer(
                &service.api,
                &request.operation(service)?,
                &mut native,
                STATUS_SUCCESS,
                source
                    .nodes
                    .iter()
                    .filter(|node| node.parent == Some(directory.id))
                    .count(),
                trace,
            )?;
            request.completed.store(true, Ordering::Release);
        } else {
            // Uses the debug-only population-create helper.
            #[cfg(all(test, debug_assertions))]
            super::tests::before_population_create(self.id, index)?;
            let path = self.file_path(index);
            // The RH reference excludes new consumer opens until publication
            // and its fully-populated marker are committed. This second handle
            // only supplies WRITE_DAC; it does not request directory enumeration.
            let file = trace.io(
                70,
                std::fs::OpenOptions::new()
                    .access_mode(WRITE_DAC | FILE_READ_ATTRIBUTES)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                    .open(&path),
            )?;
            let (owner, slot, _) = service.handle_identity_traced(file.as_raw_handle(), 0, trace)?;
            if owner != self.id || slot != index {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            if !native.is_empty() {
                let mut processed = 0;
                // SAFETY: parent path, child names and identities remain live.
                trace.hr(80, unsafe {
                    (service.api.create)(
                        wide(&path).as_ptr(),
                        native.as_mut_ptr(),
                        native.len() as u32,
                        CF_CREATE_FLAG_STOP_ON_ERROR,
                        &mut processed,
                    )
                })?;
                for entry in &native {
                    trace.hr(74, entry.Result)?;
                }
                if processed as usize != native.len() {
                    return trace.io(75, Err(io::ErrorKind::InvalidData.into()));
                }
            }
            check()?;
            // SAFETY: this live directory handle has WRITE_DAC and its identity
            // was checked above. No metadata, content or identity is replaced.
            trace.hr(81, unsafe {
                (service.api.update)(
                    file.as_raw_handle(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    CF_UPDATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            })?;
        }
        check()?;
        let mut journal = service.received.lock().unwrap();
        let source = journal.sources.get_mut(&self.id).ok_or(io::ErrorKind::NotFound)?;
        source.pending.remove(&directory.id);
        source.listed.remove(&directory.id);
        if let Err(error) = journal.save(&service.root) {
            let source = journal.sources.get_mut(&self.id).unwrap();
            source.pending.insert(directory.id);
            source.listed.insert(directory.id);
            return Err(error);
        }
        drop(journal);
        self.complete_received()?;
        Ok(())
    }
}

pub(super) unsafe extern "system" fn fetch(info: *const CF_CALLBACK_INFO, _: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi supplies a live callback record and our registered weak context.
    let info = unsafe { &*info };
    // SAFETY: the boxed weak context stays allocated until disconnect drains callbacks.
    let Some(service) = (unsafe { &*info.CallbackContext.cast::<Weak<Service>>() }).upgrade() else {
        return;
    };
    let reject = || {
        let _ = transfer(
            &service.api,
            &operation(info.ConnectionKey, info.TransferKey, info.RequestKey),
            &mut [],
            STATUS_CLOUD_FILE_REQUEST_CANCELED,
            0,
            HydrationTrace::default(),
        );
    };
    if service.stopping.load(Ordering::Acquire) || info.FileIdentityLength != 16 || info.FileIdentity.is_null() {
        reject();
        return;
    }
    // SAFETY: the callback owns the checked identity bytes for this call only.
    let bytes = unsafe { std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), 16) };
    let id = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let index = u64::from_le_bytes(bytes[8..].try_into().unwrap()) as usize;
    let delivery = service.deliveries.lock().unwrap().get(&id).cloned();
    let Some(delivery) = delivery.filter(|delivery| {
        delivery
            .get_file(index)
            .is_some_and(|file| file.entry.is_dir && !file.deleted.load(Ordering::Acquire))
    }) else {
        reject();
        return;
    };
    let Ok(pin) = native::ReaderPin::new(&delivery) else {
        reject();
        return;
    };
    let request = Arc::new(Request {
        transfer: info.TransferKey,
        key: info.RequestKey,
        start: 0,
        end: 0,
        total: 0,
        progress: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
    });
    let pending = Pending::register(
        &delivery,
        index,
        info.TransferKey,
        info.RequestKey,
        Some(request.clone()),
    );
    let id = service.next.fetch_add(1, Ordering::Relaxed);
    service.requests.lock().unwrap().insert(id, request.clone());
    if service
        .jobs
        .send(Work {
            _pin: pin,
            service: service.clone(),
            delivery,
            index,
            id,
            request,
            population: Some(pending),
        })
        .is_err()
    {
        service.requests.lock().unwrap().remove(&id);
        reject();
    }
}
impl Work {
    pub(super) fn populate(&self) {
        let connection = *self.service.connection.lock().unwrap();
        if let Some(connection) = connection {
            let op = operation(connection, self.request.transfer, self.request.key);
            let check = || {
                if self.request.cancelled.load(Ordering::Acquire) || self.service.stopping.load(Ordering::Acquire) {
                    Err(unavailable())
                } else {
                    Ok(())
                }
            };
            if let Err(error) = self.delivery.populate(
                &self.service,
                self.index,
                self.population.as_deref(),
                &check,
                self.trace(),
            ) {
                let status = if self.request.cancelled.load(Ordering::Acquire) {
                    STATUS_CLOUD_FILE_REQUEST_CANCELED
                } else {
                    STATUS_CLOUD_FILE_UNSUCCESSFUL
                };
                let _ = self.delivery.fail_received(self.index);
                let _ = transfer(&self.service.api, &op, &mut [], status, 0, self.trace());
                tracing::warn!(target: "oink_file_diagnostics", file_stage = 77_u8,
                    file_request = self.id, file_index = self.index,
                    file_code = error.raw_os_error().unwrap_or(0) as u32,
                    file_status = status as u32, file_reason = crate::content::reason(&error) as u8,
                    "cloud directory population failed");
            }
        }
        self.service.requests.lock().unwrap().remove(&self.id);
    }
}
