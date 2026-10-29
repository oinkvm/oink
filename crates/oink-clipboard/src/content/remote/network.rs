use super::*;

impl Remote {
    pub(super) async fn metadata_request(&self, request: ContentMsg, receive_max: u64) -> io::Result<ContentMsg> {
        let mut backoff = Duration::from_millis(500);
        loop {
            self.valid()?;
            let result = async {
                let mut channel = self.open_channel(receive_max).await?;
                channel
                    .send(&request.clone())
                    .await?;
                loop {
                    match self.receive_content(channel.as_mut()).await? {
                        ContentMsg::Preparing { offer, node }
                            if offer == self.0.inventory.id
                                && matches!(request, ContentMsg::ListDirectory { node: id, .. } if id == node) => {}
                        ContentMsg::DirectoryFailed { offer, node, reason } | ContentMsg::FileFailed { offer, node, reason }
                            if offer == self.0.inventory.id && matches!(request, ContentMsg::ListDirectory { node: id, .. } | ContentMsg::SelectFile { node: id, .. } if id == node) => {
                            return Err(unavailable(reason));
                        }
                        ContentMsg::Failed { delivery, reason }
                            if matches!(request, ContentMsg::BeginDelivery { delivery: id, .. } if id == delivery) => {
                            return Err(unavailable(reason));
                        }
                        response => {
                            self.end_channel(channel.as_mut()).await?;
                            return Ok(response);
                        }
                    }
                }
            }
            .await;
            match result {
                Ok(response) => return Ok(response),
                Err(error) if transient(&error) => self.retry(&mut backoff).await?,
                Err(error) => return Err(error),
            }
        }
    }
    pub(super) async fn open_channel(&self, receive_max: u64) -> io::Result<Box<dyn BulkChannel>> {
        self.valid()?;
        let options = oink_foundation::transfer::LowOptions {
            send_max: 4096,
            receive_max,
            operation_max: receive_max.saturating_add(4096),
        };
        tokio::select! {
            _ = self.0.changed.notified() => Err(unavailable(Unavailable::Cancelled)),
            _ = self.0.link.cancelled() => Err(unavailable(Unavailable::Cancelled)),
            result = tokio::time::timeout_at(self.deadline(), self.0.link.open_with_options(self.0.inventory.id.clip.origin, options)) => result.map_err(|_| unavailable(Unavailable::NoProgress))?,
        }
    }
    pub(super) fn deadline_with_floor(&self, floor: Option<tokio::time::Instant>) -> tokio::time::Instant {
        let deadline = self.deadline();
        floor.map_or(deadline, |floor| deadline.max(floor))
    }
    pub(super) async fn receive_content(&self, channel: &mut dyn BulkChannel) -> io::Result<ContentMsg> {
        self.receive_content_with_floor(channel, None).await
    }
    pub(super) async fn receive_content_with_floor(
        &self,
        channel: &mut dyn BulkChannel,
        floor: Option<tokio::time::Instant>,
    ) -> io::Result<ContentMsg> {
        loop {
            self.valid()?;
            let deadline = self.deadline_with_floor(floor);
            let result = tokio::select! {
                _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
                _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
                result = tokio::time::timeout_at(deadline, channel.recv()) => result,
            };
            if !channel.effect_validity()() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            match result {
                Ok(Ok(Some(message))) => return Ok(message),
                Ok(Ok(None)) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(Err(error)) => return Err(error),
                // Other members of this delivery can make progress while this
                // directory or stream is waiting. Only the shared deadline fails.
                Err(_) if tokio::time::Instant::now() < self.deadline_with_floor(floor) => continue,
                Err(_) => return Err(unavailable(Unavailable::NoProgress)),
            }
        }
    }
    pub(super) async fn end_channel(&self, channel: &mut dyn BulkChannel) -> io::Result<()> {
        self.end_channel_with_floor(channel, None).await
    }
    pub(super) async fn end_channel_with_floor(
        &self,
        channel: &mut dyn BulkChannel,
        floor: Option<tokio::time::Instant>,
    ) -> io::Result<()> {
        channel.finish();
        loop {
            match tokio::time::timeout_at(self.deadline_with_floor(floor), channel.recv()).await {
                Ok(Ok(None)) if channel.effect_validity()() => return Ok(()),
                Ok(Ok(_)) => return Err(unavailable(Unavailable::InvalidData)),
                Ok(Err(error)) => return Err(error),
                Err(_) if tokio::time::Instant::now() < self.deadline_with_floor(floor) => continue,
                Err(_) => return Err(unavailable(Unavailable::NoProgress)),
            }
        }
    }
    async fn retry(&self, backoff: &mut Duration) -> io::Result<()> {
        self.update_progress(false, |p| p.phase = super::super::TransferPhase::Reconnecting);
        tokio::select! {
            _ = self.0.changed.notified() => return Err(unavailable(Unavailable::Cancelled)),
            _ = self.0.link.cancelled() => return Err(unavailable(Unavailable::Cancelled)),
            _ = tokio::time::sleep_until(self.deadline().min(tokio::time::Instant::now() + *backoff)) => {},
        }
        self.valid()?;
        if tokio::time::Instant::now() >= self.deadline() {
            return Err(unavailable(Unavailable::NoProgress));
        }
        *backoff = (*backoff * 2).min(Duration::from_secs(5));
        Ok(())
    }
}
fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}
