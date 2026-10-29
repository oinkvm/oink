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
use crate::layout_profiles::{render as arrangement, types::ArrangementSnapshot};
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
    #[cfg(test)]
    lease_clock: OnceLock<Mutex<Instant>>,
    #[cfg(test)]
    wall_offset_ms: std::sync::atomic::AtomicI64,
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
    refusal_log: Option<Instant>,
}
struct Preview {
    layout: Layout,
    arrangement: Option<Box<ArrangementSnapshot>>,
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
// Tests can freeze preview leases while real stream pacing and heartbeats run.
fn lease_now(node: &Node, now: Instant) -> Instant {
    #[cfg(test)]
    if let Some(clock) = node.inner.projection.lease_clock.get() {
        return *lock(clock);
    }
    let _ = node;
    now
}

// Absolute timestamps remain sender-local metadata; tests can model unsynchronised peers.
fn preview_wall_now(node: &Node) -> u64 {
    let now = crate::layout_sync::now_ms();
    #[cfg(test)]
    return now.saturating_add_signed(
        node.inner
            .projection
            .wall_offset_ms
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    #[cfg(not(test))]
    {
        let _ = node;
        now
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
        c.mode = Some((id, lease_now(node, Instant::now()) + LEASE));
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
fn preview_locked(
    node: &Node,
    c: &mut Control,
    layout: Layout,
    snapshot: Option<Box<ArrangementSnapshot>>,
    affected: Vec<ScreenId>,
    now: Instant,
) {
    let snapshot = snapshot.unwrap_or_else(|| {
        Box::new(arrangement::capture(
            &layout,
            &node.link_config().expect("started config").applied().owners,
        ))
    });
    c.sequence += 1;
    let message = Action::Arrangement {
        sequence: c.sequence,
        lease_ms: LEASE.as_millis() as u32,
        expires_ms: preview_wall_now(node) + LEASE.as_millis() as u64,
        snapshot: Some(snapshot.clone()),
    };
    if !message.valid() {
        return;
    }
    c.pending.push(message);
    c.local = Some(Preview {
        layout,
        arrangement: Some(snapshot),
        affected,
        expires: now + LEASE,
    });
}

#[cfg(test)]
pub(crate) fn preview_ui(
    node: &Node,
    gesture: u64,
    placements: Vec<api::Placement>,
    affected: Vec<api::ScreenKey>,
    ended: bool,
) -> api::ApiResult<()> {
    preview_ui_arrangement(node, gesture, placements, affected, ended, None)
}
pub(crate) fn preview_ui_arrangement(
    node: &Node,
    gesture: u64,
    placements: Vec<api::Placement>,
    affected: Vec<api::ScreenKey>,
    ended: bool,
    snapshot: Option<Box<ArrangementSnapshot>>,
) -> api::ApiResult<()> {
    let mut c = lock(&node.inner.projection.control);
    if gesture < c.ui_latest || gesture <= c.ui_closed {
        return Ok(());
    }
    if gesture != c.ui_latest || ended {
        tracing::info!(gesture, ended, placements = placements.len(), "projection UI request");
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
        if let Some(snapshot) = &snapshot {
            let owners = node.link_config().expect("started config").applied().owners.clone();
            layout = arrangement::render(snapshot, &layout, &owners)
                .map_err(|e| api::ApiError::InvalidArgument { message: e.to_string() })?
                .0;
        }
        preview_locked(
            node,
            &mut c,
            layout,
            snapshot,
            affected
                .into_iter()
                .map(|id| ScreenId::new(id.device.0, id.display))
                .collect(),
            lease_now(node, Instant::now()),
        );
    }
    drop(c);
    node.inner.projection.wake.notify_one();
    Ok(())
}

// A malformed or obsolete frame can arrive at preview frequency. Bound refusal
// diagnostics globally; lifecycle transitions below log once per preview.
fn refusal(c: &mut Control, peer: DeviceId, reason: &'static str, now: Instant) {
    if c.refusal_log
        .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5))
    {
        tracing::info!(?peer, reason, "projection preview refused");
        c.refusal_log = Some(now);
    }
}

fn receive(node: &Node, peer: DeviceId, msg: Action, now: Instant) {
    let now = lease_now(node, now);
    let mut c = lock(&node.inner.projection.control);
    if !msg.valid() {
        refusal(&mut c, peer, "invalid_payload", now);
        return;
    }
    let was_editing = c.mode.is_some();
    match msg {
        Action::Arrangement {
            sequence,
            lease_ms,
            expires_ms,
            snapshot,
        } => {
            // Peer wall clocks are not synchronised. A forward skew of only
            // 250 ms used to reject every Windows-originated preview on Macs.
            // Bound residence using the sender's remaining TTL and our monotonic
            // clock; sequence fencing prevents cleared/older frames from reviving.
            if c.seen.get(&peer).is_some_and(|old| *old >= sequence) {
                refusal(&mut c, peer, "stale_sequence", now);
                return;
            }
            c.seen.insert(peer, sequence);
            let was_visible = c.remote.remove(&peer).is_some();
            if let Some(snapshot) = snapshot {
                let applied = node.link_config().expect("started config").applied();
                let Ok((layout, _)) = arrangement::render(&snapshot, &applied.layout, &applied.owners) else {
                    refusal(&mut c, peer, "render_validation", now);
                    return;
                };
                if !was_visible {
                    tracing::info!(?peer, sequence, lease_ms,
                        sender_expiry_delta_ms = %(i128::from(expires_ms) - i128::from(crate::layout_sync::now_ms())),
                        "projection remote preview accepted");
                }
                c.remote.insert(
                    peer,
                    Preview {
                        layout,
                        arrangement: Some(snapshot),
                        affected: vec![],
                        expires: now + Duration::from_millis(u64::from(lease_ms)),
                    },
                );
            } else if was_visible {
                tracing::info!(?peer, sequence, "projection remote preview cleared");
            }
        }
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
                        arrangement: None,
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
    let now = lease_now(node, now);
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
        let owners = visible_owners(node, &node.link_config().expect("started config").applied().owners);
        let Some(next) = gesture
            .projection
            .edit_with_preferences(&gesture.initial, &owners, gesture.handle, delta)
        else {
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
            preview_locked(node, &mut c, next, None, affected, now);
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
    let node = node.clone();
    let result = tokio::task::spawn_blocking(move || {
        node.commit_manual_layout_exact(move |current| {
            if *current == initial {
                *current = next;
            }
            Ok(())
        })
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        tracing::warn!(?result, "projection edit commit failed");
    }
}

fn expire_previews(c: &mut Control, now: Instant) {
    if c.local.as_ref().is_some_and(|p| p.expires <= now) && c.gesture.is_none() {
        clear_locked(c);
    }
    c.remote.retain(|peer, p| {
        if p.expires <= now {
            tracing::info!(?peer, "projection remote lease expired");
            false
        } else {
            true
        }
    });
}

fn tick(node: &Node, now: Instant, heartbeat: bool) -> Vec<Action> {
    let lease_now = lease_now(node, now);
    let mut c = lock(&node.inner.projection.control);
    let mut changed = false;
    if let Some((id, expiry)) = c.mode {
        if id.owner == node.id() {
            c.mode = Some((id, lease_now + LEASE));
            if heartbeat {
                c.immediate.push(Action::Mode { id, enabled: true });
            }
        } else if expiry <= lease_now {
            c.mode = None;
            c.gesture = None;
            c.gestures.clear();
            changed = true;
        }
    }
    expire_previews(&mut c, lease_now);
    c.gestures.retain(|_, expiry| *expiry > lease_now);
    if c.gesture.is_some() && heartbeat {
        let id = c.mode.expect("active mode").0;
        c.immediate.push(Action::Gesture { id, active: true });
    }
    if heartbeat && let Some(local) = c.local.as_ref() {
        let (layout, affected, snapshot) = (local.layout.clone(), local.affected.clone(), local.arrangement.clone());
        // Native held gestures renew themselves; UI previews require the UI's
        // explicit lease renewal so a closed/crashed editor cannot leave overlays.
        let expiry = local.expires;
        preview_locked(node, &mut c, layout, snapshot, affected, lease_now);
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
    if let Some(mut message) = c.pending.take(now) {
        if let Action::Arrangement {
            lease_ms, expires_ms, ..
        } = &mut message
        {
            let remaining = c.local.as_ref().map_or(0, |p| remaining_lease_ms(p.expires, lease_now));
            *lease_ms = remaining;
            *expires_ms = preview_wall_now(node) + u64::from(remaining);
        }
        if message.valid() {
            messages.push(message);
        }
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

// Pause is runtime state, not a stored arrangement preference. Apply it after
// rendering any preview, so stale snapshots cannot make paused screens visible.
fn visible_owners(node: &Node, owners: &arrangement::Owners) -> arrangement::Owners {
    let mut visible = owners.clone();
    if let Ok(config) = node.link_config() {
        for member in &config.members().members {
            if member.facts.as_ref().is_some_and(|facts| {
                facts
                    .pause()
                    .ok()
                    .flatten()
                    .is_some_and(|pause| pause.paused() == Ok(true))
            }) {
                visible.entry(member.device).or_default().screen_shared = false;
            }
        }
    }
    visible
}

// Label selection is local rendering over the already synchronized inventory.
fn projection_name(layout: &Layout, screen: &ScreenId, computer_name: String) -> String {
    // Use the complete retained inventory, so hiding a display or moving a
    // preview cannot change another screen's number on any rendering device.
    let mut screens = layout
        .screens
        .iter()
        .filter(|s| s.id.device == screen.device)
        .collect::<Vec<_>>();
    if screens.len() <= 1 {
        return computer_name;
    }
    screens.sort_by(|a, b| (!a.primary, &a.id.display).cmp(&(!b.primary, &b.id.display)));
    match screens.iter().position(|s| s.id == *screen) {
        Some(index) => format!("{computer_name} {}", index + 1),
        None => computer_name,
    }
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
    let snapshot = preview
        .and_then(|p| p.arrangement.as_deref())
        .cloned()
        .unwrap_or_else(|| arrangement::capture(layout, &applied.owners));
    let Ok((layout, owners)) = arrangement::render(&snapshot, current, &applied.owners) else {
        return View::default();
    };
    let owners = visible_owners(node, &owners);
    let displays = group.local_displays();
    let projections =
        oink_display::projection::compute(&layout.effective(&owners), node.id(), |d| online.contains(d), None)
            .into_iter()
            .filter_map(|geometry| {
                let d = displays
                    .displays
                    .iter()
                    .find(|d| d.key == geometry.host.display && !d.is_virtual)?;
                Some(NamedProjection {
                    name: projection_name(
                        &layout,
                        &geometry.neighbour,
                        node.device_name(geometry.neighbour.device),
                    ),
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

// Only live previews and stream retries need a clock; commands and arrivals wake the owner.
fn next_tick(control: &Control, now: Instant, retry: Option<Instant>) -> Option<Instant> {
    let active = control.mode.is_some()
        || control.local.is_some()
        || !control.remote.is_empty()
        || control.gesture.is_some()
        || !control.gestures.is_empty()
        || control.pending.pending.is_some()
        || !control.immediate.is_empty()
        || control.input_gesture;
    if active {
        Some(retry.map_or(now + INTERVAL, |retry| retry.min(now + INTERVAL)))
    } else {
        retry
    }
}

async fn wait_tick(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

pub(crate) fn start(node: Node, mut port: ModulePort, native: bool) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let links = node.links()?;
    let group = node.link_config()?;
    if native {
        if let Some(surface) = ProjectionOverlay::platform() {
            let _ = node.inner.projection.surface.set(surface);
            tracing::info!("projection native renderer started");
        } else {
            tracing::info!("projection native renderer unavailable");
        }
    }
    let mut streams = ProjectionStreams::new(links.clone(), group.clone());
    let wake = streams.wake();
    let mut changes = group.subscribe();
    let mut stop = node.stop_receiver();
    Ok(tokio::spawn(async move {
        let mut last_heartbeat = Instant::now();
        let mut last_draw = Instant::now() - HEARTBEAT;
        let mut drawn = false;
        let mut send_outcomes = BTreeMap::new();
        let mut online = BTreeSet::from([node.id()]);
        loop {
            let deadline = next_tick(
                &lock(&node.inner.projection.control),
                Instant::now(),
                streams.deadline(),
            );
            tokio::select! {
                _ = stop.changed() => break,
                _ = wait_tick(deadline) => {},
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
                        send_outcomes.remove(&peer);
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
                        .filter_map(|action| encode_for_peer(&node, peer, action).transpose())
                        .collect::<v1::Result<Vec<_>>>();
                    let encoded = encode_for_peer(&node, peer, message);
                    let outcome = match (&opening, &encoded) {
                        (Err(_), _) | (_, Err(_)) => "invalid_local_state",
                        (_, Ok(None)) => "unsupported_preview",
                        _ => "ready",
                    };
                    let (Ok(opening), Ok(Some(message))) = (opening, encoded) else {
                        if send_outcomes.insert(peer, outcome) != Some(outcome) {
                            tracing::info!(?peer, outcome, "projection send refused");
                        }
                        continue;
                    };
                    streams.set_opening(peer, opening);
                    let result = streams.send(peer, &message, now);
                    let outcome = if result.is_ok() { "queued" } else { "failed" };
                    if send_outcomes.insert(peer, outcome) != Some(outcome) {
                        tracing::info!(?peer, ?result, "projection send state");
                    }
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
fn remaining_lease_ms(expires: Instant, now: Instant) -> u32 {
    expires.saturating_duration_since(now).min(LEASE).as_millis() as u32
}

fn current_messages(node: &Node) -> Vec<Action> {
    let c = lock(&node.inner.projection.control);
    let mut messages = Vec::new();
    if let Some((id, _)) = c.mode
        && id.owner == node.id()
    {
        messages.push(Action::Mode { id, enabled: true });
    }
    if let Some(p) = &c.local
        && let remaining = remaining_lease_ms(p.expires, lease_now(node, Instant::now()))
        && remaining > 0
    {
        messages.push(Action::Arrangement {
            sequence: c.sequence,
            lease_ms: remaining,
            expires_ms: preview_wall_now(node) + u64::from(remaining),
            snapshot: p.arrangement.clone(),
        });
    }
    messages
}

fn encode_for_peer(node: &Node, peer: DeviceId, action: &Action) -> v1::Result<Option<v1::Message>> {
    let config = node.link_config().expect("started config");
    let capable = config
        .peer_agreement(peer)
        .is_some_and(|a| a.contains("oink.layout.preview/1"));
    let applied = config.applied();
    encode_selected(action, capable, &applied.layout, &applied.owners)
}
fn encode_selected(
    action: &Action,
    capable: bool,
    live: &Layout,
    owners: &arrangement::Owners,
) -> v1::Result<Option<v1::Message>> {
    match action {
        Action::Arrangement { sequence, snapshot, .. } if !capable => {
            let screens = if let Some(snapshot) = snapshot {
                let current = arrangement::capture(live, owners);
                // Legacy P0 carries only rigid rectangles. Native or visibility
                // changes wait for the ordinary owner/layout publication on commit.
                if snapshot.native.iter().any(|n| {
                    !current
                        .native
                        .iter()
                        .any(|c| crate::layout_profiles::model::same_native(n, c))
                }) || snapshot.disabled.len() != current.disabled.len()
                    || snapshot.disabled.iter().any(|b| !current.disabled.contains(b))
                    || current.placements.iter().any(|p| {
                        crate::layout_profiles::model::screen_shared(snapshot, p.device)
                            != crate::layout_profiles::model::screen_shared(&current, p.device)
                    })
                    || snapshot.topology != current.topology
                {
                    return Ok(None);
                }
                let Ok((layout, _)) = arrangement::render(snapshot, live, owners) else {
                    return Ok(None);
                };
                Some(
                    layout
                        .screens
                        .into_iter()
                        .map(|s| ScreenRect {
                            device: s.id.device,
                            display: s.id.display,
                            rect: s.rect,
                        })
                        .collect(),
                )
            } else {
                None
            };
            wire::encode(&Action::Preview {
                sequence: *sequence,
                screens,
                affected: vec![],
            })
            .map(Some)
        }
        Action::Preview {
            sequence,
            screens: None,
            ..
        } if capable => wire::encode(&Action::Arrangement {
            sequence: *sequence,
            lease_ms: LEASE.as_millis() as u32,
            expires_ms: crate::layout_sync::now_ms() + LEASE.as_millis() as u64,
            snapshot: None,
        })
        .map(Some),
        _ => wire::encode(action).map(Some),
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "isolated resource benchmark; run with --ignored --nocapture"]
    async fn resource_projection_idle_benchmark() {
        let control = Control::default();
        let started = Instant::now();
        let mut ticks = 0;
        let stop = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = wait_tick(next_tick(&control, Instant::now(), None)) => ticks += 1,
            }
        }
        println!(
            "idle_projection_ticks={ticks} elapsed_us={}",
            started.elapsed().as_micros()
        );
        // Keep the original fixed interval as a matched reference, including its first tick.
        let mut original = tokio::time::interval(INTERVAL);
        original.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let stop = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(stop);
        let mut original_ticks = 0;
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = original.tick() => original_ticks += 1,
            }
        }
        println!("original_fixed_interval_ticks={original_ticks}");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_projection_has_no_timer_and_retry_still_wakes() {
        use std::future::Future;
        let now = tokio::time::Instant::now().into_std();
        let control = Control::default();
        let mut idle = Box::pin(wait_tick(next_tick(&control, now, None)));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(idle.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(idle.as_mut().poll(&mut cx).is_pending());
        let retry = now + Duration::from_secs(3601);
        assert_eq!(next_tick(&control, now, Some(retry)), Some(retry));
        wait_tick(Some(retry)).await;
        assert_eq!(tokio::time::Instant::now().into_std(), retry);
    }

    #[test]
    fn projection_keeps_clock_for_pending_clear_and_gesture_expiry() {
        let now = Instant::now();
        let mut control = Control::default();
        control.pending.push(Action::Preview {
            screens: None,
            sequence: 1,
            affected: vec![],
        });
        assert_eq!(next_tick(&control, now, None), Some(now + INTERVAL));
        control.pending.barrier();
        control.gestures.insert(DeviceId([2; 32]), now + LEASE);
        assert_eq!(next_tick(&control, now, None), Some(now + INTERVAL));
        control.gestures.clear();
        assert_eq!(next_tick(&control, now, None), None);
    }

    #[test]
    fn abandoned_previews_expire_without_renewing_the_ui_lease() {
        let now = Instant::now();
        let mut c = Control {
            local: Some(Preview {
                layout: Layout::default(),
                arrangement: None,
                affected: vec![],
                expires: now + LEASE,
            }),
            ..Control::default()
        };
        c.remote.insert(
            DeviceId([2; 32]),
            Preview {
                layout: Layout::default(),
                arrangement: None,
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

#[cfg(test)]
mod label_tests {
    use super::*;
    use oink_display::layout::{ScreenPlacement, SizeSource};
    use oink_foundation::Rect;

    fn inventory(keys: &[(&str, bool)]) -> Layout {
        Layout {
            screens: keys
                .iter()
                .enumerate()
                .map(|(i, (key, primary))| ScreenPlacement {
                    id: ScreenId::new(DeviceId([1; 32]), *key),
                    name: key.to_string(),
                    rect: Rect::new(i as f64, 0., 1., 1.),
                    local: Rect::new(i as f64 * 100., 0., 100., 100.),
                    primary: *primary,
                    builtin: false,
                    size_source: SizeSource::Reported,
                })
                .collect(),
            ..Default::default()
        }
    }
    fn label(l: &Layout, key: &str) -> String {
        projection_name(l, &ScreenId::new(DeviceId([1; 32]), key), "Computer".into())
    }
    #[test]
    fn primary_first_identity_order_is_stable_across_preview_and_inventory_order() {
        let mut l = inventory(&[("z-primary", true), ("b", false), ("a", false)]);
        for _ in 0..3 {
            assert_eq!(label(&l, "z-primary"), "Computer 1");
            assert_eq!(label(&l, "a"), "Computer 2");
            assert_eq!(label(&l, "b"), "Computer 3");
            l.screens.rotate_left(1);
            for s in &mut l.screens {
                s.rect.x = -s.rect.x - 1.;
            }
        }
    }
    #[test]
    fn a_single_screen_has_no_suffix() {
        assert_eq!(label(&inventory(&[("only", true)]), "only"), "Computer");
    }
    #[test]
    fn absent_or_duplicate_primary_flags_have_a_stable_identity_tie_break() {
        for primary in [false, true] {
            let l = inventory(&[("b", primary), ("a", primary)]);
            assert_eq!(label(&l, "a"), "Computer 1");
            assert_eq!(label(&l, "b"), "Computer 2");
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod integration_tests {
    use super::*;
    use crate::api::AgentApi;
    use crate::snapshot_tests::{Displays, common, start_with_clip};
    use oink_protocol::v1::schema::WireDto;

    #[test]
    fn same_machine_projections_have_shared_numbers_and_remain_informational() {
        crate::test_watchdog::run(module_path!(), "same-machine projection labels", Some(10), async {
            use oink_foundation::{Rect, Size};
            use std::sync::Arc;
            let ad = tempfile::tempdir().unwrap();
            let bd = tempfile::tempdir().unwrap();
            let mut primary = Displays::new("z-primary").info();
            primary.bounds = Rect::new(0., 0., 100., 100.);
            primary.reference_extent = Some(Size::new(1., 1.));
            let mut second = primary.clone();
            second.key = "a-second".into();
            second.bounds.x = 100.;
            second.is_primary = false;
            let a = Node::start(crate::node::NodeOptions {
                net: oink_test_support::network_config(ad.path().into(), "labels-a", 0),
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: vec![second, primary],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: false,
                cursor_trail: false,
            })
            .await
            .unwrap();
            let b = start_with_clip(
                &oink_test_support::network_config(bd.path().into(), "labels-b", 0),
                &Displays::new("single"),
                None,
            )
            .await;
            common::pair(&a, &b).await;
            common::until("shared numbered inventory", || {
                a.layout().0 == b.layout().0 && a.layout().0.screens.len() == 3
            })
            .await;
            set_mode(&a, true);
            set_mode(&b, true);
            let online = BTreeSet::from([a.id(), b.id()]);
            let av = view(&a, &online);
            let bv = view(&b, &online);
            let remote = bv
                .projections
                .iter()
                .find(|p| p.geometry.neighbour.device == a.id())
                .unwrap();
            let number = if remote.geometry.neighbour.display == "z-primary" {
                1
            } else {
                2
            };
            assert_eq!(remote.name, format!("{} {number}", b.device_name(a.id())));
            for (key, number) in [("z-primary", 1), ("a-second", 2)] {
                let local = av
                    .projections
                    .iter()
                    .find(|p| p.geometry.neighbour.device == a.id() && p.geometry.neighbour.display == key)
                    .unwrap();
                assert_eq!(local.name, format!("{} {number}", a.device_name(a.id())));
                assert!(local.geometry.handles().is_empty());
            }
            let single = av
                .projections
                .iter()
                .find(|p| p.geometry.neighbour.device == b.id())
                .unwrap();
            assert_eq!(single.name, a.device_name(b.id()));
            let stored = a.layout().0;
            a.set_local_paused(crate::device_controls::runtime::fresh_request_id(), true)
                .unwrap();
            common::until("paused local inventory has no annotations", || {
                view(&a, &online).projections.is_empty()
            })
            .await;
            common::until("paused peer inventory has no annotations", || {
                view(&b, &online).projections.is_empty()
            })
            .await;
            assert_eq!(a.layout().0, stored);
            a.set_local_paused(crate::device_controls::runtime::fresh_request_id(), false)
                .unwrap();
            common::until("resumed local annotations retain their numbers", || {
                view(&a, &online).projections.iter().any(|p| {
                    p.geometry.neighbour.display == "a-second" && p.name == format!("{} 2", a.device_name(a.id()))
                })
            })
            .await;
            assert_eq!(a.layout().0, stored);
            a.shutdown().await;
            b.shutdown().await;
        });
    }

    #[test]
    fn paused_screens_vanish_from_native_projections_and_resume_in_place() {
        crate::test_watchdog::run(module_path!(), "paused projection visibility", Some(10), async {
            let ad = tempfile::tempdir().unwrap();
            let bd = tempfile::tempdir().unwrap();
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
            common::until("same projection layout", || {
                a.layout().0 == b.layout().0 && a.layout().0.screens.len() == 2
            })
            .await;
            set_mode(&a, true);
            let online = BTreeSet::from([a.id(), b.id()]);
            let stored = a.layout().0;
            assert!(!view(&a, &online).projections.is_empty());
            for paused in [true, false] {
                b.set_local_paused(crate::device_controls::runtime::fresh_request_id(), paused)
                    .unwrap();
                common::until("pause facts reach projection owner", || {
                    a.link_config()
                        .unwrap()
                        .members()
                        .members
                        .iter()
                        .find(|m| m.device == b.id())
                        .and_then(|m| m.facts.as_ref())
                        .is_some_and(|f| f.pause().ok().flatten().is_some_and(|p| p.paused() == Ok(paused)))
                })
                .await;
                assert_eq!(view(&a, &online).projections.is_empty(), paused);
                assert_eq!(a.layout().0, stored, "pause must never rewrite stored positions");
            }
            a.shutdown().await;
            b.shutdown().await;
        });
    }

    #[test]
    fn general_preview_native_group_visibility_and_shared_dto_are_ephemeral() {
        general_preview_scenario(false);
    }

    #[test]
    fn general_preview_waits_for_catalog_handoff_before_committing_native_observation() {
        general_preview_scenario(true);
    }

    fn general_preview_scenario(delay_catalog: bool) {
        crate::test_watchdog::run(module_path!(), "general arrangement preview", Some(6), async move {
            use oink_foundation::{Point, Rect, Size};
            use std::sync::Arc;
            let ad = tempfile::tempdir().unwrap();
            let bd = tempfile::tempdir().unwrap();
            let mut main = Displays::new("main").info();
            main.bounds = Rect::new(0., 0., 100., 100.);
            main.reference_extent = Some(Size::new(1., 1.));
            let mut second = main.clone();
            second.key = "second".into();
            second.bounds.x = 100.;
            second.is_primary = false;
            let a = Node::start(crate::node::NodeOptions {
                net: oink_test_support::network_config(ad.path().into(), "general-a", 0),
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: vec![main, second],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: false,
                cursor_trail: false,
            })
            .await
            .unwrap();
            let b = start_with_clip(
                &oink_test_support::network_config(bd.path().into(), "general-b", 0),
                &Displays::new("b"),
                None,
            )
            .await;
            // This scenario checks catalog/commit ordering, not lease expiry.
            // Disk writes and cross-thread scheduling can outlive the real 2s
            // lease under load. Freeze only lease time; stream pacing, catalog
            // handoff and the native observation still run asynchronously.
            for node in [&a, &b] {
                node.inner
                    .projection
                    .lease_clock
                    .set(Mutex::new(Instant::now()))
                    .unwrap();
            }
            let source_default = crate::layout_profiles::model::default_id(a.group_id().unwrap().into());
            a.inner
                .profiles
                .defer_catalogs
                .store(delay_catalog, std::sync::atomic::Ordering::SeqCst);
            common::pair(&a, &b).await;
            common::until("three retained screens", || {
                a.layout().0.screens.len() == 3 && a.layout().0 == b.layout().0
            })
            .await;
            let nodes = [&a, &b];
            let mut handoff = std::pin::pin!(common::profile_handoff(&nodes, &source_default));
            if delay_catalog {
                let observed =
                    std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(handoff.as_mut(), cx)))
                        .await;
                assert!(observed.is_pending(), "screen geometry alone cannot admit a saved edit");
                a.inner
                    .profiles
                    .defer_catalogs
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            handoff.await;
            let config = a.link_config().unwrap();
            assert!(config.peer_agreement(b.id()).unwrap().contains("oink.layout.preview/1"));
            let live = a.layout().0;
            let owners = config.applied().owners.clone();
            let mut snapshot = arrangement::capture(&live, &owners);
            for p in &mut snapshot.placements {
                p.origin = Point::new(if p.device.0 == a.id() { 0.25 } else { 1.25 }, 0.);
                p.scale = if p.device.0 == a.id() { 0.01 } else { 1.0 / 1728.0 };
            }
            // Every capable peer uses the shared DTO even for a whole-group rigid drag.
            let action = |snapshot: ArrangementSnapshot, sequence| Action::Arrangement {
                sequence,
                lease_ms: 2000,
                expires_ms: crate::layout_sync::now_ms() + 2000,
                snapshot: Some(Box::new(snapshot)),
            };
            let rigid = action(snapshot.clone(), 1);
            let modern = encode_selected(&rigid, true, &live, &owners).unwrap().unwrap();
            assert!(matches!(modern, v1::Message::ArrangementPreview(_)));
            let legacy = encode_selected(&rigid, false, &live, &owners).unwrap().unwrap();
            assert!(matches!(legacy, v1::Message::ProjectionPreview(_)));
            if let Action::Preview {
                screens: Some(screens), ..
            } = wire::decode(legacy).unwrap()
            {
                assert_eq!(screens.len(), 3);
                assert!(
                    screens
                        .iter()
                        .any(|s| s.device == a.id() && s.display == "main" && s.rect.x == 0.25)
                );
            } else {
                panic!("rigid legacy fallback missing")
            }
            let n = snapshot.native.iter_mut().find(|n| n.device.0 == a.id()).unwrap();
            n.screens
                .iter_mut()
                .find(|s| s.screen.display == "second")
                .unwrap()
                .origin = Point::new(0., 100.);
            let native = action(snapshot.clone(), 2);
            assert!(encode_selected(&native, false, &live, &owners).unwrap().is_none());
            let message = encode_selected(&native, true, &live, &owners).unwrap().unwrap();
            if let v1::Message::ArrangementPreview(m) = &message {
                assert_eq!(
                    m.arrangement().unwrap().unwrap().raw(),
                    &match crate::layout_profiles::wire::snapshot(&snapshot).unwrap() {
                        v1::RawValue::Map(m) => m,
                        _ => unreachable!(),
                    }
                );
            }
            let bytes = v1::frame::encode(&message, v1::limits::RELIABLE_FRAME).unwrap();
            assert_eq!(
                wire::decode(
                    v1::frame::OpaqueFrame::parse_exact(&bytes, v1::limits::RELIABLE_FRAME)
                        .unwrap()
                        .decode()
                        .unwrap()
                )
                .unwrap(),
                native
            );
            let versions = [a.layout().1.revision, b.layout().1.revision];
            let paths = [ad.path(), bd.path()]
                .into_iter()
                .map(|d| d.join(crate::config_module::group_store::STATE_FILE))
                .collect::<Vec<_>>();
            let saved = paths.iter().map(|p| std::fs::read(p).unwrap()).collect::<Vec<_>>();
            preview_ui_arrangement(
                &a,
                1,
                snapshot.placements.clone(),
                vec![],
                false,
                Some(Box::new(snapshot.clone())),
            )
            .unwrap();
            common::until("native preview arrives", || {
                lock(&b.inner.projection.control)
                    .remote
                    .get(&a.id())
                    .is_some_and(|p| p.layout.screen(&ScreenId::new(a.id(), "second")).unwrap().local.y == 100.)
            })
            .await;
            let online = BTreeSet::from([a.id(), b.id()]);
            assert!(!view(&a, &online).projections.is_empty());
            assert!(!view(&b, &online).projections.is_empty());
            // The same render adapter applies hidden/device-disabled flags.
            let mut hidden = snapshot.clone();
            crate::layout_profiles::model::set_screen_shared(&mut hidden, b.id().into(), false);
            receive(&b, a.id(), action(hidden, 1000), Instant::now());
            assert!(view(&b, &online).projections.is_empty());
            receive(
                &b,
                a.id(),
                Action::Arrangement {
                    sequence: 1001,
                    lease_ms: 0,
                    expires_ms: crate::layout_sync::now_ms() + 2000,
                    snapshot: Some(Box::new(snapshot.clone())),
                },
                Instant::now(),
            );
            assert!(
                view(&b, &online).projections.is_empty(),
                "zero remaining lease cannot replace the preview"
            );
            preview_ui(&a, 1, vec![], vec![], true).unwrap();
            // Directly fence the artificial high sequence used above.
            receive(
                &b,
                a.id(),
                Action::Arrangement {
                    sequence: 1002,
                    lease_ms: 2000,
                    expires_ms: crate::layout_sync::now_ms() + 2000,
                    snapshot: None,
                },
                Instant::now(),
            );
            assert_eq!(view(&b, &online), View::default());
            receive(&b, a.id(), native, Instant::now());
            assert_eq!(
                view(&b, &online),
                View::default(),
                "late frame cannot revive a closed lease"
            );
            assert_eq!([a.layout().1.revision, b.layout().1.revision], versions);
            assert_eq!(a.layout().0, live);
            assert_eq!(b.layout().0, live);
            assert_eq!(
                paths.iter().map(|p| std::fs::read(p).unwrap()).collect::<Vec<_>>(),
                saved
            );
            // Release does not clear before the commit arrives. While delivery
            // is delayed the receiver still renders the last preview, never its
            // old durable arrangement. Simulate the owner's observed OS result.
            receive(&b, a.id(), action(snapshot.clone(), 1003), Instant::now());
            let released = view(&b, &online);
            assert!(!released.projections.is_empty());
            assert_eq!(b.layout().0, live);
            let mut observed_displays = a.link_config().unwrap().local_displays().displays.clone();
            observed_displays.iter_mut().find(|s| s.key == "second").unwrap().bounds = Rect::new(0., 100., 100., 100.);
            let author = a.clone();
            let placements = snapshot.placements.clone();
            let commit = tokio::task::spawn_blocking(move || {
                author
                    .link_config()
                    .unwrap()
                    .initialize_displays(observed_displays)
                    .unwrap();
                let result = author.save_layout(placements);
                let catalog = lock(&author.inner.profiles.data)
                    .catalog
                    .as_ref()
                    .map(|c| (c.group, c.version.clone()));
                assert!(
                    result.is_ok(),
                    "save after native observation: {result:?}; group={:?}; catalog={catalog:?}; profile_state={:?}",
                    author.group_id(),
                    author.layout_profile_command(crate::layout_profiles::types::Command::Query),
                );
            });
            while !commit.is_finished() {
                assert_eq!(view(&b, &online), released);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            commit.await.unwrap();
            common::until("committed arrangement reaches receiver without a flash", || {
                assert_eq!(view(&b, &online), released);
                b.layout().0 == a.layout().0 && b.layout().1.revision > versions[1]
            })
            .await;
            receive(
                &b,
                a.id(),
                Action::Arrangement {
                    sequence: 1004,
                    lease_ms: 2000,
                    expires_ms: crate::layout_sync::now_ms() + 2000,
                    snapshot: None,
                },
                Instant::now(),
            );
            assert_eq!(view(&b, &online), View::default());
            assert_eq!(
                b.layout().0.screen(&ScreenId::new(a.id(), "second")).unwrap().local.y,
                100.
            );
            a.shutdown().await;
            b.shutdown().await;
        });
    }

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
            for node in &nodes {
                node.inner
                    .projection
                    .lease_clock
                    .set(Mutex::new(Instant::now()))
                    .unwrap();
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
            // A Windows-like pixel desktop must project on both receivers even
            // when its wall clock leads (observed regression) or trails theirs.
            for (gesture, offset) in [(2, 1000), (3, -60_000)] {
                upper
                    .inner
                    .projection
                    .wall_offset_ms
                    .store(offset, std::sync::atomic::Ordering::Relaxed);
                preview_ui(upper, gesture, placements.clone(), vec![], false).unwrap();
                common::until("clock-skewed preview reaches both receivers", || {
                    [left, right]
                        .iter()
                        .all(|node| view(node, &online).projections.len() == 2)
                })
                .await;
                preview_ui(upper, gesture, vec![], vec![], true).unwrap();
                common::until("clock-skewed preview end hides all screens", || {
                    nodes.iter().all(|node| view(node, &online) == View::default())
                })
                .await;
            }
            preview_ui(upper, 4, placements, vec![], false).unwrap();
            let expires = lock(&upper.inner.projection.control).local.as_ref().unwrap().expires;
            *lock(upper.inner.projection.lease_clock.get().unwrap()) = expires - Duration::from_millis(1);
            let messages = tick(upper, Instant::now() + INTERVAL, true);
            assert!(
                messages
                    .iter()
                    .any(|m| matches!(m, Action::Arrangement { lease_ms: 1, .. }))
            );
            assert!(
                current_messages(upper)
                    .iter()
                    .any(|m| matches!(m, Action::Arrangement { lease_ms: 1, .. }))
            );
            assert!(!view(upper, &online).projections.is_empty());
            *lock(upper.inner.projection.lease_clock.get().unwrap()) = expires;
            tick(upper, Instant::now(), false);
            assert_eq!(
                view(upper, &online),
                View::default(),
                "heartbeats cannot keep an abandoned UI preview visible"
            );
            assert!(
                current_messages(upper).is_empty(),
                "expired preview cannot seed a reopened stream"
            );
            // Receiver TTL is monotonic and duplicates cannot renew it.
            let snapshot = arrangement::capture(&upper.layout().0, &upper.link_config().unwrap().applied().owners);
            let frame = Action::Arrangement {
                sequence: 10000,
                lease_ms: 100,
                expires_ms: 1,
                snapshot: Some(Box::new(snapshot)),
            };
            receive(left, upper.id(), frame.clone(), Instant::now());
            let expiry = lock(&left.inner.projection.control).remote[&upper.id()].expires;
            *lock(left.inner.projection.lease_clock.get().unwrap()) = expiry;
            tick(left, Instant::now(), false);
            receive(left, upper.id(), frame, Instant::now());
            assert!(!lock(&left.inner.projection.control).remote.contains_key(&upper.id()));
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
        for node in [&a, &b] {
            node.inner
                .projection
                .lease_clock
                .set(Mutex::new(Instant::now()))
                .unwrap();
        }
        common::pair(&a, &b).await;
        common::until("same layout", || {
            a.layout().0 == b.layout().0 && a.layout().0.screens.len() == 2
        })
        .await;
        // Role baselines are persisted independently after membership/layout
        // convergence. Let that setup finish before testing preview durability.
        common::until("owner role baselines settled", || {
            [&a, &b].into_iter().all(|node| {
                let config = lock(&node.inner.config);
                let claims = config.owner().claims(node.group_id().unwrap());
                let microphone = config.owner().microphone_claims(node.group_id().unwrap());
                [a.id(), b.id()].into_iter().all(|id| {
                    claims.records.iter().any(|r| r.owner == id) && microphone.records.iter().any(|r| r.owner == id)
                })
            })
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
