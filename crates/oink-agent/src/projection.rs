//! Ephemeral projection preview owner. P0 never writes config; only a completed native
//! gesture submits a normal ConfigModule::edit_layout (and hence normal C0).
use crate::{
    api,
    node::{Node, lock},
    streams::{StreamModule, projection::ProjectionStreams},
};
use oink_display::{
    layout::{Layout, ScreenId},
    projection::Projection,
};
use oink_foundation::DeviceId;
use oink_locator::projection::{DragPhase, Event, NamedProjection, ProjectionOverlay, View};
use oink_network::link::{ModuleEvent, ModulePort};
use oink_protocol::v1;
pub(crate) mod model;
pub(crate) mod wire;
use model::{Action, ModeId, ScreenRect};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(34); // <30 previews/sec, latest wins.
const LEASE: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct State {
    surface: OnceLock<ProjectionOverlay>,
    control: Mutex<Control>,
    wake: tokio::sync::Notify,
}
#[derive(Default)]
struct Control {
    serial: u64,
    mode: Option<(ModeId, Instant)>,
    closed: BTreeSet<ModeId>,
    ui_closed: u64,
    ui_latest: u64,
    local: Option<Preview>,
    remote: BTreeMap<DeviceId, Preview>,
    seen: BTreeMap<DeviceId, u64>,
    pending: Throttle<Action>,
    immediate: Vec<Action>,
    sequence: u64,
    gesture: Option<Gesture>,
    gestures: BTreeMap<DeviceId, Instant>,
    input_gesture: bool,
}
struct Preview {
    layout: Layout,
    affected: Vec<ScreenId>,
    expires: Instant,
}
struct Gesture {
    initial: Layout,
    projection: Projection,
    handle: oink_display::projection::Handle,
}

struct Throttle<T> {
    last: Option<Instant>,
    pending: Option<T>,
}
impl<T> Default for Throttle<T> {
    fn default() -> Self {
        Self {
            last: None,
            pending: None,
        }
    }
}
impl<T> Throttle<T> {
    fn push(&mut self, value: T) {
        self.pending = Some(value);
    }
    fn take(&mut self, now: Instant) -> Option<T> {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < INTERVAL) {
            return None;
        }
        let value = self.pending.take()?;
        self.last = Some(now);
        Some(value)
    }
    fn barrier(&mut self) {
        self.pending = None;
    }
}

pub(crate) fn status(node: &Node) -> api::ProjectionStatus {
    api::ProjectionStatus {
        editing: lock(&node.inner.projection.control).mode.is_some(),
    }
}
fn emit_status(node: &Node) {
    node.emit(api::ApiEvent::LayoutProjection(status(node)));
}

pub(crate) fn set_mode(node: &Node, enabled: bool) -> api::ProjectionStatus {
    let mut c = lock(&node.inner.projection.control);
    if enabled && c.mode.is_none() {
        c.serial += 1;
        let id = ModeId {
            owner: node.id(),
            serial: c.serial,
            boot_nonce: node.boot().nonce,
        };
        c.mode = Some((id, Instant::now() + LEASE));
        c.immediate.push(Action::Mode { id, enabled: true });
    } else if !enabled && let Some((id, _)) = c.mode.take() {
        c.closed.insert(id);
        c.gesture = None;
        c.gestures.clear();
        c.remote.clear();
        c.local = None;
        c.pending.barrier();
        c.immediate.extend([
            Action::Mode { id, enabled: false },
            Action::Gesture { id, active: false },
        ]);
        clear_locked(&mut c);
    }
    drop(c);
    node.inner.projection.wake.notify_one();
    emit_status(node);
    status(node)
}
fn clear_locked(c: &mut Control) {
    c.sequence += 1;
    c.pending.barrier();
    c.immediate.push(Action::Preview {
        sequence: c.sequence,
        screens: None,
        affected: vec![],
    });
    c.local = None;
}
fn preview_locked(c: &mut Control, layout: Layout, affected: Vec<ScreenId>, now: Instant) {
    c.sequence += 1;
    let message = Action::Preview {
        sequence: c.sequence,
        screens: Some(
            layout
                .screens
                .iter()
                .map(|s| ScreenRect {
                    device: s.id.device,
                    display: s.id.display.clone(),
                    rect: s.rect,
                })
                .collect(),
        ),
        affected: affected.iter().map(|id| (id.device, id.display.clone())).collect(),
    };
    if !message.valid() {
        return;
    }
    c.pending.push(message);
    c.local = Some(Preview {
        layout,
        affected,
        expires: now + LEASE,
    });
}

