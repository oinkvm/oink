//! Persistent remote files. Credentials are stored alongside the native OS
//! placeholder. Reads keep at most one verified block in memory and write no
//! receiver cache. Dropping a reader closes its network operation immediately.
use super::{grants::LinkToken, namespace::Namespace, reason, terminal_reader, unavailable};
use crate::sync::files::{BulkChannel, BulkLink};
use crate::{
    FileReader,
    model::{
        ContentMsg, ContentSize, DIRECTORY_PAGE_BYTES, Demand, DirectoryCursor, DirectoryPage, FileNode,
        NO_PROGRESS_MS, NodeId, NodeKind, Unavailable,
    },
};
use oink_foundation::{DeviceId, content::OfferId};
use std::{
    collections::HashSet,
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
const BLOCK: u64 = 1024 * 1024;
const METADATA_MAX: u64 = 64 * 1024 * 1024;

pub mod completed;
mod connector;
mod publication;
pub use connector::{Connection, Connector, native_connector};
pub use publication::Publication;

/// Private credentials, persisted only in the native received-item catalog.
/// The offer is a correlation identifier, never a source-process authority.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Descriptor {
    pub offer: OfferId,
    pub token: LinkToken,
    pub provisional: bool,
    pub roots: Vec<FileNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fulfilled: Vec<super::grants::FulfilledRoot>,
    #[serde(
        default,
        skip_serializing_if = "super::native_metadata::NativeFileMetadata::is_empty"
    )]
    pub native: super::native_metadata::NativeFileMetadata,
}
impl std::fmt::Debug for Descriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceLink").field("roots", &self.roots.len()).finish()
    }
}
impl Descriptor {
    pub fn validate(&self) -> io::Result<()> {
        let mut ids = HashSet::new();
        if self.roots.is_empty()
            || self.roots.len() > u16::MAX as usize
            || self
                .roots
                .iter()
                .any(|root| !root.valid() || root.parent.is_some() || !ids.insert(root.id))
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        // Bound the complete catalog field, including names, before accepting it.
        let bytes = self
            .roots
            .iter()
            .try_fold(0usize, |bytes, root| bytes.checked_add(root.name.len() + 128));
        if bytes.is_none_or(|bytes| bytes > METADATA_MAX as usize) {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        self.native
            .validate(&self.roots.iter().map(|root| root.id).collect::<Vec<_>>())?;
        let mut fulfilled = HashSet::new();
        if self
            .fulfilled
            .iter()
            .any(|root| !ids.contains(&root.node) || !fulfilled.insert(root.node))
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Linked(Arc<Inner>);
impl std::fmt::Debug for Linked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.descriptor.fmt(f)
    }
}
struct Inner {
    descriptor: Descriptor,
    link: Arc<dyn BulkLink>,
    runtime: tokio::runtime::Handle,
    cancelled: AtomicBool,
    changed: tokio::sync::Notify,
}
impl Linked {
    /// Obtain the source grant while the original clipboard/drop is authorized.
    /// Publication must wait for this result and the native catalog's commit.
    pub async fn acquire(
        offer: OfferId,
        roots: Vec<NodeId>,
        demand: Demand,
        link: Arc<dyn BulkLink>,
    ) -> io::Result<Self> {
        if roots.is_empty()
            || roots.len() > u16::MAX as usize
            || roots.iter().collect::<HashSet<_>>().len() != roots.len()
        {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let bound = link.clone().bind(offer.clip.origin, offer.clip);
        if !bound.supports_cloud_links() {
            return Err(unavailable(Unavailable::Unsupported));
        }
        let request = ContentMsg::CloudLinkRequest {
            offer,
            roots: roots.clone(),
            demand,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(NO_PROGRESS_MS);
        let operation = async {
            let mut channel = bound
                .open_with_options(offer.clip.origin, options(&request, METADATA_MAX)?)
                .await?;
            channel.send(&request).await?;
            let response = channel.recv().await?.ok_or(io::ErrorKind::UnexpectedEof)?;
            channel.finish();
            if channel.recv().await?.is_some() || !channel.effect_validity()() {
                return Err(unavailable(Unavailable::Cancelled));
            }
            match response {
                ContentMsg::CloudLinkOffer {
                    offer: got,
                    token,
                    roots: received,
                    fulfilled,
                    native,
                } if got == offer => {
                    let received_ids = received.iter().map(|root| root.id).collect::<Vec<_>>();
                    if native.resolve_roots(&roots, &received_ids).ok().as_ref() != Some(&received_ids)
                        && (received_ids.is_empty() || received_ids.iter().any(|id| !roots.contains(id)))
                    {
                        return Err(unavailable(Unavailable::InvalidData));
                    }
                    Self::restore(
                        Descriptor {
                            offer,
                            token,
                            provisional: matches!(demand, Demand::Hover(_)),
                            roots: received,
                            fulfilled,
                            native,
                        },
                        link,
                        tokio::runtime::Handle::current(),
                    )
                }
                ContentMsg::CloudLinkUnavailable { offer: got, reason } if got == offer => {
                    tracing::warn!(target: "oink_file_diagnostics", file_stage = 43_u8,
                        file_offer = offer.clip.seq, file_generation = offer.generation,
                        file_reason = ?reason, "source file namespace rejection received");
                    Err(unavailable(reason))
                }
                _ => Err(unavailable(Unavailable::InvalidData)),
            }
        };
        tokio::select! {
            _ = bound.cancelled() => Err(unavailable(Unavailable::Cancelled)),
            result = tokio::time::timeout_at(deadline, operation) => result.map_err(|_| unavailable(Unavailable::NoProgress))?,
        }
    }
    /// Restoring is metadata-only and succeeds offline. Each later operation
    /// authenticates the current link and checks the source's token map again.
    pub fn restore(
        descriptor: Descriptor,
        link: Arc<dyn BulkLink>,
        runtime: tokio::runtime::Handle,
    ) -> io::Result<Self> {
        descriptor.validate()?;
        let link = link.bind_persistent(descriptor.offer.clip.origin)?;
        Ok(Self(Arc::new(Inner {
            descriptor,
            link,
            runtime,
            cancelled: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        })))
    }
    pub fn descriptor(&self) -> &Descriptor {
        &self.0.descriptor
    }
    fn valid(&self) -> io::Result<()> {
        if self.0.cancelled.load(Ordering::Acquire) {
            Err(unavailable(Unavailable::Cancelled))
        } else {
            Ok(())
        }
    }
    async fn bounded<T>(&self, work: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
        // Enable before checking the flag, so cancellation cannot fall between
        // the validity check and registration of a waiting network operation.
        let changed = self.0.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        self.valid()?;
        tokio::select! {
            _ = changed => Err(unavailable(Unavailable::Cancelled)),
            result = tokio::time::timeout(Duration::from_millis(NO_PROGRESS_MS), work) => {
                self.valid()?;
                result.map_err(|_| unavailable(Unavailable::NoProgress))?
            }
        }
    }
    async fn channel(&self, request: &ContentMsg, receive_max: u64) -> io::Result<Box<dyn BulkChannel>> {
        self.bounded(async {
            let mut channel = self
                .0
                .link
                .open_with_options(self.0.descriptor.offer.clip.origin, options(request, receive_max)?)
                .await?;
            channel.send(request).await?;
            Ok(channel)
        })
        .await
    }
    async fn receive(&self, channel: &mut dyn BulkChannel) -> io::Result<Option<ContentMsg>> {
        let message = self.bounded(channel.recv()).await?;
        if !channel.effect_validity()() {
            return Err(unavailable(Unavailable::Cancelled));
        }
        Ok(message)
    }
    async fn finish(&self, channel: &mut dyn BulkChannel) -> io::Result<()> {
        channel.finish();
        if self.receive(channel).await?.is_some() {
            return Err(unavailable(Unavailable::InvalidData));
        }
        Ok(())
    }
    async fn metadata(&self, request: ContentMsg, budget: u64) -> io::Result<ContentMsg> {
        let mut channel = self.channel(&request, budget).await?;
        let reply = self
            .receive(channel.as_mut())
            .await?
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        self.finish(channel.as_mut()).await?;
        Ok(reply)
    }
    pub async fn directory(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        let Descriptor { offer, token, .. } = self.descriptor();
        match self
            .metadata(
                ContentMsg::CloudLinkList {
                    offer: *offer,
                    token: *token,
                    node,
                    cursor,
                },
                DIRECTORY_PAGE_BYTES as u64 + 65536,
            )
            .await?
        {
            ContentMsg::Directory { offer: got, page } if got == *offer && page.directory == node => {
                page.validate(cursor).map_err(unavailable)?;
                Ok(page)
            }
            ContentMsg::DirectoryFailed {
                offer: got,
                node: id,
                reason,
            } if got == *offer && id == node => Err(unavailable(reason)),
            _ => Err(unavailable(Unavailable::InvalidData)),
        }
    }
    pub async fn selected(&self, node: NodeId) -> io::Result<FileNode> {
        let Descriptor { offer, token, .. } = self.descriptor();
        match self
            .metadata(
                ContentMsg::CloudLinkSelect {
                    offer: *offer,
                    token: *token,
                    node,
                },
                65536,
            )
            .await?
        {
            ContentMsg::FileSelected { offer: got, entry }
                if got == *offer
                    && entry.id == node
                    && entry.kind == NodeKind::File
                    && entry.valid()
                    && entry.version != 0
                    && matches!(entry.size, ContentSize::Known(_)) =>
            {
                Ok(entry)
            }
            ContentMsg::FileFailed {
                offer: got,
                node: id,
                reason,
            } if got == *offer && id == node => Err(unavailable(reason)),
            _ => Err(unavailable(Unavailable::InvalidData)),
        }
    }
}
fn options(request: &ContentMsg, receive_max: u64) -> io::Result<oink_foundation::transfer::LowOptions> {
    let send_max = oink_protocol::v1::frame::encode(
        &crate::wire::content(request).map_err(|_| unavailable(Unavailable::InvalidData))?,
        oink_protocol::v1::limits::LOW_FRAME,
    )
    .map_err(|_| unavailable(Unavailable::InvalidData))?
    .len() as u64;
    Ok(oink_foundation::transfer::LowOptions {
        send_max,
        receive_max,
        operation_max: send_max
            .checked_add(receive_max)
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?,
    })
}
impl Namespace for Linked {
    fn native_file_metadata(&self) -> super::native_metadata::NativeFileMetadata {
        self.0.descriptor.native.clone()
    }
    fn source_link(&self) -> Option<Descriptor> {
        Some(self.descriptor().clone())
    }
    fn origin(&self) -> Option<DeviceId> {
        Some(self.0.descriptor.offer.clip.origin)
    }
    fn roots(&self) -> Vec<FileNode> {
        self.0.descriptor.roots.clone()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.0.runtime.block_on(self.directory(node, cursor))
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        self.0.runtime.block_on(self.selected(node))
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        let report_start = (offset == 0).then(crate::timing::start).flatten();
        if self.descriptor().provisional {
            return Err(unavailable(Unavailable::Permission));
        }
        let entry = self.select(node)?;
        if version != entry.version {
            return Err(unavailable(Unavailable::Superseded));
        }
        let ContentSize::Known(size) = entry.size else {
            unreachable!()
        };
        if offset > size {
            return Err(unavailable(Unavailable::InvalidData));
        }
        // Native readers may seek arbitrarily. Fetch the containing block and
        // discard its prefix in memory, without reading the preceding file.
        let start = offset / BLOCK * BLOCK;
        let left = size - start;
        let budget = left
            .checked_add(left.div_ceil(BLOCK).saturating_add(2).saturating_mul(512))
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        let Descriptor { offer, token, .. } = self.descriptor();
        let request = ContentMsg::CloudLinkRead {
            offer: *offer,
            token: *token,
            node,
            version,
            offset: start,
        };
        let mut report = crate::timing::FileRead::new(report_start, size);
        let channel = match self.0.runtime.block_on(self.channel(&request, budget)) {
            Ok(channel) => channel,
            Err(error) => {
                report.finish(crate::timing::Outcome::Failed(reason(&error)));
                return Err(error);
            }
        };
        Ok(terminal_reader(Box::new(Reader {
            files: self.clone(),
            report,
            channel,
            node,
            version,
            size,
            at: start,
            skip: (offset - start) as usize,
            buffer: io::Cursor::new(Vec::new()),
            complete: false,
            failed: false,
        })))
    }
    fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
    }
}
struct Reader {
    files: Linked,
    report: crate::timing::FileRead,
    channel: Box<dyn BulkChannel>,
    node: NodeId,
    version: u64,
    size: u64,
    at: u64,
    skip: usize,
    buffer: io::Cursor<Vec<u8>>,
    complete: bool,
    failed: bool,
}
impl Reader {
    fn transport_lost(&self, error: &io::Error) -> bool {
        if let Some(content) = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<super::ContentError>())
        {
            return content.0 == Unavailable::Cancelled && !self.channel.effect_validity()();
        }
        matches!(
            error.kind(),
            io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::NotConnected
        ) || (error.kind() == io::ErrorKind::UnexpectedEof && !self.channel.effect_validity()())
    }
    async fn reopen(&mut self) -> io::Result<()> {
        if self.buffer.position() < self.buffer.get_ref().len() as u64 {
            // Reauthorize even buffered bytes on the new authenticated channel.
            // The selected metadata version binds the re-fetched suffix.
            self.at -= self.buffer.get_ref().len() as u64;
            self.skip = self.buffer.position() as usize;
        }
        self.buffer = io::Cursor::new(Vec::new());
        self.complete = false;
        let descriptor = self.files.descriptor();
        let request = ContentMsg::CloudLinkRead {
            offer: descriptor.offer,
            token: descriptor.token,
            node: self.node,
            version: self.version,
            offset: self.at,
        };
        let left = self.size - self.at;
        let budget = left
            .checked_add(left.div_ceil(BLOCK).saturating_add(2).saturating_mul(512))
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        let mut delay = Duration::from_millis(100);
        loop {
            self.files.valid()?;
            if self.files.0.link.is_cancelled() {
                return Err(unavailable(Unavailable::Cancelled));
            }
            match self.files.channel(&request, budget).await {
                Ok(channel) => {
                    self.channel = channel;
                    return Ok(());
                }
                Err(error) if self.transport_lost(&error) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
                Err(error) => return Err(error),
            }
        }
    }
    async fn ready(&mut self) -> io::Result<()> {
        loop {
            self.files.valid()?;
            if self.files.0.link.is_cancelled() {
                return Err(unavailable(Unavailable::Cancelled));
            }
            if !self.channel.effect_validity()() {
                self.reopen().await?;
            }
            if self.buffer.position() < self.buffer.get_ref().len() as u64 || self.complete {
                return Ok(());
            }
            self.buffer = io::Cursor::new(Vec::new());
            match self.next().await {
                Ok(()) => return Ok(()),
                Err(error) if self.transport_lost(&error) => self.reopen().await?,
                Err(error) => return Err(error),
            }
        }
    }

