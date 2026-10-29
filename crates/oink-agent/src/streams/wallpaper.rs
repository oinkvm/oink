//! Wallpaper delivery retries per peer until the receiver stores the current set
//! and acknowledges it with an orderly Low end. Pause and link retirement cancel it.
use super::StreamModule;
use crate::config_module::ConfigModule;
use crate::wallpaper::model::{self, ScreenWallpaper};
use oink_foundation::{DeviceId, snapshot::Snapshot};
use oink_network::link::{InboundLow, InboundStream, LinkId, LinkLayer, LinkLost, StreamClosed, StreamId};
use oink_protocol::{
    frame::{StreamCode, StreamKind},
    v1::{self, Message, dto::WallpaperSet},
};
use std::{collections::HashMap, sync::Arc, time::Duration};

const RETRY_INITIAL: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

pub const KINDS: &[StreamKind] = &[StreamKind::Wallpaper];

#[derive(Debug)]
struct Peer {
    link: LinkId,
    revision: Option<u64>,
    sending: Option<tokio::task::JoinHandle<()>>,
    // Receiving remains enabled during pause, including normal Low completion.
    finishing: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(task) = self.sending.take() {
            task.abort();
        }
        for task in self.finishing.drain(..) {
            task.abort();
        }
    }
}

// Jitter in [80%, 100%] keeps the exponential ceiling at 60 seconds.
fn retry_delay(backoff: Duration) -> Duration {
    let mut random = [0; 2];
    let _ = getrandom::fill(&mut random);
    backoff.mul_f64(0.8 + 0.2 * f64::from(u16::from_ne_bytes(random)) / f64::from(u16::MAX))
}

async fn deliver(
    links: &LinkLayer,
    link: LinkId,
    config: &ConfigModule,
    local: &Snapshot<Option<WallpaperSet>>,
) -> bool {
    if config.applied().paused {
        return false;
    }
    let max = v1::limits::LOW_FRAME as u64;
    let options = oink_network::LowOptions {
        send_max: max,
        receive_max: 0,
        operation_max: max,
    };
    let Ok(mut stream) = links.open_low(link, StreamKind::Wallpaper, options).await else {
        return false;
    };
    // Admission may finish after pause is applied but before this owner's
    // ConfigChange branch runs. Never read/send a new image through that gap.
    if config.applied().paused {
        return false;
    }
    // Read after opening: a change during pool admission must send the latest set.
    let current = local.load();
    let Some(message) = current.as_ref() else {
        return false;
    };
    if stream.send(&model::message(message.clone())).await.is_err() {
        return false;
    }
    stream.finish();
    // The receiver ends its side only after validating and storing the complete set.
    // Sending bytes, a reset, or a timeout alone must never count as delivery.
    matches!(stream.recv().await, Ok(None))
}

pub struct WallpaperStreams {
    links: LinkLayer,
    config: ConfigModule,
    local: Arc<Snapshot<Option<WallpaperSet>>>,
    peers: HashMap<DeviceId, Peer>,
    received: HashMap<DeviceId, Vec<ScreenWallpaper>>,
    paused: bool,
    revision: u64,
}

impl std::fmt::Debug for WallpaperStreams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallpaperStreams")
            .field("peers", &self.peers.len())
            .field("paused", &self.paused)
            .field("revision", &self.revision)
            .finish()
    }
}

impl WallpaperStreams {
    pub fn new(links: LinkLayer, config: ConfigModule) -> Self {
        Self {
            links,
            local: Arc::new(Snapshot::default()),
            peers: HashMap::new(),
            received: HashMap::new(),
            paused: config.applied().paused,
            config,
            revision: 0,
        }
    }

