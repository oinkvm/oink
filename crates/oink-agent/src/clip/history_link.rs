//! Holder selection belongs to the PC owner, not to protocol or transport.
use super::*;
use oink_clipboard::model::{ContentMsg, ItemId, OfferId, RepresentationId};
use oink_clipboard::sync::files::{BoxFuture, BulkChannel, BulkLink, PacingObserver};
use oink_foundation::{clipboard::ClipPlatform, clipboard_history as h, transfer::LowOptions};
use std::io;

pub(super) struct Link {
    pub files: Arc<crate::streams::clipboard::files::FileLinks>,
    pub network: oink_network::link::LinkLayer,
    pub store: Arc<oink_clipboard::history::Store>,
    pub offer: OfferId,
    pub origin: Arc<dyn BulkLink>,
    pub unavailable: Arc<Mutex<Vec<DeviceId>>>,
}
impl Link {
    fn candidates(&self) -> Vec<DeviceId> {
        let window = self.store.window();
        let unavailable = self.unavailable.lock().unwrap();
        let holders = window.holders();
        let table = self.network.links();
        let mut peers: Vec<_> = table
            .links
            .iter()
            .filter(|entry| {
                entry.state == oink_network::link::LinkState::Ready
                    && !self.files.is_phone(entry.peer)
                    && !unavailable.contains(&entry.peer)
                    && (holders.iter().any(|h| {
                        h.record_id == self.offer && h.holder == entry.peer && h.state == h::HolderState::Holding
                    }) || entry.peer == self.offer.clip.origin
                        && !holders.iter().any(|h| {
                            h.record_id == self.offer && h.holder == entry.peer && h.state != h::HolderState::Holding
                        }))
            })
            .map(|entry| {
                let holding = holders
                    .iter()
                    .any(|h| h.record_id == self.offer && h.holder == entry.peer && h.state == h::HolderState::Holding);
                let priority = if holding && matches!(entry.path, oink_network::link::LinkPath::Direct) {
                    0
                } else if entry.peer == self.offer.clip.origin {
                    1
                } else {
                    2
                };
                (priority, entry.peer)
            })
            .collect();
        peers.sort();
        peers.into_iter().map(|(_, id)| id).collect()
    }
    fn supports_request(&self, peer: DeviceId, request: &ContentMsg) -> bool {
        let mask = match request {
            ContentMsg::ReadRepresentation { accepted_format, .. } => [
                h::PortableFormat::Text,
                h::PortableFormat::Html,
                h::PortableFormat::Rtf,
                h::PortableFormat::Png,
                h::PortableFormat::Url,
            ]
            .into_iter()
            .find(|format| {
                oink_clipboard::negotiation::portable_format(oink_clipboard::history::standard(*format))
                    == accepted_format
            })
            .map(|format| 1u32 << format as u32),
            ContentMsg::CloudLinkRequest { .. } => Some(h::HolderScope::FILES.0),
            _ => None,
        };
        let Some(mask) = mask else {
            return peer == self.offer.clip.origin;
        };
        self.store
            .window()
            .holders()
            .iter()
            .find(|e| e.record_id == self.offer && e.holder == peer)
            .map_or(peer == self.offer.clip.origin, |e| {
                e.state == h::HolderState::Holding && e.scope.0 & mask != 0
            })
    }
    async fn open_holder(&self, peer: DeviceId, options: LowOptions) -> io::Result<Box<dyn BulkChannel>> {
        self.files.accept(peer, self.offer.clip);
        self.files
            .clone()
            .bind(peer, self.offer.clip)
            .open_with_options(peer, options)
            .await
    }
}
impl BulkLink for Link {
    fn bind(self: Arc<Self>, _: DeviceId, _: ClipId) -> Arc<dyn BulkLink> {
        self
    }
    fn peer_label(&self, peer: DeviceId) -> String {
        self.files.peer_label(peer)
    }
    fn supports_cloud_links(&self) -> bool {
        self.origin.supports_cloud_links()
    }
    fn supports_namespace(&self) -> bool {
        self.origin.supports_namespace()
    }
    fn bind_persistent(self: Arc<Self>, peer: DeviceId) -> io::Result<Arc<dyn BulkLink>> {
        self.origin.clone().bind_persistent(peer)
    }
    // The origin going offline does not revoke another holder's authority.
    // Individual opened channels still retain FileLinks' pause/session guards.
    fn select_representation(
        &self,
        inventory: &Inventory,
        representation: RepresentationId,
        target: ClipPlatform,
    ) -> io::Result<(ItemId, String)> {
        if let Some(rep) = content::find(inventory, representation)
            && oink_clipboard::history::portable(rep.standard).is_some()
        {
            for peer in self.candidates() {
                if let Some(entry) = self.network.links().by_peer(peer)
                    && let Some(profile) = &entry.peer_formats
                    && let Ok(selected) = oink_clipboard::negotiation::negotiated_selection(
                        profile,
                        &oink_clipboard::negotiation::formats(),
                        inventory,
                        representation,
                        target,
                    )
                {
                    return Ok(selected);
                }
            }
        }
        self.origin.select_representation(inventory, representation, target)
    }
    fn open(&self, _: DeviceId) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async { Err(io::ErrorKind::InvalidInput.into()) })
    }
    fn open_with_options(&self, _: DeviceId, options: LowOptions) -> BoxFuture<'_, io::Result<Box<dyn BulkChannel>>> {
        Box::pin(async move {
            let mut candidates = self.candidates();
            while !candidates.is_empty() {
                let peer = candidates.remove(0);
                if let Ok(channel) = self.open_holder(peer, options).await {
                    let link = Arc::new(Link {
                        files: self.files.clone(),
                        network: self.network.clone(),
                        store: self.store.clone(),
                        offer: self.offer,
                        origin: self.origin.clone(),
                        unavailable: self.unavailable.clone(),
                    });
                    let validity = Arc::new(Mutex::new(channel.effect_validity()));
                    return Ok(Box::new(Channel {
                        channel,
                        link,
                        peer,
                        remaining: candidates,
                        options,
                        request: None,
                        validity,
                    }) as Box<dyn BulkChannel>);
                }
            }
            Err(io::ErrorKind::NotConnected.into())
        })
    }
}
type Validity = Arc<dyn Fn() -> bool + Send + Sync>;
struct Channel {
    validity: Arc<Mutex<Validity>>,
    channel: Box<dyn BulkChannel>,
    link: Arc<Link>,
    peer: DeviceId,
    remaining: Vec<DeviceId>,
    options: LowOptions,
    request: Option<ContentMsg>,
}
impl BulkChannel for Channel {
    fn diagnostic_id(&self) -> [u8; 16] {
        self.channel.diagnostic_id()
    }
    fn pacing_observer(&self) -> PacingObserver {
        self.channel.pacing_observer()
    }
    fn transfer_path(&self) -> &'static str {
        self.channel.transfer_path()
    }
    fn effect_validity(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let validity = self.validity.clone();
        Arc::new(move || validity.lock().unwrap()())
    }
    fn finish(&mut self) {
        self.channel.finish();
    }
    fn send<'a>(&'a mut self, request: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
        self.request = Some(request.clone());
        Box::pin(async move {
            if !self.link.supports_request(self.peer, request) {
                let mut replacement = None;
                while !self.remaining.is_empty() {
                    let next = self.remaining.remove(0);
                    if !self.link.supports_request(next, request) {
                        continue;
                    }
                    if let Ok(channel) = self.link.open_holder(next, self.options).await {
                        replacement = Some((next, channel));
                        break;
                    }
                }
                let Some((peer, channel)) = replacement else {
                    return Err(content::unavailable(Unavailable::NoSuchItem));
                };
                *self.validity.lock().unwrap() = channel.effect_validity();
                self.channel = channel;
                self.peer = peer;
            }
            self.channel.send(request).await
        })
    }
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
        Box::pin(async move {
            loop {
                let response = self.channel.recv().await?;
                let missing = matches!(
                    &response,
                    Some(
                        ContentMsg::RepresentationFailed {
                            reason: Unavailable::NoSuchItem,
                            ..
                        } | ContentMsg::CloudLinkUnavailable {
                            reason: Unavailable::NoSuchItem,
                            ..
                        }
                    )
                );
                if !missing || self.remaining.is_empty() {
                    return Ok(response);
                }
                // A reader only records a local observation. It never forges a
                // Gone revision for the remote holder; that provider authors it.
                self.link.unavailable.lock().unwrap().push(self.peer);
                let mut replacement = None;
                while !self.remaining.is_empty() {
                    let next = self.remaining.remove(0);
                    if !self
                        .link
                        .supports_request(next, self.request.as_ref().ok_or(io::ErrorKind::InvalidData)?)
                    {
                        continue;
                    }
                    if let Ok(mut channel) = self.link.open_holder(next, self.options).await {
                        channel
                            .send(self.request.as_ref().ok_or(io::ErrorKind::InvalidData)?)
                            .await?;
                        replacement = Some((next, channel));
                        break;
                    }
                }
                let Some((next, channel)) = replacement else {
                    return Ok(response);
                };
                *self.validity.lock().unwrap() = channel.effect_validity();
                self.channel = channel;
                self.peer = next;
            }
        })
    }
}