pub(crate) fn preview_ui(
    node: &Node,
    gesture: u64,
    placements: Vec<api::Placement>,
    affected: Vec<api::ScreenKey>,
    ended: bool,
) -> api::ApiResult<()> {
    let mut c = lock(&node.inner.projection.control);
    if gesture < c.ui_latest || gesture <= c.ui_closed {
        return Ok(());
    }
    c.ui_latest = gesture;
    if ended {
        c.ui_closed = gesture;
        clear_locked(&mut c);
    } else {
        if placements.len() > 64 || affected.len() > 64 {
            return Err(api::ApiError::InvalidArgument {
                message: "预览屏幕过多".into(),
            });
        }
        let mut layout = node.layout().0;
        let mut seen = std::collections::BTreeSet::new();
        for p in placements {
            if !seen.insert(p.device.0) || !layout.place_device(p.device.0, p.origin, p.scale, false) {
                return Err(api::ApiError::InvalidArgument {
                    message: "Invalid computer placement".into(),
                });
            }
        }
        preview_locked(
            &mut c,
            layout,
            affected
                .into_iter()
                .map(|id| ScreenId::new(id.device.0, id.display))
                .collect(),
            Instant::now(),
        );
    }
    drop(c);
    node.inner.projection.wake.notify_one();
    Ok(())
}

fn receive(node: &Node, peer: DeviceId, msg: Action, now: Instant) {
    if !msg.valid() {
        return;
    }
    let mut c = lock(&node.inner.projection.control);
    let was_editing = c.mode.is_some();
    match msg {
        Action::Preview {
            sequence,
            screens,
            affected,
        } => {
            if c.seen.get(&peer).is_some_and(|old| *old >= sequence) {
                return;
            }
            c.seen.insert(peer, sequence);
            c.remote.remove(&peer);
            if let Some(screens) = screens {
                let mut layout = node.layout().0;
                for p in screens {
                    if let Some(s) = layout.screen_mut(&ScreenId::new(p.device, p.display)) {
                        s.rect = p.rect;
                    }
                }
                if crate::config_module::layout_wire::to_wire(&layout, oink_protocol::v1::dto::LayoutSource::Manual)
                    .is_err()
                {
                    return;
                }
                c.remote.insert(
                    peer,
                    Preview {
                        layout,
                        affected: affected
                            .into_iter()
                            .map(|(device, display)| ScreenId::new(device, display))
                            .collect(),
                        expires: now + LEASE,
                    },
                );
            }
        }
        Action::Mode { id, enabled } => {
            if !enabled {
                c.closed.insert(id);
                if c.mode.is_some_and(|(active, _)| active == id) {
                    c.mode = None;
                    c.gesture = None;
                    c.gestures.clear();
                    c.local = None;
                    c.remote.clear();
                    c.pending.barrier();
                }
            } else if id.owner == peer
                && node.links().is_ok_and(|links| {
                    links
                        .links()
                        .by_peer(peer)
                        .is_some_and(|p| p.peer_boot.nonce == id.boot_nonce)
                })
                && !c.closed.contains(&id)
                && c.mode.is_none_or(|(active, _)| active == id || id < active)
            {
                c.mode = Some((id, now + LEASE));
            }
            while c.closed.len() > 128 {
                c.closed.pop_first();
            }
        }
        Action::Gesture { id, active } => {
            if !c.mode.is_some_and(|(current, _)| current == id) {
                return;
            }
            if active {
                c.gestures.insert(peer, now + LEASE);
            } else {
                c.gestures.remove(&peer);
            }
        }
    }
    let changed = was_editing != c.mode.is_some();
    drop(c);
    if changed {
        emit_status(node);
    }
}