    /// Called after the existing wallpaper/display watcher has re-read the thumbnails.
    pub(crate) fn set_wallpapers(&mut self, screens: Vec<ScreenWallpaper>) {
        if self
            .local
            .load()
            .as_ref()
            .as_ref()
            .is_some_and(|message| model::decode(message).is_ok_and(|old| old == screens))
        {
            return;
        }
        let next = self.revision + 1;
        let Ok(message) = model::encode(next, &screens) else {
            tracing::warn!("invalid local wallpaper thumbnails");
            return;
        };
        self.revision = next;
        self.local.publish(Some(message));
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.send_to(peer);
        }
    }

    /// Display changes are an independent send occasion even if the cached image
    /// bytes did not change. The watcher may subsequently publish a newer set.
    pub fn displays_changed(&mut self) {
        for peer in self.peers.keys().copied().collect::<Vec<_>>() {
            self.send_to(peer);
        }
    }

    fn send_to(&mut self, peer: DeviceId) {
        // Image and config notifications arrive on independent select! branches.
        // The applied snapshot fences sends even while our pause event is queued.
        if self.paused
            || self.config.applied().paused
            || self.local.load().is_none()
            || !self
                .config
                .peer_agreement(peer)
                .is_some_and(|agreement| agreement.contains("oink.wallpaper/1"))
        {
            return;
        }
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        if let Some(task) = state.sending.take() {
            task.abort();
        }
        let links = self.links.clone();
        let config = self.config.clone();
        let link = state.link;
        let local = self.local.clone();
        state.sending = Some(tokio::spawn(async move {
            let mut backoff = RETRY_INITIAL;
            loop {
                // Bound a silent peer or stuck admission as well as explicit errors.
                if tokio::time::timeout(DELIVERY_TIMEOUT, deliver(&links, link, &config, &local))
                    .await
                    .unwrap_or(false)
                {
                    return;
                }
                tokio::time::sleep(retry_delay(backoff)).await;
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }));
    }

    pub(crate) fn received(&self, peer: DeviceId) -> Option<&[ScreenWallpaper]> {
        self.received.get(&peer).map(Vec::as_slice)
    }

    fn store(&mut self, peer: DeviceId, link: LinkId, message: WallpaperSet) -> v1::Result<()> {
        let screens = model::decode(&message)?;
        let revision = message.revision()?;
        let state = self
            .peers
            .get_mut(&peer)
            .filter(|p| p.link == link)
            .ok_or(v1::ErrorCode::NotReady)?;
        if state.revision.is_none_or(|old| revision > old) {
            state.revision = Some(revision);
            self.received.insert(peer, screens);
        }
        Ok(())
    }
}
impl StreamModule for WallpaperStreams {
    const KINDS: &'static [StreamKind] = KINDS;
    fn link_ready(&mut self, link: LinkId, peer: DeviceId) {
        if self.peers.get(&peer).is_none_or(|state| state.link != link) {
            self.peers.insert(
                peer,
                Peer {
                    link,
                    revision: None,
                    sending: None,
                    finishing: Vec::new(),
                },
            );
        }
        self.send_to(peer);
    }
    fn link_lost(&mut self, link: LinkId, peer: DeviceId, _: LinkLost) {
        if self.peers.get(&peer).is_some_and(|p| p.link == link) {
            self.peers.remove(&peer);
        }
    }
    fn inbound(&mut self, stream: InboundStream) {
        stream.reject(StreamCode::Unsupported);
    }
    fn inbound_low(&mut self, mut operation: InboundLow) {
        if operation.stream.kind() == StreamKind::Wallpaper
            && let Message::WallpaperSet(message) = operation.first
        {
            if let Err(error) = self.store(operation.stream.peer(), operation.stream.link(), message) {
                super::wire::report_low_payload(
                    &self.links,
                    operation.stream.link(),
                    StreamKind::Wallpaper,
                    0x0800,
                    error.code,
                );
                operation.stream.cancel();
                return;
            }
            operation.stream.finish();
            if let Some(state) = self.peers.get_mut(&operation.stream.peer()) {
                state.finishing.retain(|task| !task.is_finished());
                state.finishing.push(tokio::spawn(async move {
                    let _ = operation.stream.recv().await;
                }));
            }
            return;
        }
        operation.stream.cancel();
    }
    fn stream_closed(&mut self, _: StreamId, _: StreamClosed) {}
    fn pause_changed(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if paused {
            for peer in self.peers.values_mut() {
                if let Some(task) = peer.sending.take() {
                    task.abort();
                }
            }
        } else {
            for peer in self.peers.keys().copied().collect::<Vec<_>>() {
                self.send_to(peer);
            }
        }
    }
    fn facts_changed(&mut self, _: DeviceId, _: bool) {}
}

