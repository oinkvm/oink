use super::*;
use oink_foundation::DeviceId;
use oink_mobile_core::{Peer, PeerKind, PeerState};
use oink_network::{
    discovery::{Advert, Sighting, native::NativeDiscoverySnapshot},
    link::{LinkPath, LinkState},
};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Weak,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RETRY: Duration = Duration::from_secs(4);

#[derive(Debug, Default)]
pub(super) struct Presence {
    pub stopped: bool,
    pub suspended: bool,
    generation: Option<u64>,
    retry: HashMap<DeviceId, Instant>,
    /// Each member's dial candidates of the previous pass, to notice new ones.
    candidates: HashMap<DeviceId, Vec<SocketAddr>>,
    last_seen: super::last_seen::LastSeen,
    cards: Vec<CardInput>,
    pub card_revision: u64,
    nearby_revision: u64,
    nearby: Vec<NearbyRow>,
    pair_revision: u64,
    advert: Option<Advert>,
    queried_startup_current: bool,
    signals: Vec<DeviceCardSignal>,
    signals_revision: u64,
    pub targets: TransferTargetsView,
}

#[derive(Debug, PartialEq)]
struct CardInput {
    id: DeviceId,
    name: String,
    os: CardOs,
    pc: bool,
    local: bool,
    online: bool,
    facts_version: Option<([u8; 16], u64)>,
    incompatible: bool,
    connecting: bool,
    /// When this phone last saw it online; offline cards only.
    last_seen: Option<u64>,
    /// This phone's endpoint masks (enabled, running, failed) for its own card.
    local_audio: Option<[u8; 3]>,
    /// Android privileged mode running (DeviceFacts key 32), online only.
    android_privileged: bool,
    /// Speaker and microphone mode and switch in the global audio
    /// configuration, so a change there refreshes the cards.
    audio: [(Option<oink_audio::route::Mode>, Option<bool>); 2],
}