async fn native_event(node: &Node, event: Event, now: Instant) {
    if matches!(event, Event::Escape) {
        set_mode(node, false);
        return;
    }
    let Event::Drag {
        projection,
        handle,
        delta,
        phase,
    } = event
    else {
        return;
    };
    tracing::debug!(?phase, ?handle, delta, "projection gesture received");
    let (initial, next) = {
        let mut c = lock(&node.inner.projection.control);
        if c.mode.is_none() {
            return;
        }
        if phase == DragPhase::Begin {
            if node.input_thread().is_ok_and(|i| i.status().dnd_active) {
                return;
            }
            c.gesture = Some(Gesture {
                initial: node.layout().0,
                projection: *projection,
                handle,
            });
            let id = c.mode.expect("active mode").0;
            c.immediate.push(Action::Gesture { id, active: true });
        }
        let Some(gesture) = &c.gesture else { return };
        let Some(next) = gesture.projection.edit(&gesture.initial, gesture.handle, delta) else {
            if phase == DragPhase::End {
                c.gesture = None;
                let id = c.mode.expect("active mode").0;
                c.immediate.push(Action::Gesture { id, active: false });
                clear_locked(&mut c);
            }
            return;
        };
        let affected = next
            .screens
            .iter()
            .filter(|s| s.id.device == gesture.projection.neighbour.device)
            .map(|s| s.id.clone())
            .collect();
        if phase != DragPhase::End {
            preview_locked(&mut c, next, affected, now);
            return;
        }
        let initial = c.gesture.take().expect("gesture present").initial;
        let id = c.mode.expect("active mode").0;
        c.immediate.push(Action::Gesture { id, active: false });
        clear_locked(&mut c);
        (initial, next)
    };
    if initial == next {
        return;
    }
    if let Ok(group) = node.link_config() {
        // The config owner performs the comparison inside its normal transaction;
        // a concurrent UI/C0 change cancels this gesture rather than overwriting it.
        let result = tokio::task::spawn_blocking(move || {
            group.edit_layout(
                Box::new(move |current| {
                    if *current == initial {
                        *current = next;
                    }
                }),
                v1::dto::LayoutSource::Manual,
            )
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(?result, "projection edit commit failed");
        }
    }
}

fn expire_previews(c: &mut Control, now: Instant) {
    if c.local.as_ref().is_some_and(|p| p.expires <= now) && c.gesture.is_none() {
        clear_locked(c);
    }
    c.remote.retain(|_, p| p.expires > now);
}

fn tick(node: &Node, now: Instant, heartbeat: bool) -> Vec<Action> {
    let mut c = lock(&node.inner.projection.control);
    let mut changed = false;
    if let Some((id, expiry)) = c.mode {
        if id.owner == node.id() {
            c.mode = Some((id, now + LEASE));
            if heartbeat {
                c.immediate.push(Action::Mode { id, enabled: true });
            }
        } else if expiry <= now {
            c.mode = None;
            c.gesture = None;
            c.gestures.clear();
            changed = true;
        }
    }
    expire_previews(&mut c, now);
    c.gestures.retain(|_, expiry| *expiry > now);
    if c.gesture.is_some() && heartbeat {
        let id = c.mode.expect("active mode").0;
        c.immediate.push(Action::Gesture { id, active: true });
    }
    if heartbeat && let Some(local) = c.local.as_ref() {
        let (layout, affected) = (local.layout.clone(), local.affected.clone());
        // Native held gestures renew themselves; UI previews require the UI's
        // explicit lease renewal so a closed/crashed editor cannot leave overlays.
        let expiry = local.expires;
        preview_locked(&mut c, layout, affected, now);
        if c.gesture.is_none()
            && let Some(p) = c.local.as_mut()
        {
            p.expires = expiry;
        }
    }
    let active_gesture = c.gesture.is_some() || !c.gestures.is_empty();
    if c.input_gesture != active_gesture || (active_gesture && heartbeat) {
        c.input_gesture = active_gesture;
        if let Ok(input) = node.input_thread() {
            input.send(crate::threads::input::InputCommand::ProjectionGesture(active_gesture));
        }
    }
    let mut messages = std::mem::take(&mut c.immediate);
    if let Some(message) = c.pending.take(now) {
        messages.push(message);
    }
    drop(c);
    if changed {
        emit_status(node);
    }
    messages
}

fn draw(node: &Node, online: &BTreeSet<DeviceId>) {
    let Some(surface) = node.inner.projection.surface.get() else {
        return;
    };
    surface.update(view(node, online));
}

fn view(node: &Node, online: &BTreeSet<DeviceId>) -> View {
    let c = lock(&node.inner.projection.control);
    let editing = c.mode.is_some();
    let preview = c.local.iter().chain(c.remote.values()).max_by_key(|p| p.expires);
    if !editing && preview.is_none() {
        return View::default();
    }
    let group = node.link_config().expect("started config");
    let applied = group.applied();
    let current = &*applied.layout;
    let layout = preview.map_or(current, |p| &p.layout);
    let displays = group.local_displays();
    let projections = oink_display::projection::compute(
        &layout.effective(&applied.owners),
        node.id(),
        |d| online.contains(d),
        None,
    )
    .into_iter()
    .filter_map(|geometry| {
        let d = displays
            .displays
            .iter()
            .find(|d| d.key == geometry.host.display && !d.is_virtual)?;
        Some(NamedProjection {
            name: node.device_name(geometry.neighbour.device),
            geometry,
            pixels_per_unit: d.pixels_per_unit(),
            screen: d.bounds,
        })
    })
    .collect();
    let accent = node.resolved_accent();
    View {
        projections,
        accent,
        show_handles: editing,
        editable: editing,
    }
}

pub(crate) fn start(node: Node, mut port: ModulePort, native: bool) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let links = node.links()?;
    let group = node.link_config()?;
    if native && let Some(surface) = ProjectionOverlay::platform() {
        let _ = node.inner.projection.surface.set(surface);
    }
    let mut streams = ProjectionStreams::new(links.clone(), group.clone());
    let wake = streams.wake();
    let mut changes = group.subscribe();
    let mut stop = node.stop_receiver();
    Ok(tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_heartbeat = Instant::now();
        let mut last_draw = Instant::now() - HEARTBEAT;
        let mut drawn = false;
        let mut online = BTreeSet::from([node.id()]);
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = interval.tick() => {},
                _ = node.inner.projection.wake.notified() => {},
                _ = wake.notified() => {},
                change = changes.next() => match change {
                    Some(crate::config_module::ConfigChange::Facts { device, capabilities_changed }) => streams.facts_changed(device, capabilities_changed),
                    Some(_) => {}, None => break,
                },
                event = port.next() => match event {
                    Some(ModuleEvent::LinkReady { link, peer }) => { streams.link_ready(link, peer); online.insert(peer); },
                    Some(ModuleEvent::LinkLost { link, peer, reason }) => {
                        streams.link_lost(link, peer, reason); online.remove(&peer);
                        let mut c = lock(&node.inner.projection.control);
                        c.remote.remove(&peer); c.seen.remove(&peer); c.gestures.remove(&peer);
                        let closed_mode = c.mode.is_some_and(|(id, _)| id.owner == peer);
                        if closed_mode { c.mode = None; c.gesture = None; }
                        drop(c);
                        if closed_mode { emit_status(&node); }
                    },
                    Some(ModuleEvent::Inbound(s)) => streams.inbound(s),
                    Some(ModuleEvent::InboundLow(op)) => streams.inbound_low(*op),
                    Some(ModuleEvent::StreamClosed { stream, reason }) => streams.stream_closed(stream, reason),
                    Some(ModuleEvent::PathChanged { .. }) | Some(ModuleEvent::ProtocolViolation { .. }) => {}, None => break,
                }
            }
            let now = Instant::now();
            if let Some(surface) = node.inner.projection.surface.get() {
                for event in surface.take_events() {
                    native_event(&node, event, now).await;
                }
            }
            for (peer, message) in streams.poll(now) {
                receive(&node, peer, message, now);
            }
            let heartbeat = now.duration_since(last_heartbeat) >= HEARTBEAT;
            if heartbeat {
                last_heartbeat = now;
            }
            let messages = tick(&node, now, heartbeat);
            let dirty = !messages.is_empty();
            for peer in online.iter().copied().filter(|peer| *peer != node.id()) {
                for message in &messages {
                    // Reopen seeds are snapshots only; no edit/commit history.
                    let opening = current_messages(&node)
                        .iter()
                        .map(wire::encode)
                        .collect::<v1::Result<Vec<_>>>();
                    let (Ok(opening), Ok(message)) = (opening, wire::encode(message)) else {
                        tracing::warn!("invalid local projection state");
                        continue;
                    };
                    streams.set_opening(peer, opening);
                    let _ = streams.send(peer, &message, now);
                }
            }
            let active = {
                let c = lock(&node.inner.projection.control);
                c.mode.is_some() || c.local.is_some() || !c.remote.is_empty()
            };
            if active && (dirty || heartbeat || now.duration_since(last_draw) >= INTERVAL) {
                draw(&node, &online);
                last_draw = now;
                drawn = true;
            } else if !active && drawn {
                if let Some(surface) = node.inner.projection.surface.get() {
                    surface.hide();
                }
                drawn = false;
            }
            streams.take_closed();
        }
        if let Some(surface) = node.inner.projection.surface.get() {
            surface.shutdown();
        }
        if let Ok(input) = node.input_thread() {
            input.send(crate::threads::input::InputCommand::ProjectionGesture(false));
        }
    }))
}
fn current_messages(node: &Node) -> Vec<Action> {
    let c = lock(&node.inner.projection.control);
    let mut messages = Vec::new();
    if let Some((id, _)) = c.mode
        && id.owner == node.id()
    {
        messages.push(Action::Mode { id, enabled: true });
    }
    if let Some(p) = &c.local {
        messages.push(Action::Preview {
            sequence: c.sequence,
            screens: Some(
                p.layout
                    .screens
                    .iter()
                    .map(|s| ScreenRect {
                        device: s.id.device,
                        display: s.id.display.clone(),
                        rect: s.rect,
                    })
                    .collect(),
            ),
            affected: p.affected.iter().map(|s| (s.device, s.display.clone())).collect(),
        });
    }
    messages
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn abandoned_previews_expire_without_renewing_the_ui_lease() {
        let now = Instant::now();
        let mut c = Control::default();
        preview_locked(&mut c, Layout::default(), vec![], now);
        c.remote.insert(
            DeviceId([2; 32]),
            Preview {
                layout: Layout::default(),
                affected: vec![],
                expires: now + LEASE,
            },
        );
        expire_previews(&mut c, now + LEASE - Duration::from_millis(1));
        assert!(c.local.is_some() && !c.remote.is_empty());
        expire_previews(&mut c, now + LEASE);
        assert!(c.local.is_none() && c.remote.is_empty());
        assert!(c.pending.pending.is_none());
        assert!(matches!(
            c.immediate.last(),
            Some(Action::Preview { screens: None, .. })
        ));
    }
    #[test]
    fn preview_rate_is_bounded_latest_wins_and_terminal_clear_cannot_resurrect() {
        let now = Instant::now();
        let mut t = Throttle::default();
        t.push(0);
        assert_eq!(t.take(now), Some(0));
        for i in 1..100 {
            t.push(i);
            assert_eq!(t.take(now + Duration::from_millis(1)), None);
        }
        assert_eq!(t.take(now + INTERVAL), Some(99));
        t.push(100);
        t.barrier();
        assert_eq!(t.take(now + INTERVAL * 2), None);
    }
}