#[cfg(all(test, debug_assertions))]
mod lifecycle {
    use super::*;
    use crate::streams::{diagnostics::DiagnosticsStreams, lifecycle_tests::Fixture};
    use oink_network::link::{ModuleEvent, ModulePort};
    use std::time::Duration;

    async fn receive(port: &mut ModulePort) -> InboundLow {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if let Some(ModuleEvent::InboundLow(low)) = port.next().await {
                    return *low;
                }
            }
        })
        .await
        .expect("wallpaper should arrive, including a timer retry after failure")
    }

    async fn no_wallpaper(port: &mut ModulePort) {
        assert!(
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(ModuleEvent::InboundLow(_)) = port.next().await {
                        break;
                    }
                }
            })
            .await
            .is_err(),
            "no wallpaper operation should be opened"
        );
    }

    async fn finished(owner: &WallpaperStreams, peer: DeviceId) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while owner.peers[&peer].sending.iter().any(|task| !task.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("receiver's normal Low end acknowledges the stored set");
    }

    #[test]
    fn applied_pause_blocks_wallpaper_before_owner_notification() {
        crate::test_watchdog::run(
            module_path!(),
            "applied_pause_before_wallpaper_notification",
            Some(4),
            async {
                use oink_protocol::v1::{
                    RawValue, dto,
                    schema::{WireDto, WireValue},
                };
                let fixture = Fixture::<DiagnosticsStreams>::start().await;
                let (links, group) = fixture.context(0);
                let peer = fixture.context(1).0.id();
                let mut owner = WallpaperStreams::new(links.clone(), group.clone());
                owner.link_ready(links.links().by_peer(peer).unwrap().link, peer);
                let pause = dto::LocalPauseState::from_raw(
                    [
                        (1, RawValue::Bool(true)),
                        (2, dto::PauseReason::LocalUser.write()),
                        (3, 2u64.write()),
                    ]
                    .into(),
                )
                .unwrap();
                group
                    .publish_local_owner(group.local_facts().preferences().unwrap().unwrap().0, pause)
                    .unwrap();
                crate::test_watchdog::until("pause is durably applied", || group.applied().paused).await;
                // Deliberately do not deliver pause_changed: the image callback and
                // config notification are separate select! branches in the runtime.
                assert!(!owner.paused);
                owner.set_wallpapers(vec![]);
                assert!(
                    owner.peers[&peer].sending.is_none(),
                    "an applied pause must prevent a new wallpaper send even before its notification"
                );
                let attempts = oink_network::link::testing::low_open_attempts(&links, StreamKind::Wallpaper);
                assert!(!deliver(&links, owner.peers[&peer].link, &group, &owner.local).await);
                assert_eq!(
                    oink_network::link::testing::low_open_attempts(&links, StreamKind::Wallpaper),
                    attempts,
                    "a pending attempt must also observe the applied pause before opening Low"
                );
                drop(owner);
                fixture.stop().await;
            },
        );
    }

    #[test]
    fn failed_delivery_retries_until_empty_set_is_stored_then_stops() {
        crate::test_watchdog::run(
            module_path!(),
            "failed_delivery_retries_until_empty_set_is_stored_then_stops",
            Some(4),
            async {
                let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
                let (links, group) = fixture.context(0);
                let (remote_links, remote_group) = fixture.context(1);
                let peer = remote_links.id();
                let me = links.id();
                let mut receiver = WallpaperStreams::new(remote_links.clone(), remote_group);
                receiver.link_ready(remote_links.links().by_peer(me).unwrap().link, me);
                let mut owner = WallpaperStreams::new(links.clone(), group);
                owner.link_ready(links.links().by_peer(peer).unwrap().link, peer);
                let screens = vec![ScreenWallpaper {
                    display: "screen".into(),
                    width: 2,
                    height: 3,
                    jpeg: vec![0xff, 0xd8, 1],
                }];
                owner.set_wallpapers(screens.clone());
                let mut failed = receive(fixture.port(1)).await;
                failed.stream.cancel();
                drop(failed);
                let retried = receive(fixture.port(1)).await;
                let first = retried.first.clone();
                receiver.inbound_low(retried);
                assert_eq!(receiver.received(me), Some(screens.as_slice()));
                finished(&owner, peer).await;
                no_wallpaper(fixture.port(1)).await;
                owner.displays_changed();
                let changed = receive(fixture.port(1)).await;
                assert_eq!(changed.first, first, "display changes still send unchanged snapshots");
                receiver.inbound_low(changed);
                finished(&owner, peer).await;
                owner.set_wallpapers(vec![]);
                receiver.inbound_low(receive(fixture.port(1)).await);
                assert_eq!(
                    receiver.received(me),
                    Some([].as_slice()),
                    "empty static wallpaper set counts as obtained"
                );
                finished(&owner, peer).await;
                no_wallpaper(fixture.port(1)).await;
                drop(owner);
                drop(receiver);
                fixture.stop().await;
            },
        );
    }

    #[test]
    fn pending_retry_obeys_pause_and_link_loss() {
        crate::test_watchdog::run(
            module_path!(),
            "pending_retry_obeys_pause_and_link_loss",
            Some(4),
            async {
                let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
                let (links, group) = fixture.context(0);
                let peer = fixture.context(1).0.id();
                let link = links.links().by_peer(peer).unwrap().link;
                let mut owner = WallpaperStreams::new(links, group);
                owner.link_ready(link, peer);
                owner.set_wallpapers(vec![]);
                let mut failed = receive(fixture.port(1)).await;
                failed.stream.cancel();
                drop(failed);
                owner.pause_changed(true);
                owner.displays_changed();
                no_wallpaper(fixture.port(1)).await;
                owner.pause_changed(false);
                let mut failed = receive(fixture.port(1)).await;
                failed.stream.cancel();
                drop(failed);
                // A resumed send must also retry without another external occasion.
                let mut retry = receive(fixture.port(1)).await;
                retry.stream.cancel();
                drop(retry);
                owner.link_lost(link, peer, LinkLost::ConnectionLost);
                no_wallpaper(fixture.port(1)).await;
                drop(owner);
                fixture.stop().await;
            },
        );
    }

    #[test]
    fn replacement_link_cancels_old_retry_and_resets_received_revision() {
        crate::test_watchdog::run(
            module_path!(),
            "replacement_link_cancels_old_retry_and_resets_received_revision",
            Some(4),
            async {
                let mut fixture = Fixture::<DiagnosticsStreams>::start().await;
                let (links, group) = fixture.context(0);
                let peer = fixture.context(1).0.id();
                let live = links.links().by_peer(peer).unwrap().link;
                let gone = LinkId(u64::MAX);
                let mut owner = WallpaperStreams::new(links, group);
                owner.link_ready(gone, peer);
                owner.set_wallpapers(vec![]);
                owner.store(peer, gone, model::encode(99, &[]).unwrap()).unwrap();
                owner.link_ready(live, peer);
                owner.link_lost(gone, peer, LinkLost::ConnectionLost);
                let mut ready = receive(fixture.port(1)).await;
                ready.stream.finish();
                let _ = ready.stream.recv().await;
                finished(&owner, peer).await;
                owner.store(peer, live, model::encode(1, &[]).unwrap()).unwrap();
                assert_eq!(owner.peers[&peer].revision, Some(1));
                drop(owner);
                fixture.stop().await;
            },
        );
    }
}

