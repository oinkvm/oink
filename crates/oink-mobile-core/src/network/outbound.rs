//! Streams this phone writes (B0, M0, D0, I0): one [`StreamMachine`] per link
//! and kind, the same rules a desktop follows (`oink_network::stream_machine`).
//!
//! A peer that closes or refuses one of them is noticed at once: `Cancelled`
//! (an intentional retirement, e.g. a desktop pausing or resuming input)
//! reopens immediately with the next ordinal and the opening is sent again;
//! `Paused` (the desktop paused sharing of that kind) does not reopen until
//! the peer shows it resumed (new facts, or a stream it opens to us), and
//! fails what was waiting on the stream with that reason; other closes reopen
//! with the machine's backoff. The link itself is never dropped for it.
//! A B0 or I0 that lived through the peer's paused Facts is replaced when its
//! Facts show it resumed, so a late `Paused` refusal cannot park it
//! (`StreamMachine::peer_facts`).
use super::*;
use oink_network::{
    link::{StreamClosed, StreamDir},
    stream_machine::{Failure, State as MachineState, Step, StreamMachine},
};
use oink_protocol::frame::StreamCode;

#[derive(Debug, Default)]
pub(super) struct Outbound(Mutex<HashMap<(LinkId, StreamKind), (DeviceId, StreamMachine)>>);

impl Outbound {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(LinkId, StreamKind), (DeviceId, StreamMachine)>> {
        self.0.lock().expect("outbound stream mutex poisoned")
    }
}

/// The kinds this phone writes on a ready link: those of its process kind
/// (protocol-v2 1.1) that this link negotiated. The send gate would refuse the
/// others (`OpenError::Unnegotiated`); they get no machine at all.
fn kinds(state: &NetworkState, link: LinkId) -> Vec<StreamKind> {
    let mut kinds = match state.process {
        MobileProcess::Broadcast => vec![StreamKind::Audio],
        _ => vec![StreamKind::Clipboard],
    };
    if state.process == MobileProcess::App {
        kinds.extend([StreamKind::Audio, StreamKind::Diagnostics, StreamKind::Input]);
    }
    let table = state.links.links();
    let Some(entry) = table.links.iter().find(|entry| entry.link == link) else {
        return Vec::new();
    };
    kinds.retain(|kind| entry.opens(*kind));
    kinds
}

/// A link became ready: a fresh machine per kind, each opening its first stream.
pub(super) fn link_ready(state: &Arc<NetworkState>, link: LinkId, peer: DeviceId) {
    let kinds = kinds(state, link);
    {
        let mut machines = state.outbound.lock();
        for kind in &kinds {
            machines.insert((link, *kind), (peer, StreamMachine::new(*kind)));
        }
    }
    for kind in kinds {
        poll(state, link, kind);
    }
}

/// The link is gone with all its streams.
pub(super) fn link_lost(state: &NetworkState, link: LinkId) {
    state
        .outbound
        .lock()
        .retain(|(machine_link, _), _| *machine_link != link);
}

/// Opens the kind's stream when its machine wants one now; arms the backoff
/// timer otherwise.
pub(super) fn poll(state: &Arc<NetworkState>, link: LinkId, kind: StreamKind) {
    let (peer, step, retry) = {
        let mut machines = state.outbound.lock();
        let Some((peer, machine)) = machines.get_mut(&(link, kind)) else {
            return;
        };
        let step = machine.poll(true, true, Instant::now());
        let retry = (machine.state() == MachineState::Backoff)
            .then(|| machine.deadline())
            .flatten();
        (*peer, step, retry)
    };
    run(state, link, peer, step);
    if let Some(at) = retry {
        let weak = Arc::downgrade(state);
        state.runtime.spawn(async move {
            tokio::time::sleep_until(at.into()).await;
            if let Some(state) = weak.upgrade() {
                poll(&state, link, kind);
            }
        });
    }
}