#[cfg(all(test, debug_assertions))]
mod integration_tests {
    use super::*;
    use crate::api::AgentApi;
    use crate::snapshot_tests::{Displays, common, start_with_clip};

    #[test]
    fn screenshot_short_seam_projects_during_preview_and_hides_afterward() {
        crate::test_watchdog::run(module_path!(), "projection short seam", Some(6), async {
            use crate::node::NodeOptions;
            use oink_foundation::{Point, Rect};
            use std::sync::Arc;

            let dirs = [
                tempfile::tempdir().unwrap(),
                tempfile::tempdir().unwrap(),
                tempfile::tempdir().unwrap(),
            ];
            let mut nodes = Vec::new();
            for (i, dir) in dirs.iter().enumerate() {
                let mut display = Displays::new("screen").info();
                if i == 0 {
                    display.bounds = Rect::new(0., 0., 3840., 2160.);
                    display.pixel_width = 3840;
                    display.pixel_height = 2160;
                }
                display.reference_extent = None;
                nodes.push(
                    Node::start(NodeOptions {
                        net: oink_test_support::network_config(dir.path().into(), "projection-seam", 0),
                        input: Arc::new(oink_test_support::input::FakeInput::new()),
                        displays: vec![display],
                        display_backend: None,
                        clipboard: None,
                        mode_switch_in_callback: true,
                        capture_reports_key_repeat: false,
                        cursor_trail: false,
                    })
                    .await
                    .unwrap(),
                );
            }
            let (upper, left, right) = (&nodes[0], &nodes[1], &nodes[2]);
            common::pair(upper, left).await;
            common::pair(upper, right).await;
            common::until("three-screen layout converges", || {
                let layout = upper.layout().0;
                layout.screens.len() == 3 && nodes.iter().all(|node| node.layout().0 == layout)
            })
            .await;
            let online = nodes.iter().map(Node::id).collect();
            for node in &nodes {
                assert_eq!(view(node, &online), View::default());
            }
            let placements = nodes
                .iter()
                .zip([
                    (493., 0., 741., 3840.),
                    (0., 416.8125 - 1e-8, 604., 1728.),
                    (604., 472.8125, 604., 1728.),
                ])
                .map(|(node, (x, y, width, native_width))| api::Placement {
                    device: node.id().into(),
                    origin: Point::new(x / 4096., y / 4096.),
                    scale: width / native_width / 4096.,
                })
                .collect::<Vec<_>>();
            // Exercise the real UI preview API and P0 receiver without native windows.
            preview_ui(upper, 1, placements.clone(), vec![], false).unwrap();
            common::until("short seam preview reaches all peers", || {
                [left, right]
                    .iter()
                    .all(|node| lock(&node.inner.projection.control).remote.contains_key(&upper.id()))
            })
            .await;
            for node in &nodes {
                let preview = view(node, &online);
                assert_eq!(preview.projections.len(), 2);
                assert!(!preview.editable && !preview.show_handles);
            }
            let preview = view(upper, &online);
            let short = &preview
                .projections
                .iter()
                .find(|p| p.geometry.neighbour.device == left.id())
                .unwrap()
                .geometry;
            assert_eq!(short.side, oink_display::layout::Side::Bottom);
            assert!((short.clip.w - 111. / 741. * 3840.).abs() < 1e-8);
            preview_ui(upper, 1, vec![], vec![], true).unwrap();
            common::until("preview end hides every screen", || {
                nodes.iter().all(|node| view(node, &online) == View::default())
            })
            .await;
            preview_ui(upper, 2, placements, vec![], false).unwrap();
            let expires = lock(&upper.inner.projection.control).local.as_ref().unwrap().expires;
            tick(upper, expires - Duration::from_millis(1), true);
            assert!(!view(upper, &online).projections.is_empty());
            tick(upper, expires, false);
            assert_eq!(
                view(upper, &online),
                View::default(),
                "heartbeats cannot keep an abandoned UI preview visible"
            );
            for node in nodes {
                node.shutdown().await;
            }
        });
    }

