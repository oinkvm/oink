use super::*;

impl Remote {
    /// The local PrepareDrop owner supplies the actual start. Merely upgrading
    /// demand after an acceptance callback must not invent a missing start.
    pub fn record_drop_start(&self) {
        if self.0.drop_report.get().is_none() {
            // Remember even an uncollected start: a repeated PrepareDrop after
            // opting in must not backfill the already-started operation.
            let _ = self.0.drop_report.set(crate::timing::start());
        }
    }
    /// Called at the existing native session-end callback, not Begin success.
    pub fn record_drop_accepted(&self, accepted: bool) {
        if self.0.drop_report_ended.swap(true, Ordering::Relaxed) {
            return;
        }
        crate::timing::record(
            self.0.drop_report.get().copied().flatten(),
            crate::timing::Measurement::DropAccepted { accepted },
        );
    }
    pub(super) fn record_delivery(&self, outcome: crate::timing::Outcome) {
        let drag = self.0.report_drag;
        let at = if drag {
            self.0.drop_report.get().copied().flatten()
        } else {
            self.0.delivery_report.get().copied()
        };
        crate::timing::record(at, crate::timing::Measurement::Delivery { drag, outcome });
    }

    pub fn progress(&self) -> tokio::sync::watch::Receiver<super::super::TransferProgress> {
        self.0.progress.subscribe()
    }
    pub fn receipt(&self) -> Option<[u8; 32]> {
        *self.0.receipt.lock().unwrap()
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
        if let Some(at) = crate::timing::start() {
            let _ = self.0.delivery_report.set(at);
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
                        remote.record_delivery(crate::timing::Outcome::Completed);
                        *remote.0.receipt.lock().unwrap() = Some(receipt);
                        remote.update_progress(true, |p| {
                            p.phase = super::super::TransferPhase::Stored;
                            p.enumerating = false;
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
        self.record_delivery(crate::timing::Outcome::Failed(failure));
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
        if let Some(files) = &self.0.files {
            files.cancel();
        }
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
        // The native OS provider owns every received file. This task requests
        // hydration and observes completion; it never allocates a backing file.
        let mut digest = blake3::Hasher::new();
        digest.update(b"oink-native-delivery-v1");
        // This digest identifies duplicate ACKs alongside DeliveryId; it is not
        // content verification. TLS/QUIC AEAD covers bytes (2026-10-29 decision).
        let mut file_bytes = 0u64;
        if let Some(files) = &self.0.files {
            files.prepare().await?;
            let native = files.wait_native().await?;
            let remote = self.clone();
            let received = tokio::task::spawn_blocking(move || {
                let check_remote = remote.clone();
                let check: ReaderGuard = Arc::new(move || {
                    if check_remote.0.cancelled.load(Ordering::Acquire) {
                        Err(unavailable(Unavailable::Cancelled))
                    } else {
                        Ok(())
                    }
                });
                native.download(&check, &|bytes, count| {
                    remote.update_progress(true, |p| {
                        p.phase = super::super::TransferPhase::Receiving;
                        p.bytes = bytes;
                        p.files_done = count;
                    })
                })
            })
            .await
            .map_err(|_| unavailable(Unavailable::ReadFailed))??;
            for file in &received {
                digest.update(&file.node.0.to_le_bytes());
                digest.update(&file.version.to_le_bytes());
                digest.update(&file.size.to_le_bytes());
                file_bytes = file_bytes
                    .checked_add(file.size)
                    .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
            }
            self.update_progress(true, |p| {
                p.files_done = received.len() as u64;
                p.total_files = Some(p.files_done);
            });
        } else {
            self.begin_delivery().await?;
        }
        let mut wanted = Vec::new();
        for representation in self.0.inventory.wanted(self.0.target) {
            if representation.data.size().is_some()
                && !(self.0.target == self.0.inventory.platform
                    && matches!(representation.data, RepresentationData::Inline(_)))
            {
                wanted.push((representation.id, self.request(representation.id)));
            }
        }
        for (id, slot) in wanted {
            loop {
                let changed = slot.changed.notified();
                let result = *slot.result.lock().unwrap();
                if let Some(result) = result {
                    if let Err(failure) = result {
                        // A failed companion format never revokes received files.
                        if self.0.files.is_none()
                            && matches!(
                                failure,
                                Unavailable::Superseded | Unavailable::Cancelled | Unavailable::NoProgress
                            )
                        {
                            return Err(unavailable(failure));
                        }
                        digest.update(&id.0.to_le_bytes());
                        digest.update(&[failure as u8]);
                    }
                    break;
                }
                changed.await;
            }
        }
        if self.0.cancelled.load(Ordering::Acquire) {
            return Err(unavailable(Unavailable::Cancelled));
        }
        let mut representations: Vec<_> = self
            .0
            .representations
            .lock()
            .unwrap()
            .iter()
            .map(|(id, bytes)| (*id, bytes.size))
            .collect();
        representations.sort_by_key(|(id, _)| id.0);
        let total = representations
            .iter()
            .try_fold(file_bytes, |sum, (id, size)| {
                digest.update(&id.0.to_le_bytes());
                digest.update(&size.to_le_bytes());
                sum.checked_add(*size)
            })
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        self.update_progress(false, |p| {
            p.total_bytes = Some(total);
            p.bytes = total;
            p.enumerating = false;
        });
        Ok(*digest.finalize().as_bytes())
    }
}