/// A stream of a registered kind closed. Outbound: the machine decides when
/// to reopen. Inbound: the business owner of that kind is told.
pub(super) fn closed(state: &Arc<NetworkState>, stream: StreamId, reason: StreamClosed) {
    let StreamClosed::Peer(code) = reason else {
        // Our own close, or the link went away (`link_lost`).
        return;
    };
    let kind = stream.tag.kind;
    let Some(peer) = state
        .links
        .links()
        .links
        .iter()
        .find(|entry| entry.link == stream.link)
        .map(|entry| entry.peer)
    else {
        return;
    };
    if stream.dir == StreamDir::In {
        match kind {
            // The desktop stopped sending input (paused, or retired its session):
            // nothing it pressed may stay down here.
            StreamKind::Input => {
                if let Some(sink) = state.input_sink.get() {
                    sink.input_closed(peer);
                }
            }
            // A paused desktop closes its B0 too: no answer can come over it.
            StreamKind::Clipboard if code == StreamCode::Paused => clipboard_paused(state, peer),
            _ => {}
        }
        return;
    }
    let step = {
        let mut machines = state.outbound.lock();
        let Some((_, machine)) = machines.get_mut(&(stream.link, kind)) else {
            return;
        };
        if machine.current() != Some(stream.tag) {
            return;
        }
        machine.failed(Failure::Peer(code), Instant::now())
    };
    run(state, stream.link, peer, step);
    if kind == StreamKind::Clipboard && code == StreamCode::Paused {
        clipboard_paused(state, peer);
    }
    poll(state, stream.link, kind);
}

/// The peer's Facts changed; `paused` is its own pause in them.
pub(super) fn peer_facts(state: &Arc<NetworkState>, link: LinkId, paused: bool) {
    // A B0 or I0 the peer could see while paused may already be refused as
    // `Paused`; with no opening timeout on the phone, a late refusal would
    // park it until some later Facts. The machine replaces such a stream now.
    let now = Instant::now();
    let steps: Vec<(DeviceId, Step)> = state
        .outbound
        .lock()
        .iter_mut()
        .filter(|((machine_link, _), _)| *machine_link == link)
        .map(|(_, (peer, machine))| (*peer, machine.peer_facts(paused, now)))
        .collect();
    for (peer, step) in steps {
        run(state, link, peer, step);
    }
    // New facts may say the peer resumed what it paused.
    peer_resumed(state, link);
}

/// The peer may have resumed sharing (new facts, or it opened a stream to
/// us): every stream it refused as `Paused` is opened again.
pub(super) fn peer_resumed(state: &Arc<NetworkState>, link: LinkId) {
    let resumed: Vec<StreamKind> = state
        .outbound
        .lock()
        .iter_mut()
        .filter(|((machine_link, _), (_, machine))| *machine_link == link && machine.peer_paused())
        .filter_map(|((_, kind), (_, machine))| machine.peer_resumed().then_some(*kind))
        .collect();
    for kind in resumed {
        poll(state, link, kind);
    }
}

/// Replaces the kind's stream on every link (its opening changed), without
/// counting it as a failure.
pub(super) fn reopen(state: &Arc<NetworkState>, kind: StreamKind) {
    for (link, peer, step) in reopen_steps(state, kind) {
        run(state, link, peer, step);
    }
}

fn reopen_steps(state: &NetworkState, kind: StreamKind) -> Vec<(LinkId, DeviceId, Step)> {
    state
        .outbound
        .lock()
        .iter_mut()
        .filter(|((_, machine_kind), _)| *machine_kind == kind)
        .map(|((link, _), (peer, machine))| (*link, *peer, machine.reopen(Instant::now())))
        .collect()
}

/// [`reopen`] decided now and carried out when the returned function runs,
/// as on another thread after a later decision.
#[cfg(test)]
pub(super) fn reopen_decided(state: &Arc<NetworkState>, kind: StreamKind) -> impl FnOnce() + use<> {
    let steps = reopen_steps(state, kind);
    let state = state.clone();
    move || {
        for (link, peer, step) in steps {
            run(&state, link, peer, step);
        }
    }
}

/// The peer refused or closed this kind because it paused sharing.
pub(super) fn paused(state: &NetworkState, peer: DeviceId, kind: StreamKind) -> bool {
    state
        .outbound
        .lock()
        .iter()
        .any(|((_, machine_kind), (machine_peer, machine))| {
            *machine_kind == kind && *machine_peer == peer && machine.peer_paused()
        })
}

fn clipboard_paused(state: &NetworkState, peer: DeviceId) {
    if let Some(transfer) = clipboard_owner(state) {
        transfer.peer_paused(&hex(peer));
    }
}