    #[test]
    fn previews_are_ephemeral_and_native_edits_commit_through_c0() {
        crate::test_watchdog::run(module_path!(), "projection C0", Some(6), scenario());
    }
    async fn scenario() {
        let ad = tempfile::tempdir().unwrap();
        let bd = tempfile::tempdir().unwrap();
        // Removed preferences are ordinary extras, including an old false value.
        for (dir, value) in [(ad.path(), true), (bd.path(), false)] {
            let mut config = crate::config::AgentConfig::load_from(dir).unwrap();
            config.save().unwrap();
            let path = config.settings_path();
            let mut raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            raw["local"]["layout_projection_enabled"] = value.into();
            std::fs::write(path, serde_json::to_vec(&raw).unwrap()).unwrap();
        }
        let a = start_with_clip(
            &oink_test_support::network_config(ad.path().into(), "projection-a", 0),
            &Displays::new("a"),
            None,
        )
        .await;
        let b = start_with_clip(
            &oink_test_support::network_config(bd.path().into(), "projection-b", 0),
            &Displays::new("b"),
            None,
        )
        .await;
        common::pair(&a, &b).await;
        common::until("same layout", || {
            a.layout().0 == b.layout().0 && a.layout().0.screens.len() == 2
        })
        .await;
        let online = BTreeSet::from([a.id(), b.id()]);
        let idle = view(&a, &online);
        assert_eq!(idle, View::default(), "idle projections must be hidden");
        let input_streams = [common::input_id(&a, b.id()), common::input_id(&b, a.id())];
        let initial = a.layout().0;
        let version = a.layout().1.revision;
        let id = initial.screens[0].id.clone();
        let mut placements: Vec<_> = initial
            .screens
            .iter()
            .map(|s| {
                let (origin, scale) = initial.device_transform(s.id.device).unwrap();
                api::Placement {
                    device: s.id.device.into(),
                    origin,
                    scale,
                }
            })
            .collect();
        // A stationary canvas press projects the unchanged group geometry. P0
        // must reach the peer without entering native edit mode or saving files.
        let paths: Vec<_> = [ad.path(), bd.path()]
            .into_iter()
            .flat_map(|dir| {
                [
                    dir.join(crate::config::SETTINGS_FILE),
                    dir.join(crate::config_module::group_store::STATE_FILE),
                ]
            })
            .collect();
        let saved: Vec<_> = paths.iter().map(|path| std::fs::read(path).unwrap()).collect();
        preview_ui(&a, 1, placements.clone(), vec![], false).unwrap();
        common::until("stationary press reaches B", || {
            lock(&b.inner.projection.control)
                .remote
                .get(&a.id())
                .is_some_and(|p| p.layout == initial && p.affected.is_empty())
        })
        .await;
        assert!(!status(&a).editing && !status(&b).editing);
        for node in [&a, &b] {
            let pressed = view(node, &online);
            assert!(!pressed.projections.is_empty(), "stationary press shows projections");
            assert!(!pressed.editable && !pressed.show_handles);
        }
        preview_ui(&a, 1, vec![], vec![], true).unwrap();
        common::until("stationary click clears B", || {
            lock(&b.inner.projection.control).remote.is_empty()
        })
        .await;
        assert_eq!(view(&a, &online), idle, "ending a preview hides projections");
        assert_eq!(view(&b, &online), View::default());
        assert_eq!(a.layout().0, initial);
        assert_eq!(b.layout().0, initial);
        assert_eq!(a.layout().1.revision, version);
        assert_eq!(
            paths
                .iter()
                .map(|path| std::fs::read(path).unwrap())
                .collect::<Vec<_>>(),
            saved
        );
        set_mode(&a, true);
        common::until("mode reaches B", || status(&b).editing).await;
        for node in [&a, &b] {
            let editing = view(node, &online);
            assert!(!editing.projections.is_empty());
            assert!(editing.editable && editing.show_handles);
        }
        placements[0].origin.y += 20.0 / 686.0;
        let affected = vec![api::ScreenKey::from(&id)];
        preview_ui(&a, 2, placements.clone(), affected.clone(), false).unwrap();
        common::until("P0 preview reaches B", || {
            lock(&b.inner.projection.control).remote.get(&a.id()).is_some_and(|p| {
                (p.layout.screen(&id).unwrap().rect.y - initial.screen(&id).unwrap().rect.y - 20.0 / 686.0).abs() < 1e-8
            })
        })
        .await;
        assert_eq!(a.layout().0, initial);
        assert_eq!(b.layout().0, initial);
        assert_eq!(a.layout().1.revision, version);
        preview_ui(&a, 2, vec![], vec![], true).unwrap();
        preview_ui(&a, 2, placements, affected, false).unwrap(); // late IPC result after pointer-up
        common::until("clear reaches B", || {
            lock(&b.inner.projection.control).remote.is_empty()
        })
        .await;
        assert!(lock(&a.inner.projection.control).local.is_none());
        assert!(
            !view(&a, &online).projections.is_empty(),
            "edit mode survives preview end"
        );
        let closed_id = lock(&a.inner.projection.control).mode.unwrap().0;
        native_event(&b, Event::Escape, Instant::now()).await;
        common::until("Esc on B ends A's mode", || !status(&a).editing && !status(&b).editing).await;
        assert_eq!(view(&a, &online), View::default());
        assert_eq!(view(&b, &online), View::default());
        receive(
            &b,
            a.id(),
            Action::Mode {
                id: closed_id,
                enabled: true,
            },
            Instant::now(),
        );
        assert!(!status(&b).editing, "late renewal cannot resurrect a closed mode");
        set_mode(&a, true);
        common::until("new mode reaches B", || status(&b).editing).await;
        receive(
            &b,
            a.id(),
            Action::Gesture {
                id: closed_id,
                active: true,
            },
            Instant::now(),
        );
        assert!(
            lock(&b.inner.projection.control).gestures.is_empty(),
            "an old mode cannot block crossing in a new one"
        );
        let p = oink_display::projection::compute(
            &initial.effective(&b.link_config().unwrap().applied().owners),
            b.id(),
            |_| true,
            None,
        )
        .remove(0);
        let old = initial.screen(&p.neighbour).unwrap().rect;
        for (phase, delta) in [(DragPhase::Begin, 0.0), (DragPhase::End, f64::NAN)] {
            native_event(
                &b,
                Event::Drag {
                    projection: Box::new(p.clone()),
                    handle: oink_display::projection::Handle::Middle,
                    delta,
                    phase,
                },
                Instant::now(),
            )
            .await;
        }
        assert!(
            lock(&b.inner.projection.control).gesture.is_none(),
            "even an invalid up must release the gesture barrier"
        );
        assert_eq!(b.layout().1.revision, version);
        for phase in [DragPhase::Begin, DragPhase::End] {
            native_event(
                &b,
                Event::Drag {
                    projection: Box::new(p.clone()),
                    handle: oink_display::projection::Handle::End,
                    delta: 0.0,
                    phase,
                },
                Instant::now(),
            )
            .await;
        }
        assert_eq!(b.layout().1.revision, version, "a click is not a config edit");

        native_event(
            &b,
            Event::Drag {
                projection: Box::new(p.clone()),
                handle: oink_display::projection::Handle::End,
                delta: 0.0,
                phase: DragPhase::Begin,
            },
            Instant::now(),
        )
        .await;
        native_event(
            &b,
            Event::Drag {
                projection: Box::new(p.clone()),
                handle: oink_display::projection::Handle::End,
                delta: 40.0,
                phase: DragPhase::Update,
            },
            Instant::now(),
        )
        .await;
        assert_eq!(b.layout().1.revision, version);
        native_event(
            &b,
            Event::Drag {
                projection: Box::new(p.clone()),
                handle: oink_display::projection::Handle::End,
                delta: 40.0,
                phase: DragPhase::End,
            },
            Instant::now(),
        )
        .await;
        common::until("native resize commits over C0", || {
            a.layout().0 == b.layout().0 && b.layout().1.revision > version
        })
        .await;
        let next = a.layout().0;
        let resized = next.screen(&p.neighbour).unwrap();
        assert!((resized.rect.w / resized.rect.h - old.w / old.h).abs() < 1e-8);
        assert_eq!(resized.size_source, oink_display::layout::SizeSource::Manual);
        assert_eq!(
            [common::input_id(&a, b.id()), common::input_id(&b, a.id())],
            input_streams
        );
        set_mode(&a, false);
        let _ = b.set_layout_projection(false).unwrap();
        a.shutdown().await;
        b.shutdown().await;
    }
}