    async fn terminal(&mut self) -> io::Result<()> {
        match self
            .files
            .receive(self.channel.as_mut())
            .await?
            .ok_or(io::ErrorKind::UnexpectedEof)?
        {
            ContentMsg::FileEnd {
                offer,
                node,
                version,
                size,
            } if offer == self.files.descriptor().offer
                && node == self.node
                && version == self.version
                && size == self.size
                && self.at == self.size =>
            {
                self.files.finish(self.channel.as_mut()).await?;
                self.complete = true;
                Ok(())
            }
            ContentMsg::FileFailed { offer, node, reason }
                if offer == self.files.descriptor().offer && node == self.node =>
            {
                Err(unavailable(reason))
            }
            _ => Err(unavailable(Unavailable::InvalidData)),
        }
    }
    async fn next(&mut self) -> io::Result<()> {
        if self.at == self.size {
            return self.terminal().await;
        }
        match self
            .files
            .receive(self.channel.as_mut())
            .await?
            .ok_or(io::ErrorKind::UnexpectedEof)?
        {
            ContentMsg::FileBlock {
                offer,
                node,
                version,
                offset,
                bytes,
            } if offer == self.files.descriptor().offer
                && node == self.node
                && version == self.version
                && offset == self.at
                && bytes.len() as u64 == BLOCK.min(self.size - self.at)
                && !bytes.is_empty() =>
            {
                self.at += bytes.len() as u64;
                // Readers often stop after the advertised length without an
                // extra EOF call. Do not expose the final block before EOF.
                if self.at == self.size
                    && let Err(error) = self.terminal().await
                {
                    // No byte of this block was exposed: retry its original
                    // offset, including any seek prefix still to discard.
                    self.at -= bytes.len() as u64;
                    return Err(error);
                }
                self.buffer = io::Cursor::new(bytes);
                self.buffer.set_position(self.skip as u64);
                self.skip = 0;
                Ok(())
            }
            ContentMsg::FileFailed { offer, node, reason }
                if offer == self.files.descriptor().offer && node == self.node =>
            {
                Err(unavailable(reason))
            }
            _ => Err(unavailable(Unavailable::InvalidData)),
        }
    }
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(unavailable(Unavailable::ReadFailed));
        }
        let result = (|| {
            let files = self.files.clone();
            // A single deadline covers opening, retry backoff and receiving the
            // next verified block. Cancellation interrupts every wait.
            files.0.runtime.block_on(files.bounded(self.ready()))?;
            self.buffer.read(bytes)
        })();
        match &result {
            Ok(bytes) => {
                if *bytes > 0 {
                    self.report.first_byte();
                }
                if self.complete {
                    self.report.finish(crate::timing::Outcome::Completed);
                }
            }
            Err(error) => self.report.finish(crate::timing::Outcome::Failed(reason(error))),
        }
        if result.is_err() {
            self.failed = true;
            self.buffer = io::Cursor::new(Vec::new());
        }
        result
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;