fn run(state: &Arc<NetworkState>, link: LinkId, peer: DeviceId, step: Step) {
    match step {
        Step::Nothing => {}
        Step::Close(tag) => close(state, link, peer, tag),
        Step::Open(tag) => open(state, link, peer, tag),
        Step::Reopen { close: old, open: new } => {
            close(state, link, peer, old);
            open(state, link, peer, new);
        }
    }
}

fn close(state: &NetworkState, link: LinkId, peer: DeviceId, tag: StreamTag) {
    let stream = StreamId {
        link,
        dir: StreamDir::Out,
        tag,
    };
    // Closed through the handle as it is dropped: a `close_stream` command
    // sent after the drop arrives too late, the network owner already closed
    // the dropped queue with `None` (the peer would treat it as a failure).
    state
        .senders
        .lock()
        .expect("mobile sender mutex poisoned")
        .retain(|(sender_peer, kind), sender| {
            let retired = *sender_peer == peer && *kind == tag.kind && sender.id() == stream;
            if retired {
                sender.close_with(StreamCode::Cancelled);
            }
            !retired
        });
    state.audio_tx.lock().expect("audio sender mutex").remove(&stream);
    if tag.kind == StreamKind::Input {
        state
            .input_motion
            .lock()
            .expect("input motion mutex poisoned")
            .retain(|motion_peer, motion| *motion_peer != peer || motion.follows() != stream);
    }
    // Already closed by the peer: nothing happens.
    state.links.close_stream(stream, StreamCode::Cancelled);
}

fn open(state: &Arc<NetworkState>, link: LinkId, peer: DeviceId, tag: StreamTag) {
    let kind = tag.kind;
    let Ok((sender, datagram)) = state.links.open_stream(
        link,
        kind.priority(),
        tag,
        QueueSpec::reliable(kind).0,
        // M0 carries media and every v2 I0 its relative-motion flow; a PC
        // refuses an I0 without it.
        matches!(kind, StreamKind::Audio | StreamKind::Input).then(|| QueueSpec::datagram(kind)),
    ) else {
        // The link is going away (`link_lost` follows), or it did not
        // negotiate this kind: the machine stays closed.
        if let Some((_, machine)) = state.outbound.lock().get_mut(&(link, kind))
            && machine.current() == Some(tag)
        {
            machine.poll(false, true, Instant::now());
        }
        return;
    };
    // Link events, backoff timers and host calls decide their steps in order
    // under the machines lock but carry them out after it, on their own
    // threads. Install the stream only while it is still the machine's
    // current one, under that lock: a stale open carried out after a newer
    // one would replace the newer stream's sender (dropping, so closing, it)
    // and leave the current stream without one.
    let machines = state.outbound.lock();
    if machines.get(&(link, kind)).and_then(|(_, machine)| machine.current()) != Some(tag) {
        drop(machines);
        sender.close_with(StreamCode::Cancelled);
        return;
    }
    if kind == StreamKind::Input
        && let Some(datagram) = datagram
    {
        state
            .input_motion
            .lock()
            .expect("input motion mutex poisoned")
            .insert(peer, datagram);
    } else if kind == StreamKind::Audio {
        if let Some(datagram) = datagram {
            state
                .audio_tx
                .lock()
                .expect("audio sender mutex")
                .insert(sender.id(), datagram);
        }
        let wake = Arc::downgrade(state);
        sender.set_wake(Arc::new(move || {
            if let Some(state) = wake.upgrade() {
                state.audio_changed.notify_one();
            }
        }));
    }
    state
        .senders
        .lock()
        .expect("mobile sender mutex poisoned")
        .insert((peer, kind), sender);
    drop(machines);
    match kind {
        StreamKind::Audio => state.audio_changed.notify_one(),
        StreamKind::Input => input_stream_opened(state, link, peer),
        _ => {}
    }
}

/// A new I0 of ours is open to `peer`: its opening records go out on it
/// first (`InputCapability.controllable` and the provider's records, see
/// `input_opening`), then the input owner learns of the new stream.
fn input_stream_opened(state: &Arc<NetworkState>, link: LinkId, peer: DeviceId) {
    if !super::input_opening::post_opening(state, peer) {
        // The stream is unusable already; its close arrives as a machine
        // failure and the next stream carries the opening.
        tracing::debug!(?link, peer = %peer, "I0 opening was not queued");
    }
    if let Some(sink) = state.input_sink.get() {
        sink.links_changed();
    }
}