#[cfg(all(test, debug_assertions))]
mod cutover_regression {
    use super::*;
    use crate::streams::{diagnostics::DiagnosticsStreams, lifecycle_tests::Fixture};
    use oink_network::link::{LinkPath, ModuleEvent, ModulePort};
    use std::time::Duration;

    async fn wallpaper(port: &mut ModulePort) -> InboundLow {
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(ModuleEvent::InboundLow(operation)) = port.next().await {
                    return *operation;
                }
            }
        })
        .await
        .expect("wallpaper must recover after path cutover without another link-ready event")
    }

    #[test]
    fn relay_to_direct_cutover_retries_unstored_wallpaper_in_both_directions() {
        crate::test_watchdog::run(
            module_path!(),
            "relay_to_direct_cutover_retries_unstored_wallpaper_in_both_directions",
            Some(4),
            async {
                let mut fixture = Fixture::<DiagnosticsStreams>::start_relayed().await;
                let contexts = [fixture.context(0), fixture.context(1)];
                let peers = [contexts[1].0.id(), contexts[0].0.id()];
                let links = [
                    contexts[0].0.links().by_peer(peers[0]).unwrap().link,
                    contexts[1].0.links().by_peer(peers[1]).unwrap().link,
                ];
                for (index, (network, _)) in contexts.iter().enumerate() {
                    assert!(matches!(
                        network.links().by_peer(peers[index]).unwrap().path,
                        LinkPath::Relayed { .. }
                    ));
                }
                let mut owners = contexts
                    .iter()
                    .map(|(network, config)| WallpaperStreams::new(network.clone(), config.clone()))
                    .collect::<Vec<_>>();
                let screens = (0..2)
                    .map(|index| {
                        vec![ScreenWallpaper {
                            display: "screen".into(),
                            width: 48,
                            height: 30,
                            jpeg: vec![0xff, 0xd8, index],
                        }]
                    })
                    .collect::<Vec<_>>();
                for index in 0..2 {
                    owners[index].link_ready(links[index], peers[index]);
                    owners[index].set_wallpapers(screens[index].clone());
                }
                // Hold each complete first set before storage/acknowledgement, so
                // the actual migration deterministically interrupts both Low operations.
                let pending = [wallpaper(fixture.port(0)).await, wallpaper(fixture.port(1)).await];
                for index in 0..2 {
                    oink_network::link::testing::set_direct_blocked(&contexts[index].0, peers[index], false);
                }
                for index in 0..2 {
                    // Renew candidates after restoring reachability; an identical
                    // address snapshot intentionally does not reset network backoff.
                    contexts[index].0.update_addresses(peers[index], Vec::new());
                    contexts[index]
                        .1
                        .set_member_address(peers[index], contexts[1 - index].0.local_addr().to_string())
                        .unwrap();
                }
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if contexts.iter().enumerate().all(|(index, (network, _))| {
                            network.links().by_peer(peers[index]).is_some_and(|entry| {
                                assert_eq!(entry.link, links[index], "migration preserves the logical link");
                                entry.path == LinkPath::Direct
                            })
                        }) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("real relay links cut over to direct");
                for operation in pending {
                    assert!(
                        !(operation.stream.effect_validity())(),
                        "cutover retires the actual old Low operation"
                    );
                }
                for index in 0..2 {
                    let retried = wallpaper(fixture.port(index)).await;
                    assert_eq!(retried.stream.path(), LinkPath::Direct);
                    owners[index].inbound_low(retried);
                    assert_eq!(
                        owners[index].received(peers[index]),
                        Some(screens[1 - index].as_slice())
                    );
                }
                drop(owners);
                fixture.stop().await;
            },
        );
    }
}
