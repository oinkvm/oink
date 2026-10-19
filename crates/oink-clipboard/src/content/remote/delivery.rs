use super::super::manifest::Manifest;
use super::*;
use crate::model::NodeKind;
impl Remote {
    /// Retain only the offer's network capability, never the completed payload
    /// or its owner pin. A later read is authorized anew by the source registry.
    pub(super) fn refetcher(&self) -> Arc<dyn Fn() -> io::Result<super::super::Completed> + Send + Sync> {
        let data = self.0.batch.path().parent().unwrap().parent().unwrap().to_owned();
        let inventory = self.0.inventory.clone();
        let wire_inventory = self.0.wire_inventory.clone();
        let target = self.0.target;
        let link = self.0.link.clone();
        let runtime = self.0.runtime.clone();
        let demand = match *self.0.demand.lock().unwrap() {
            Demand::Hover(holder) => Demand::Drop(holder),
            demand => demand,
        };
        let receiver = *self.0.receiver.lock().unwrap();
        Arc::new(move || {
            let remote = Remote::on_wire_runtime(
                &data,
                BatchId::random()?,
                wire_inventory.clone(),
                target,
                link.clone(),
                demand,
                runtime.clone(),
            )?;
            if let Some(receiver) = receiver {
                remote.set_receiver(receiver);
            }
            // Do not expose a new partial file or reuse a cached receipt. The
            // source must still authorize this offer and the whole delivery must
            // pass its normal integrity/timeout checks before reopening locally.
            runtime.block_on(async {
                // BeginDelivery has no negative response variant. Validate the
                // current offer with a metadata request first so a superseded
                // source returns its typed failure instead of an EOF retry loop.
                if let Some(root) = inventory.namespace_roots_for(target).first() {
                    let request = if root.kind == NodeKind::Directory {
                        ContentMsg::ListDirectory {
                            offer: inventory.id,
                            node: root.id,
                            cursor: None,
                            demand,
                        }
                    } else {
                        ContentMsg::SelectFile {
                            offer: inventory.id,
                            node: root.id,
                            demand,
                        }
                    };
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        remote.metadata_request(request, crate::model::DIRECTORY_PAGE_BYTES as u64 + 65536),
                    )
                    .await
                    .map_err(|_| unavailable(Unavailable::NoProgress))??;
                }
                if !remote.start_delivery() {
                    return Err(unavailable(Unavailable::Cancelled));
                }
                let mut progress = remote.progress();
                progress
                    .wait_for(|p| {
                        matches!(
                            p.phase,
                            super::super::TransferPhase::Stored
                                | super::super::TransferPhase::Failed
                                | super::super::TransferPhase::Cancelled
                        )
                    })
                    .await
                    .map_err(|_| unavailable(Unavailable::Cancelled))?;
                if progress.borrow().phase != super::super::TransferPhase::Stored {
                    return Err(unavailable(
                        progress.borrow().failure.unwrap_or(Unavailable::NoSuchItem),
                    ));
                }
                super::super::Completed::restore(&data, remote.0.batch.id())
            })
        })
    }
    /// File Provider's explicit-download fallback must discover a known bounded
    /// total before opening any file. Unknown length is not a zero-sized file.
    pub async fn prefetch_bounded(&self, maximum: u64) -> io::Result<super::super::Completed> {
        let result = async {
            self.begin_delivery().await?;
            let nodes = self.expand().await?;
            let mut total = 0u64;
            for node in nodes.iter().filter(|node| node.kind == NodeKind::File) {
                let demand = *self.0.demand.lock().unwrap();
                let selected = self
                    .metadata_request(
                        ContentMsg::SelectFile {
                            offer: self.0.inventory.id,
                            node: node.id,
                            demand,
                        },
                        65536,
                    )
                    .await?;
                let ContentMsg::FileSelected { offer, entry } = selected else {
                    return Err(unavailable(Unavailable::InvalidData));
                };
                if offer != self.0.inventory.id
                    || entry.id != node.id
                    || entry.parent != node.parent
                    || entry.name != node.name
                    || entry.kind != NodeKind::File
                {
                    return Err(unavailable(Unavailable::InvalidData));
                }
                let ContentSize::Known(size) = entry.size else {
                    return Err(unavailable(Unavailable::Unsupported));
                };
                self.update_progress(true, |_| {});
                total = total
                    .checked_add(size)
                    .filter(|sum| *sum <= maximum)
                    .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
            }
            self.0.max_files.fetch_min(maximum, Ordering::AcqRel);
            if !self.start_delivery() {
                return Err(unavailable(Unavailable::Cancelled));
            }
            let mut progress = self.progress();
            progress
                .wait_for(|p| {
                    matches!(
                        p.phase,
                        super::super::TransferPhase::Stored
                            | super::super::TransferPhase::Failed
                            | super::super::TransferPhase::Cancelled
                    )
                })
                .await
                .map_err(|_| unavailable(Unavailable::Cancelled))?;
            if progress.borrow().phase != super::super::TransferPhase::Stored {
                return Err(unavailable(progress.borrow().failure.unwrap_or(Unavailable::Cancelled)));
            }
            let data = self.0.batch.path().parent().unwrap().parent().unwrap().to_owned();
            let id = self.0.batch.id();
            tokio::task::spawn_blocking(move || super::super::Completed::restore(&data, id))
                .await
                .map_err(|_| unavailable(Unavailable::ReadFailed))?
        }
        .await;
        if let Err(error) = &result {
            self.fail_delivery(reason(error));
            if !self.0.delivery_started.load(Ordering::Acquire) {
                let remote = self.clone();
                self.0.runtime.spawn(async move {
                    remote.report_terminal().await;
                });
            }
        }
        result
    }
    pub fn progress(&self) -> tokio::sync::watch::Receiver<super::super::TransferProgress> {
        self.0.progress.subscribe()
    }
    pub fn receipt(&self) -> Option<[u8; 32]> {
        *self.0.receipt.lock().unwrap()
    }
    /// Clipboard's configured file budget applies while discovering the tree;
    /// DnD leaves the default unlimited file budget. Raw budgets stay separate.
    pub fn set_file_budget(&self, bytes: u64) {
        self.0.max_files.store(bytes, Ordering::Release);
    }
    pub(super) fn deadline(&self) -> tokio::time::Instant {
        *self.0.last_progress.lock().unwrap() + Duration::from_millis(NO_PROGRESS_MS)
    }
    pub(super) fn valid(&self) -> io::Result<()> {
        if self.0.terminal.load(Ordering::Acquire) == 2 {
            return Ok(());
        }
        if self.0.cancelled.load(Ordering::Acquire) || self.0.link.is_cancelled() {
            Err(unavailable(Unavailable::Cancelled))
        } else {
            Ok(())
        }
    }
    pub(super) fn update_progress(&self, meaningful: bool, update: impl FnOnce(&mut super::super::TransferProgress)) {
        if meaningful {
            *self.0.last_progress.lock().unwrap() = tokio::time::Instant::now();
        }
        self.0.progress.send_modify(update);
    }
    /// Native accepted and the first real Drop reader call this same operation.
    /// The task owns the whole batch, even if every application reader closes.
    pub fn start_delivery(&self) -> bool {
        if matches!(*self.0.demand.lock().unwrap(), Demand::Hover(_)) || self.valid().is_err() {
            return false;
        }
        let Some(update_activity) =
            oink_foundation::maintenance::process().enter(oink_foundation::maintenance::ActivityKind::FileTransfer)
        else {
            return false;
        };
        if self.0.delivery_started.swap(true, Ordering::AcqRel) {
            return true;
        }
        self.update_progress(true, |p| {
            p.phase = super::super::TransferPhase::Preparing;
            p.discovered = self.0.inventory.roots.len() as u64;
            p.enumerating = !self.0.inventory.roots.is_empty();
        });
        let remote = self.clone();
        self.0.runtime.spawn(async move {
            let _update_activity = update_activity;
            match remote.deliver().await {
                Ok(receipt) => {
                    // Serialize cancel against publishing Stored. The manifest
                    // itself is already durable; a cancel that won this race
                    // prevents acknowledgement and leaves cleanup a known owner.
                    if remote
                        .0
                        .terminal
                        .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        *remote.0.receipt.lock().unwrap() = Some(receipt);
                        remote.update_progress(true, |p| {
                            p.phase = super::super::TransferPhase::Stored;
                            p.enumerating = false;
                        });
                    } else {
                        let batch = remote.0.batch.clone();
                        remote.0.runtime.spawn_blocking(move || {
                            let _ = batch.abandon();
                        });
                    }
                }
                Err(error) => remote.fail_delivery(reason(&error)),
            }
            remote.report_terminal().await;
        });
        true
    }
    /// Retry the durable receipt independently of focus and application readers.
    /// Losing its ACK does not undo Stored. The source has its own bounded
    /// no-progress obligation and remembers duplicate receipts after cleanup.
    async fn report_terminal(&self) {
        let Ok(delivery) = self.delivery_id() else { return };
        let receipt = self.receipt();
        let message = match receipt {
            Some(receipt) => ContentMsg::TargetStored { delivery, receipt },
            None => ContentMsg::Failed {
                delivery,
                reason: self.0.progress.borrow().failure.unwrap_or(Unavailable::Cancelled),
            },
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(NO_PROGRESS_MS);
        let options = oink_foundation::transfer::LowOptions {
            send_max: 4096,
            receive_max: 4096,
            operation_max: 8192,
        };
        let mut backoff = Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline && !self.0.link.is_cancelled() {
            let result = tokio::time::timeout_at(deadline, async {
                let mut channel = self
                    .0
                    .link
                    .open_with_options(delivery.offer.clip.origin, options)
                    .await?;
                channel.send(&message.clone()).await?;
                let expected = ContentMsg::TerminalAck { delivery, receipt };
                if channel.recv().await? != Some(expected) {
                    return Err(unavailable(Unavailable::InvalidData));
                }
                channel.finish();
                if channel.recv().await?.is_some() || !channel.effect_validity()() {
                    return Err(unavailable(Unavailable::InvalidData));
                }
                Ok::<_, io::Error>(())
            })
            .await;
            if matches!(result, Ok(Ok(()))) {
                self.update_progress(false, |p| p.source_acknowledged = true);
                return;
            }
            tokio::select! {
                _ = self.0.link.cancelled() => return,
                _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + backoff)) => {},
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
    fn fail_delivery(&self, failure: Unavailable) {
        if self
            .0
            .terminal
            .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
        self.cancel_files(failure);
        for slot in self.0.slots.lock().unwrap().values() {
            let mut state = slot.result.lock().unwrap();
            if state.is_none() {
                *state = Some(Err(failure));
            }
            drop(state);
            slot.ready.notify_all();
            slot.changed.notify_waiters();
        }
        self.update_progress(false, |p| {
            p.phase = super::super::TransferPhase::Failed;
            p.failure = Some(failure);
        });
        let batch = self.0.batch.clone();
        self.0.runtime.spawn_blocking(move || {
            let _ = batch.abandon();
        });
    }
    async fn begin_delivery(&self) -> io::Result<()> {
        let demand = *self.0.demand.lock().unwrap();
        if matches!(demand, Demand::Hover(_)) {
            return Err(unavailable(Unavailable::Permission));
        }
        let delivery = self.delivery_id()?;
        tracing::info!(target: "oink_file_diagnostics", file_stage = 30_u8,
            file_offer = delivery.offer.clip.seq, file_generation = delivery.offer.generation,
            "file delivery begin requested");
        let reply = self
            .metadata_request(ContentMsg::BeginDelivery { delivery, demand }, 4096)
            .await?;
        tracing::info!(target: "oink_file_diagnostics", file_stage = 31_u8,
            file_offer = delivery.offer.clip.seq, file_generation = delivery.offer.generation,
            "file delivery begin response received");
        if reply
            != (ContentMsg::DeliveryReady {
                offer: delivery.offer,
                delivery,
            })
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        Ok(())
    }
    async fn deliver(&self) -> io::Result<[u8; 32]> {
        self.begin_delivery().await?;
        let mut wanted = Vec::new();
        for representation in self.0.inventory.wanted(self.0.target) {
            if representation.data.size().is_some()
                && !(self.0.target == self.0.inventory.platform
                    && matches!(representation.data, RepresentationData::Inline(_)))
            {
                wanted.push((representation.id, self.request(representation.id)));
            }
        }
        let nodes = self.expand().await?;
        let mut files = Vec::new();
        for node in nodes.iter().filter(|node| node.kind == NodeKind::File) {
            // expand() starts each file as discovered. Asking again joins the
            // same slot used by File Provider and native IStream readers.
            let slot = self.file_request(node.id)?;
            files.push(Self::wait_file(&slot).await?);
        }
        let mut inventory = self.0.inventory.clone();
        for (id, slot) in wanted {
            loop {
                let changed = slot.changed.notified();
                let result = *slot.result.lock().unwrap();
                if let Some(result) = result {
                    if let Err(failure) = result
                        && (!matches!(
                            failure,
                            Unavailable::Superseded | Unavailable::Cancelled | Unavailable::NoProgress
                        ) || (!files.is_empty()
                            && matches!(*self.0.demand.lock().unwrap(), Demand::Clipboard)
                            && matches!(failure, Unavailable::Superseded | Unavailable::NoSuchItem)))
                    {
                        // Conversion/read failures belong to this representation;
                        // successful siblings still form a usable native item.
                        // A retained disk file and a native clipboard byte format
                        // have independent authority. The file can still complete
                        // after the clipboard slot changed; record the unavailable
                        // companion explicitly in the committed inventory instead
                        // of revoking verified file bytes. Drop remains atomic.
                        for rep in inventory
                            .items
                            .iter_mut()
                            .flat_map(|item| &mut item.representations)
                            .filter(|rep| rep.id == id)
                        {
                            rep.data = RepresentationData::Unavailable(failure);
                        }
                    } else {
                        result.map_err(unavailable)?;
                    }
                    break;
                }
                changed.await;
            }
        }
        self.valid()?;
        let nodes = {
            let tree = self.0.tree.lock().unwrap();
            nodes
                .iter()
                .map(|node| tree.nodes[&node.id].clone())
                .collect::<Vec<_>>()
        };
        let total = files
            .iter()
            .try_fold(0u64, |sum, file| sum.checked_add(file.size))
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        let representations: Vec<_> = self
            .0
            .representations
            .lock()
            .unwrap()
            .iter()
            .map(|(id, (size, hash))| (*id, *size, *hash))
            .collect();
        let total = representations
            .iter()
            .try_fold(total, |sum, (_, size, _)| sum.checked_add(*size))
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        self.update_progress(false, |p| {
            p.total_bytes = Some(total);
            p.bytes = total;
        });
        let manifest = Manifest {
            inventory: crate::wire::updated_inventory(&self.0.wire_inventory, &inventory)
                .map_err(|_| unavailable(Unavailable::InvalidData))?,
            target: self.0.target,
            nodes,
            files,
            representations,
        };
        let batch = self.0.batch.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = manifest.encode()?;
            batch.commit_manifest(&bytes)
        })
        .await
        .map_err(|_| unavailable(Unavailable::ReadFailed))?
    }
}