impl Presence {
    pub(super) fn new(last_seen: super::last_seen::LastSeen) -> Self {
        Self {
            last_seen,
            ..Self::default()
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn card_os(os: &str) -> CardOs {
    match os.to_ascii_lowercase().as_str() {
        "android" => CardOs::Android,
        "ios" => CardOs::Ios,
        "macos" => CardOs::Macos,
        "windows" => CardOs::Windows,
        "linux" => CardOs::Linux,
        _ => CardOs::Other,
    }
}

fn is_mobile(os: CardOs) -> bool {
    matches!(os, CardOs::Android | CardOs::Ios)
}

fn sightings(snapshot: NativeDiscoverySnapshot, local: DeviceId) -> BTreeMap<DeviceId, Sighting> {
    let mut by_device: BTreeMap<DeviceId, Sighting> = BTreeMap::new();
    for sighting in snapshot.services.into_values() {
        let id = sighting.advert.id;
        if id == local {
            continue;
        }
        by_device
            .entry(id)
            .and_modify(|previous| {
                if !previous.advert.supports_current_protocol() && sighting.advert.supports_current_protocol() {
                    *previous = sighting.clone();
                } else if previous.advert == sighting.advert {
                    previous.addrs.extend(sighting.addrs.iter().copied());
                    previous.addrs.sort_unstable();
                    previous.addrs.dedup();
                    previous.addrs.truncate(16);
                }
            })
            .or_insert(sighting);
    }
    by_device
}

fn address_view(address: SocketAddr) -> SocketAddress {
    SocketAddress {
        address: address.ip().to_string(),
        port: address.port(),
        scope_id: match address {
            SocketAddr::V4(_) => 0,
            SocketAddr::V6(address) => address.scope_id(),
        },
    }
}

fn dialing_addresses(sighting: &Sighting) -> Vec<SocketAddr> {
    // LinkLayer is IPv4-only. Keep IPv6 endpoints in discovery snapshots, but
    // only present supported addresses to the current transport.
    sighting.addrs.iter().copied().filter(SocketAddr::is_ipv4).collect()
}

impl MobileSession {
    pub(super) fn emit_event(&self, event: MobileEvent) {
        if let Some(listener) = { lock(&self.listener).clone() } {
            listener.on_event(event);
        }
    }

    pub(super) fn begin_presence(self: &Arc<Self>) {
        let session = Arc::downgrade(self);
        let discovery = self.discovery.inner.subscribe();
        let links = self.network.subscribe_changes();
        tokio::spawn(pump(session, discovery, links));
    }

    pub(super) fn cards_view(&self) -> DeviceCardsView {
        let state = lock(&self.presence);
        let links = self.network.links();
        let mut cards: Vec<_> = state
            .cards
            .iter()
            .map(|input| {
                let rtt = links
                    .by_peer(input.id)
                    .and_then(|link| link.rtt)
                    .map(|rtt| rtt.as_secs_f64() * 1000.0);
                let (section, connection) = if input.local {
                    (CardSection::ThisDevice, CardConnection::ThisDevice)
                } else if input.online {
                    let signal = rtt.map_or(Signal::Unknown { rtt_ms: None }, |rtt_ms| Signal::Measured { rtt_ms });
                    (CardSection::Online, CardConnection::Online { signal })
                } else {
                    let reason = if input.incompatible {
                        OfflineKind::ProtocolIncompatible
                    } else if input.connecting {
                        OfflineKind::Connecting
                    } else {
                        OfflineKind::Disconnected
                    };
                    (
                        CardSection::Offline,
                        CardConnection::Offline {
                            reason,
                            signal: Signal::Offline { rtt_ms: rtt },
                        },
                    )
                };
                let (status, pause) = self.desktop_control_cards(input.id);
                let mobile_audio = is_mobile(input.os)
                    .then(|| self.mobile_audio_card(input.id, input.local))
                    .flatten();
                DeviceCard {
                    mobile_audio,
                    fingerprint: input.id.short(),
                    is_content_target_pc: input.pc,
                    device: input.id.to_hex(),
                    name: input.name.clone(),
                    os: input.os,
                    is_self: input.local,
                    section,
                    connection,
                    last_seen_ms: input.last_seen,
                    status,
                    pause,
                    android_privileged: input.android_privileged,
                }
            })
            .collect();
        cards.sort_by_key(|card| match card.section {
            CardSection::ThisDevice => 0,
            CardSection::Online => 1,
            CardSection::Offline => 2,
        });
        DeviceCardsView {
            stream: self.stream.clone(),
            revision: state.card_revision,
            cards,
            exit_volume: self.exit_volume_card().map(|(card, _)| card),
        }
    }

    pub(super) fn nearby_view(&self) -> NearbySnapshot {
        let state = lock(&self.presence);
        NearbySnapshot {
            revision: state.nearby_revision,
            rows: state.nearby.clone(),
        }
    }

    pub(super) fn compatibility_view(&self) -> Vec<PeerCompatibility> {
        lock(&self.presence)
            .cards
            .iter()
            .filter(|card| card.incompatible)
            .map(|card| PeerCompatibility {
                device: card.id.to_hex(),
                problem: Some(NegotiationProblem {
                    code: NegotiationFailureCode::Incompatible,
                    stage: NegotiationStage::Discovery,
                    missing_capability: None,
                }),
            })
            .collect()
    }

    pub(super) fn network_peers(&self, direct_only: bool) -> Vec<Peer> {
        let links = self.network.links();
        self.network
            .members()
            .into_iter()
            .filter_map(|member| {
                let link = links.by_peer(member.device)?;
                if link.state != LinkState::Ready || (direct_only && link.path != LinkPath::Direct) {
                    return None;
                }
                let advert = self.network.authenticated_advert(member.device);
                let mobile = advert.as_ref().map_or_else(
                    || member.os.as_deref().is_some_and(|os| is_mobile(card_os(os))),
                    |advert| advert.device_kind == oink_protocol::v2::dto::DeviceKind::Mobile,
                );
                // Content acceptance merges the negotiation with the peer's
                // facts: a PC whose sharing is paused refuses shares. Its
                // receive/send switches are not published; its refusal names
                // them and moves the share on (protocol-v2 5.2, review M17).
                let facts = self.network.peer_facts(member.device).filter(|facts| {
                    facts
                        .boot()
                        .ok()
                        .and_then(|boot| oink_network::wire_identity::read_boot(&boot).ok())
                        == Some(link.peer_boot)
                });
                let receiving = facts.as_ref().map(accepts_content);
                Some(Peer {
                    id: member.device.to_hex(),
                    kind: if mobile { PeerKind::Mobile } else { PeerKind::Pc },
                    state: PeerState::Ready,
                    accepts_clipboard: link.permits(oink_protocol::v2::registry::type_id::ClipboardShareBegin)
                        && receiving != Some(false),
                    connection_order: link.link.0,
                    relayed: link.path != LinkPath::Direct,
                    clipboard_pending: receiving.is_none(),
                })
            })
            .collect()
    }

    pub(super) fn refresh_presence(&self) {
        let snapshot = self.discovery.inner.snapshot();
        let generation = snapshot.generation;
        let advert = self.network.advertisement();
        let nearby = sightings(snapshot, advert.id);
        let members = self.network.members();
        let links = self.network.links();
        let pair_revision = self.network.pairing_snapshot().map_or(0, |value| value.revision);
        let mut state = lock(&self.presence);
        if state.stopped || state.suspended {
            return;
        }
        if state
            .generation
            .replace(generation)
            .is_some_and(|old| old != generation)
        {
            // A new discovery generation replaces the browse results and the
            // redial state only. Links stay up: a link whose path really went
            // away fails on its own heartbeat, a healthy one keeps its
            // transfers and audio.
            state.retry.clear();
        }
        let now = Instant::now();
        let wall = now_ms();
        let group = advert.group;
        let mut went_offline = false;
        let mut cards = Vec::new();
        for member in &members {
            let local = member.device == advert.id;
            let link = links.by_peer(member.device);
            let online = link.is_some_and(|entry| entry.state == LinkState::Ready);
            let sighting = nearby.get(&member.device);
            let incompatible = !online && sighting.is_some_and(|s| !s.advert.supports_current_protocol());
            let authenticated = self.network.authenticated_advert(member.device);
            let os = card_os(if local {
                &advert.os
            } else {
                authenticated
                    .as_ref()
                    .map(|value| value.os.as_str())
                    .or(member.os.as_deref())
                    .unwrap_or("")
            });
            let facts = self.network.peer_facts(member.device);
            let name = if local {
                advert.name.clone()
            } else {
                facts
                    .as_ref()
                    .and_then(|facts| facts.name().ok().map(|name| name.0))
                    .or_else(|| authenticated.as_ref().map(|value| value.name.clone()))
                    .or_else(|| member.name.clone())
                    .unwrap_or_else(|| member.device.short())
            };
            if !local && online {
                state.retry.remove(&member.device);
            }
            // Last seen is the end of the live link by this phone's clock:
            // the latest refresh that found it ready. While suspended no
            // refresh runs, so a link that went down meanwhile keeps the
            // time from before the suspension.
            if !local && online {
                state.last_seen.observe(group, member.device, wall);
            }
            went_offline |= !local && !online && state.cards.iter().any(|card| card.id == member.device && card.online);
            // An admitted direct link proves its endpoint: remember it for the
            // next start, which then dials before discovery has answered (the
            // PC's addresses.json). The peer's listener is its advertised
            // port; an inbound link's source port need not be it.
            if !local
                && let Some(entry) = link.filter(|entry| online && entry.path == LinkPath::Direct)
                && let (Some(remote), Some(advert)) = (entry.remote_address, authenticated.as_ref())
                && let Err(error) =
                    lock(&self.addresses).remember(member.device, SocketAddr::new(remote.ip(), advert.port))
            {
                tracing::warn!(?error, "could not remember a member's direct endpoint");
            }
            // A user-edited member address is tried first, then the endpoints
            // of earlier successful links, then discovery.
            let mut candidates: Vec<SocketAddr> = Vec::new();
            let known: Vec<SocketAddr> = {
                let book = lock(&self.addresses);
                book.get(member.device)
                    .into_iter()
                    .chain(book.remembered(member.device).iter().copied())
                    .collect()
            };
            for address in known.into_iter().chain(
                sighting
                    .filter(|_| !incompatible)
                    .map(dialing_addresses)
                    .unwrap_or_default(),
            ) {
                if !candidates.contains(&address) {
                    candidates.push(address);
                }
            }
            // New candidates, as on the PC (config_module runtime `sighting`):
            // the link layer adds them to a dial still in flight (the first
            // dial after the start often has none, discovery starts after
            // the session) or tries the direct path of a relayed link, and an
            // offline member is redialed now instead of after the retry pause.
            // The retry pause stays for attempts that found nothing new.
            if !local {
                let previous = state
                    .candidates
                    .insert(member.device, candidates.clone())
                    .unwrap_or_default();
                if previous != candidates {
                    self.network.update_addresses(member.device, candidates.clone());
                    if !online && candidates.iter().any(|address| !previous.contains(address)) {
                        state.retry.remove(&member.device);
                    }
                }
            }
            // Without a reachable address the member may still be reached
            // through a PC relay station (protocol-v2 §1.1); an incompatible
            // (v1) device cannot.
            if !local && link.is_none() && !incompatible && state.retry.get(&member.device).is_none_or(|at| *at <= now)
            {
                // A pending retry entry means the previous attempt did not
                // bring the member online: that is a background retry.
                let origin = if state.retry.contains_key(&member.device) {
                    oink_network::link::DialOrigin::BackgroundRetry
                } else {
                    oink_network::link::DialOrigin::Initial
                };
                let _ = self.network.dial_member(member.device, candidates, origin);
                state.retry.insert(member.device, now + RETRY);
            }
            // The crown means "can be driven right now": this phone's own
            // published fact, or the peer's fact on its live admitted link.
            let android_privileged = os == CardOs::Android
                && if local {
                    self.network.local_android_privileged()
                } else {
                    online
                        && facts
                            .as_ref()
                            .is_some_and(|facts| facts.android_privileged() == Ok(true))
                };
            let audio = self.media.with_audio(|audio| {
                [oink_audio::route::Role::Speaker, oink_audio::route::Role::Microphone]
                    .map(|role| (audio.mode(member.device, role), audio.switch(member.device, role)))
            });
            cards.push(CardInput {
                audio,
                android_privileged,
                id: member.device,
                name,
                os,
                pc: !local && !is_mobile(os),
                local,
                online,
                facts_version: facts
                    .as_ref()
                    .and_then(|facts| Some((facts.boot().ok()?.nonce().ok()?.0, facts.fact_sequence().ok()?))),
                incompatible,
                connecting: !online && (link.is_some() || state.retry.get(&member.device).is_some_and(|at| *at > now)),
                last_seen: if local || online {
                    None
                } else {
                    state.last_seen.get(group, member.device)
                },
                local_audio: local.then(|| {
                    let media = self.media.snapshot();
                    let failed = media
                        .failures
                        .iter()
                        .enumerate()
                        .filter(|(_, failure)| **failure != crate::MobileMediaFailure::None)
                        .fold(0u8, |mask, (index, _)| mask | (1 << index));
                    [media.enabled_mask, media.running_mask, failed]
                }),
            });
        }
        state
            .retry
            .retain(|peer, _| members.iter().any(|member| member.device == *peer));
        state
            .candidates
            .retain(|peer, _| members.iter().any(|member| member.device == *peer));
        state
            .last_seen
            .retain(group, |peer| members.iter().any(|member| member.device == peer));
        state.last_seen.save(went_offline);
        let mut rows = Vec::new();
        for sighting in nearby.values() {
            let advert = &sighting.advert;
            if members.iter().any(|member| member.device == advert.id) || self.network.is_blocked(advert.id) {
                continue;
            }
            let compatible = advert.supports_current_protocol();
            let authenticated = self.network.authenticated_advert(advert.id);
            let selected = sighting
                .addrs
                .iter()
                .find(|a| a.is_ipv4())
                .unwrap_or(&sighting.addrs[0]);
            rows.push(NearbyRow {
                device: advert.id.to_hex(),
                name: authenticated.as_ref().unwrap_or(advert).name.clone(),
                os: authenticated.as_ref().unwrap_or(advert).os.clone(),
                group: DiscoveredGroupRef {
                    group: advert.group.to_hex(),
                    root: None,
                },
                count: Some(advert.count),
                addresses: sighting.addrs.iter().copied().map(address_view).collect(),
                selected: address_view(*selected),
                source: 1,
                identity: if authenticated.is_some() {
                    IdentityEvidence::Authenticated
                } else {
                    IdentityEvidence::Unverified
                },
                reachable: selected.is_ipv4(),
                compatible,
                problem: (!compatible).then(incompatible_problem),
            });
        }
        let advert_changed = state.advert.as_ref() != Some(&advert);
        state.advert = Some(advert);
        let cards_changed = state.cards != cards;
        if cards_changed {
            state.cards = cards;
            state.card_revision += 1;
        }
        let mut targets = transfer_targets(&state.cards, &links, !self.network.connect_round_settled());
        let eligible = self.network_peers(true);
        targets.online.retain(|t| {
            eligible
                .iter()
                .any(|p| p.id == t.device && p.accepts_clipboard && !p.clipboard_pending)
        });
        if let Some(only) = &mut targets.only_device {
            only.online = targets.online.iter().any(|target| target.device == only.device);
        }
        let focus = self.input.focused_device().map(|d| d.to_hex());
        targets.active_device = targets
            .online
            .iter()
            .find(|t| t.kind == TransferTargetKind::Computer && Some(&t.device) == focus.as_ref())
            .or_else(|| targets.online.iter().min_by_key(|t| t.connection_order))
            .map(|t| t.device.clone());
        self.clipboard.set_proxy(targets.active_pc());
        let targets_changed = TransferTargetsView {
            revision: state.targets.revision,
            ..targets.clone()
        } != state.targets;
        if targets_changed {
            state.targets = TransferTargetsView {
                revision: state.targets.revision + 1,
                ..targets
            };
        }
        let nearby_changed = state.nearby != rows;
        if nearby_changed {
            state.nearby = rows;
            state.nearby_revision += 1;
        }
        let last_pair_revision = state.pair_revision;
        let pairing_changed = last_pair_revision != pair_revision;
        state.pair_revision = pair_revision;

        if let Some(transfer) = lock(&self.transfer).as_ref() {
            let direct = self.network_peers(true);
            let peers = self.network_peers(false);
            if !state.queried_startup_current
                && direct
                    .iter()
                    .any(|peer| peer.kind == PeerKind::Pc && peer.accepts_clipboard)
            {
                transfer.set_peers(direct.into_iter().filter(|peer| peer.accepts_clipboard).collect());
                state.queried_startup_current = transfer.request_current(rand::random(), 64).is_ok();
            }
            transfer.set_targets(peers, self.network.connect_round_settled());
        }
        let signals = self.signal_update(&mut state, &links);
        drop(state);
        if let Some(signals) = signals {
            self.emit_event(MobileEvent::DeviceCardSignals { signals });
        }
        if advert_changed {
            self.emit_event(MobileEvent::AdvertisementChanged);
        }
        if targets_changed {
            let targets = self.targets_view();
            self.persist_targets(&targets.online);
            self.emit_event(MobileEvent::TransferTargets { targets });
        }
        if cards_changed {
            self.emit_event(MobileEvent::DeviceCards {
                snapshot: self.cards_view(),
            });
            // Focus lock state rides on the same owner facts as the cards.
            self.emit_event(MobileEvent::FocusChanged);
        }
        if pairing_changed && let Ok(events) = self.pairing_errors(last_pair_revision) {
            for event in events {
                self.emit_event(MobileEvent::Pairing { event });
            }
        }
        if nearby_changed || pairing_changed {
            self.emit_event(MobileEvent::Pairing {
                event: PairingEvent::NearbyRefreshFinished {
                    result: TypedResult::Ok,
                },
            });
        }
    }

    /// Members online at the last refresh were seen now (unless the links
    /// are already suspended); writes the record before the phone suspends
    /// or stops.
    pub(super) fn save_last_seen(&self) {
        let mut state = lock(&self.presence);
        let state = &mut *state;
        if !state.suspended
            && let Some(group) = state.advert.as_ref().map(|advert| advert.group)
        {
            let wall = now_ms();
            for card in state.cards.iter().filter(|card| card.online && !card.local) {
                state.last_seen.observe(group, card.id, wall);
            }
        }
        state.last_seen.save(true);
    }

    /// A group member's display name as the device cards show it.
    pub(super) fn member_name(&self, device: &str) -> String {
        lock(&self.presence)
            .cards
            .iter()
            .find(|card| card.id.to_hex() == device)
            .map_or_else(|| device.chars().take(8).collect(), |card| card.name.clone())
    }

    /// `device` is another phone or tablet of the group.
    pub(super) fn is_mobile_member(&self, device: &str) -> bool {
        lock(&self.presence)
            .cards
            .iter()
            .any(|card| card.id.to_hex() == device && !card.local && !card.pc)
    }

    /// Where a dial of member `device` goes: a user-edited address first,
    /// then the endpoints of earlier successful links, then what discovery
    /// sees (nothing from an incompatible advert).
    pub(super) fn member_candidates(&self, device: DeviceId) -> Vec<SocketAddr> {
        let advert = self.network.advertisement();
        let nearby = sightings(self.discovery.inner.snapshot(), advert.id);
        let mut candidates: Vec<SocketAddr> = Vec::new();
        {
            let book = lock(&self.addresses);
            for address in book
                .get(device)
                .into_iter()
                .chain(book.remembered(device).iter().copied())
            {
                if !candidates.contains(&address) {
                    candidates.push(address);
                }
            }
        }
        if let Some(sighting) = nearby.get(&device).filter(|s| s.advert.supports_current_protocol()) {
            for address in dialing_addresses(sighting) {
                if !candidates.contains(&address) {
                    candidates.push(address);
                }
            }
        }
        candidates
    }

    /// Latency changes do not bump the card revision; they are pushed as a
    /// separate lightweight signal snapshot when any online RTT moves >= 1 ms.
    fn signal_update(&self, state: &mut Presence, links: &oink_network::link::LinkTable) -> Option<DeviceCardSignals> {
        let signals: Vec<_> = state
            .cards
            .iter()
            .filter(|card| card.online && !card.local)
            .map(|card| DeviceCardSignal {
                device: card.id.to_hex(),
                rtt_ms: links
                    .by_peer(card.id)
                    .and_then(|link| link.rtt)
                    .map(|rtt| rtt.as_secs_f64() * 1000.0),
            })
            .collect();
        let moved = signals.len() != state.signals.len()
            || signals.iter().zip(&state.signals).any(|(now, before)| {
                now.device != before.device
                    || match (now.rtt_ms, before.rtt_ms) {
                        (Some(now), Some(before)) => (now - before).abs() >= 1.0,
                        (None, None) => false,
                        _ => true,
                    }
            });
        if !moved {
            return None;
        }
        state.signals = signals.clone();
        state.signals_revision += 1;
        Some(DeviceCardSignals {
            stream: self.stream.clone(),
            revision: state.signals_revision,
            sampled_at_ms: now_ms(),
            signals,
        })
    }

    /// Discovery sees an Oink advert that this version cannot connect to at
    /// this endpoint's IP.
    pub(super) fn incompatible_endpoint(&self, address: SocketAddr) -> bool {
        self.discovery.inner.snapshot().services.into_values().any(|sighting| {
            !sighting.advert.supports_current_protocol() && sighting.addrs.iter().any(|seen| seen.ip() == address.ip())
        })
    }

    pub(super) fn nearby_incompatible(&self, peer: DeviceId) -> bool {
        let local = self.network.advertisement().id;
        sightings(self.discovery.inner.snapshot(), local)
            .get(&peer)
            .is_some_and(|sighting| !sighting.advert.supports_current_protocol())
    }

    pub(super) fn nearby_addresses(&self, peer: DeviceId) -> Option<Vec<SocketAddr>> {
        let snapshot = self.discovery.inner.snapshot();
        let local = self.network.advertisement().id;
        let found = sightings(snapshot, local);
        let sighting = found.get(&peer)?.clone();
        sighting
            .advert
            .supports_current_protocol()
            .then(|| dialing_addresses(&sighting))
    }
}

/// Whether a PC's facts say it takes mobile content now: its sharing is not
/// paused. (A paused PC also closes its clipboard channel.)
pub(super) fn accepts_content(facts: &oink_protocol::v2::dto::DeviceFacts) -> bool {
    !facts
        .pause()
        .ok()
        .flatten()
        .is_some_and(|pause| pause.paused().unwrap_or(false))
}

/// The transfer targets of these cards (revision left at 0).
fn transfer_targets(
    cards: &[CardInput],
    links: &oink_network::link::LinkTable,
    searching: bool,
) -> TransferTargetsView {
    let kind = |card: &CardInput| {
        if card.pc {
            TransferTargetKind::Computer
        } else {
            TransferTargetKind::Mobile
        }
    };
    let others: Vec<&CardInput> = cards.iter().filter(|card| !card.local).collect();
    let mut online: Vec<TransferTarget> = others
        .iter()
        .filter(|card| card.online)
        .map(|card| TransferTarget {
            device: card.id.to_hex(),
            name: card.name.clone(),
            kind: kind(card),
            os: card.os,
            android_privileged: card.android_privileged,
            connection_order: links.by_peer(card.id).map_or(u64::MAX, |link| link.link.0),
        })
        .collect();
    online.sort_by_key(|target| (target.kind != TransferTargetKind::Computer, target.connection_order));
    let only_device = match others.as_slice() {
        [card] => Some(TransferOnlyDevice {
            device: card.id.to_hex(),
            name: card.name.clone(),
            kind: kind(card),
            os: card.os,
            online: card.online,
        }),
        _ => None,
    };
    TransferTargetsView {
        revision: 0,
        active_device: None,
        online,
        other_devices: u32::try_from(others.len()).unwrap_or(u32::MAX),
        only_device,
        searching,
    }
}

fn incompatible_problem() -> PairProblem {
    use oink_protocol::v2::dto::{PairCode, PairStage};
    PairProblem {
        code: PairCode::UnsupportedProtocol.number(),
        stage: PairStage::Negotiate.number(),
        detail: "peer requires a compatible Oink version".into(),
        network: Some(AddressNetworkProblem::Incompatible),
    }
}

/// Refreshes cards, dials and transfer targets on every discovery or link
/// change, so a target that became ready is used at once. The tick only
/// samples latency and paces redials.
async fn pump(
    session: Weak<MobileSession>,
    mut discovery: tokio::sync::watch::Receiver<NativeDiscoverySnapshot>,
    mut links: tokio::sync::watch::Receiver<u64>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            changed = discovery.changed() => if changed.is_err() { break },
            changed = links.changed() => if changed.is_err() { break },
        }
        let Some(session) = session.upgrade() else { break };
        if lock(&session.presence).stopped {
            break;
        }
        session.refresh_presence();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn session(dir: &std::path::Path, platform: MobilePlatform) -> Arc<MobileSession> {
        let config = MobileConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            cache_dir: None,
            log_dir: None,
            device_name: "Synthetic".into(),
            platform,
            profile: SessionProfile::Mobile,
            form_factor: None,
            app_version: None,
        };
        // iOS keeps its seed in the Keychain and passes it in; Android owns a file.
        let seed = matches!(platform, MobilePlatform::Ios).then(|| vec![7; 32]);
        initialize_mobile_group(config.clone(), seed.clone()).unwrap();
        MobileSession::start(config, MobileDiscovery::new(), seed)
            .await
            .unwrap()
    }

    fn own_card(session: &MobileSession) -> DeviceCard {
        session.refresh_presence();
        session
            .cards_view()
            .cards
            .into_iter()
            .find(|card| card.is_self)
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn own_card_wears_the_crown_only_while_privileged_mode_runs() {
        let dir = tempfile::tempdir().unwrap();
        let android = session(dir.path(), MobilePlatform::Android).await;
        assert!(!own_card(&android).android_privileged);
        android.network.set_android_privileged(true).unwrap();
        assert!(own_card(&android).android_privileged);
        android.network.set_android_privileged(false).unwrap();
        assert!(!own_card(&android).android_privileged);

        let dir = tempfile::tempdir().unwrap();
        let ios = session(dir.path(), MobilePlatform::Ios).await;
        ios.network.set_android_privileged(true).unwrap();
        assert!(
            !own_card(&ios).android_privileged,
            "only an Android card wears the crown"
        );
    }

    fn card(byte: u8, os: CardOs, online: bool, local: bool) -> CardInput {
        CardInput {
            id: DeviceId([byte; 32]),
            name: format!("device {byte}"),
            os,
            pc: !local && !is_mobile(os),
            local,
            online,
            facts_version: None,
            incompatible: false,
            connecting: false,
            last_seen: None,
            local_audio: None,
            android_privileged: false,
            audio: [(None, None); 2],
        }
    }

    /// Online computers come before online phones; offline devices and this
    /// phone are not targets but count toward the group size.
    #[test]
    fn targets_list_online_computers_then_mobile_devices() {
        let links = oink_network::link::LinkTable::default();
        let cards = [
            card(1, CardOs::Android, true, true),
            card(2, CardOs::Ios, true, false),
            card(3, CardOs::Windows, true, false),
            card(4, CardOs::Macos, false, false),
        ];
        let view = transfer_targets(&cards, &links, false);
        let online: Vec<_> = view
            .online
            .iter()
            .map(|target| (target.name.as_str(), target.kind))
            .collect();
        assert_eq!(
            online,
            vec![
                ("device 3", TransferTargetKind::Computer),
                ("device 2", TransferTargetKind::Mobile)
            ]
        );
        assert_eq!((view.other_devices, view.only_device), (3, None));

        let pair = [
            card(1, CardOs::Android, true, true),
            card(4, CardOs::Macos, false, false),
        ];
        let view = transfer_targets(&pair, &links, true);
        assert!(view.online.is_empty() && view.searching);
        let only = view.only_device.unwrap();
        assert_eq!(
            (only.name.as_str(), only.kind, only.online),
            ("device 4", TransferTargetKind::Computer, false)
        );
    }

    #[test]
    fn the_target_cache_round_trips_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_string_lossy().into_owned();
        assert!(cached_transfer_targets(data_dir.clone()).is_empty());
        let targets = vec![TransferTarget {
            device: "0a".repeat(32),
            name: "Studio Mac".into(),
            kind: TransferTargetKind::Computer,
            os: CardOs::Macos,
            android_privileged: false,
            connection_order: 4,
        }];
        targets::write_cache(&crate::device_only::transfer_targets(dir.path()), &targets).unwrap();
        assert_eq!(cached_transfer_targets(data_dir), targets);
    }

    /// This phone (a session with presence, so it dials by itself) and the
    /// other phone of its group (a bare network that never dials).
    struct TwoPhones {
        phone_seed: [u8; 32],
        phone_dir: tempfile::TempDir,
        other: oink_mobile_core::MobileNetwork,
        other_id: DeviceId,
        _other_dir: tempfile::TempDir,
    }

    impl TwoPhones {
        async fn new(phone_seed: [u8; 32], other_seed: [u8; 32]) -> Self {
            use oink_network::{group_state::State, identity::Identity};
            use oink_protocol::v2::schema::WireDto;
            let phone = Identity::from_seed(phone_seed);
            let other = Identity::from_seed(other_seed);
            let base = State::create(&phone, "phone", "android", 1).unwrap();
            let roster = base.roster().add(&phone, other.id(), "other phone", "ios", 2).unwrap();
            let state = base
                .with_roster(&roster)
                .unwrap()
                .wire()
                .encode(oink_protocol::v2::limits::CONFIG_STATE)
                .unwrap();
            let phone_dir = tempfile::tempdir().unwrap();
            let group_state = crate::device_only::group_state(phone_dir.path());
            std::fs::create_dir_all(group_state.parent().unwrap()).unwrap();
            std::fs::write(group_state, &state).unwrap();
            let other_dir = tempfile::tempdir().unwrap();
            let network = oink_mobile_core::MobileNetwork::start(oink_mobile_core::MobileNetworkConfig {
                identity_seed: other_seed,
                boot: oink_foundation::session::Boot {
                    device_id: other.id(),
                    nonce: [other_seed[0]; 16],
                },
                bind: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                group_state: Some(state),
                group_state_path: other_dir.path().join("group-state.v2"),
                pairing_journal_path: other_dir.path().join("pairing-recovery.cbor"),
                device_name: "other phone".into(),
                os: "ios".into(),
                form_factor: None,
                trusted_peers: Vec::new(),
                trust_path: None,
                process: Default::default(),
            })
            .await
            .unwrap();
            Self {
                phone_seed,
                phone_dir,
                other: network,
                other_id: other.id(),
                _other_dir: other_dir,
            }
        }

        async fn start_phone(&self, discovery: Arc<MobileDiscovery>) -> Arc<MobileSession> {
            let config = MobileConfig {
                data_dir: self.phone_dir.path().to_string_lossy().into_owned(),
                cache_dir: None,
                log_dir: None,
                device_name: "phone".into(),
                platform: MobilePlatform::Android,
                profile: SessionProfile::Mobile,
                form_factor: None,
                app_version: None,
            };
            MobileSession::start(config, discovery, Some(self.phone_seed.to_vec()))
                .await
                .unwrap()
        }

        /// How long until the phone has a ready link to the other phone.
        async fn ready_after(&self, phone: &MobileSession, since: Instant) -> Duration {
            while !phone.network.links().ready(self.other_id) {
                assert!(since.elapsed() < Duration::from_secs(10), "the phones never connected");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            since.elapsed()
        }
    }

    /// The session's first pass dials the other phone before the host's
    /// browser has found it (no address: a relay search that cannot succeed).
    /// The address discovery reports a moment later is dialed at once; it
    /// used to wait for that search to end (3 s) and the 4 s retry pause.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_member_discovered_after_the_first_dial_is_dialed_at_once() {
        let phones = TwoPhones::new([41; 32], [42; 32]).await;
        let browser = MobileDiscovery::new();
        let phone = phones.start_phone(browser.clone()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!phone.network.links().ready(phones.other_id));

        let found = Instant::now();
        let txt = oink_network::discovery::txt_pairs(&phones.other.advertisement())
            .into_iter()
            .collect();
        browser.upsert(
            0,
            "other phone".into(),
            txt,
            vec![ResolvedEndpoint {
                address: "127.0.0.1".into(),
                port: phones.other.local_addr().port(),
                scope_id: 0,
            }],
        );
        let elapsed = phones.ready_after(&phone, found).await;
        assert!(
            elapsed < Duration::from_millis(2500),
            "connected {elapsed:?} after discovery"
        );
        phone.clone().shutdown().await.unwrap();
        phones.other.stop().await;
    }

    /// An admitted direct link's endpoint is remembered in the device-only
    /// directory; the next start dials it before (here: without) discovery.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restarted_phone_reconnects_through_the_remembered_address_without_discovery() {
        let phones = TwoPhones::new([43; 32], [44; 32]).await;
        let phone = phones.start_phone(MobileDiscovery::new()).await;
        // The first link comes in from the other phone; nothing is discovered.
        let port = phone.advertisement().await.unwrap().port;
        let phone_id = phone.network.advertisement().id;
        phones
            .other
            .dial(phone_id, vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
            .unwrap();
        phones.ready_after(&phone, Instant::now()).await;
        let listener: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, phones.other.local_addr().port()).into();
        let since = Instant::now();
        while lock(&phone.addresses).remembered(phones.other_id).first() != Some(&listener) {
            assert!(
                since.elapsed() < Duration::from_secs(5),
                "the endpoint was not remembered"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        phone.clone().shutdown().await.unwrap();

        // Restart with a browser that never reports anything: only the
        // remembered address can reach the other phone (which never dials).
        let started = Instant::now();
        let phone = phones.start_phone(MobileDiscovery::new()).await;
        let elapsed = phones.ready_after(&phone, started).await;
        assert!(elapsed < Duration::from_millis(2500), "reconnected after {elapsed:?}");
        phone.clone().shutdown().await.unwrap();
        phones.other.stop().await;
    }
}
