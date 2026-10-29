//! Local observations and authenticated source leases; received state is never a source.
use super::*;
use oink_foundation::{GroupId, session::Boot};
use oink_input::power::State;
use oink_protocol::v1::{
    Message, dto,
    schema::{WireDto, WireValue},
};
use std::collections::HashMap;
const CAPABILITY: &str = "oink.power/1";
const HEARTBEAT: Duration = Duration::from_secs(5);
const LEASE: Duration = Duration::from_secs(15);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub available: bool,
    pub state: State,
}
impl Snapshot {
    fn message(self) -> Message {
        Message::ControlPowerState(
            dto::ControlPowerState::from_raw(
                [
                    (1, self.available.write()),
                    (2, self.state.display.write()),
                    (3, self.state.system.write()),
                    (4, self.state.activity.write()),
                ]
                .into(),
            )
            .expect("typed power snapshot"),
        )
    }
}
#[derive(Clone, Copy)]
struct Lease {
    boot: Boot,
    received: Instant,
    state: State,
}
#[derive(Default)]
pub(super) struct Power {
    group: Option<GroupId>,
    remote: HashMap<DeviceId, Lease>,
    sent: HashMap<DeviceId, (Boot, Instant, Snapshot)>,
    local: Snapshot,
    #[cfg(test)]
    fake_source: Snapshot,
    observed: Option<Instant>,
    suspended: bool,
    // Queued pre-suspend observations cannot renew leases after resume.
    accept_after: Option<Instant>,
    applied: State,
}
impl Power {
    fn aggregate(&mut self, now: Instant, mut boot: impl FnMut(DeviceId) -> Option<Boot>) -> State {
        self.remote.retain(|peer, lease| {
            now.saturating_duration_since(lease.received) < LEASE && boot(*peer) == Some(lease.boot)
        });
        self.remote.values().fold(State::default(), |s, l| s.union(l.state))
    }
    fn reset(&mut self, group: GroupId, suspended: bool, now: Instant) {
        if self.group != Some(group) || self.suspended != suspended {
            self.remote.clear();
            self.sent.clear();
            self.observed = None;
            self.accept_after = Some(now);
            self.group = Some(group);
            self.suspended = suspended;
        }
    }
    fn receive(&mut self, boot: Boot, state: State, received: Instant) {
        if !self.suspended && self.accept_after.is_none_or(|at| received >= at) {
            self.remote.insert(boot.device_id, Lease { boot, received, state });
        }
    }
}
fn policy(mut state: State, suspended: bool, locked: bool, wake: bool) -> State {
    if suspended {
        return State::default();
    }
    if locked || !wake {
        state.display = false;
        state.activity = false;
    }
    state
}
impl Node {
    pub(crate) fn receive_power(
        &self,
        group_id: GroupId,
        boot: Boot,
        received: Instant,
        value: dto::ControlPowerState,
    ) {
        let Some(group) = self.inner.group.get() else { return };
        let applied = group.applied();
        if applied.group != group_id
            || applied.paused
            || !applied.members.contains(&boot.device_id)
            || group.peer_boot(boot.device_id) != Some(boot)
            || !group
                .peer_agreement(boot.device_id)
                .is_some_and(|a| a.contains(CAPABILITY))
            || self.inner.input_environment.load().system_sleeping
        {
            return;
        }
        let (Ok(available), Ok(keep_display), Ok(system), Ok(activity)) =
            (value.available(), value.display(), value.system(), value.activity())
        else {
            return;
        };
        let state = if available {
            State {
                display: keep_display,
                system,
                activity,
            }
        } else {
            State::default()
        };
        let mut power = lock(&self.inner.power);
        // A group change may arrive before the ordinary observer's next tick.
        if power.group != Some(group_id) {
            power.reset(group_id, false, received);
        }
        if power.remote.get(&boot.device_id).is_none_or(|old| old.state != state) {
            // These are the peer's sleep-prevention holds (display/system kept
            // awake by an app, user activity), not observed sleep states.
            tracing::info!(
                keep_display_awake = keep_display,
                keep_system_awake = system,
                user_activity = activity,
                available,
                "peer power state changed"
            );
        }
        power.receive(boot, state, received);
    }
    pub(super) fn power_tick(
        &self,
        now: Instant,
        sleeping: bool,
        locked: bool,
        holder: &mut oink_input::power::Hold,
        last_activity: &mut Option<Instant>,
        last_error: &mut Option<String>,
    ) {
        let Some(group) = self.inner.group.get() else { return };
        let applied = group.applied();
        let suspended = applied.paused || sleeping;
        let sample_due = {
            let mut power = lock(&self.inner.power);
            power.reset(applied.group, suspended, now);
            !suspended
                && power
                    .observed
                    .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
        };
        let sampled = if sample_due {
            Some(if self.inner.native_services {
                oink_input::power::observe()
                    .map(|state| Snapshot { available: true, state })
                    .unwrap_or_default()
            } else {
                #[cfg(test)]
                {
                    lock(&self.inner.power).fake_source
                }
                #[cfg(not(test))]
                {
                    Snapshot::default()
                }
            })
        } else {
            None
        };
        let (effective, outgoing) = {
            let mut power = lock(&self.inner.power);
            if let Some(sample) = sampled {
                if power.observed.is_none() || power.local != sample {
                    tracing::info!(
                        available = sample.available,
                        display = sample.state.display,
                        system = sample.state.system,
                        activity = sample.state.activity,
                        "local power observation changed"
                    );
                }
                power.local = sample;
                power.observed = Some(now);
            }
            let local = if suspended { Snapshot::default() } else { power.local };
            let mut outgoing = Vec::new();
            power
                .sent
                .retain(|peer, _| applied.members.contains(peer) && group.peer_boot(*peer).is_some());
            for peer in &applied.members {
                if *peer == self.id() || !group.peer_supports(*peer, CAPABILITY) {
                    continue;
                }
                let Some(boot) = group.peer_boot(*peer) else { continue };
                if power.sent.get(peer).is_none_or(|(old, at, state)| {
                    *old != boot || *state != local || now.saturating_duration_since(*at) >= HEARTBEAT
                }) {
                    outgoing.push((*peer, boot, local));
                    // Record attempts to bound retries even when a sender is unavailable.
                    power.sent.insert(*peer, (boot, now, local));
                }
            }
            let mut effective = power.aggregate(now, |peer| {
                applied.members.contains(&peer).then(|| group.peer_boot(peer)).flatten()
            });
            effective = policy(
                effective,
                suspended,
                locked,
                lock(&self.inner.config).settings.routing.wake_displays_on_input,
            );
            if power.applied != effective {
                tracing::info!(
                    keep_display_awake = effective.display,
                    keep_system_awake = effective.system,
                    user_activity = effective.activity,
                    "mirrored power state changed"
                );
                power.applied = effective;
            }
            (effective, outgoing)
        };
        for (peer, _, snapshot) in outgoing {
            let _ = group.send_device_control(peer, snapshot.message());
        }
        // No OS work in fake agents. Tests exercise the same aggregate/lifecycle logic.
        if self.inner.native_services {
            let error = holder.apply(effective).err();
            if *last_error != error {
                tracing::info!(error=?error,"native power application availability changed");
                *last_error = error;
            }
            if mirrored_idle_refresh_due(effective, *last_activity, now) {
                let _ = self.input_backend().reset_user_idle();
                *last_activity = Some(now);
            }
        }
    }
}
// macOS session-idle pulses are input-driven only. A power/audio lease must
// never turn into synthetic user activity, including while an audio stream lasts.
fn mirrored_idle_refresh_due(state: State, last: Option<Instant>, now: Instant) -> bool {
    cfg!(windows) && state.activity && last.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "macos")]
    fn power_and_audio_leases_never_generate_mac_session_idle_pulses() {
        let now = Instant::now();
        for state in [
            State::default(),
            State {
                system: true,
                ..State::default()
            },
            State {
                display: true,
                system: true,
                activity: true,
            },
        ] {
            for second in [0, 1, 5, 60, 1200, 3600] {
                assert!(!mirrored_idle_refresh_due(
                    state,
                    None,
                    now + Duration::from_secs(second)
                ));
            }
        }
    }
    fn boot(n: u8) -> Boot {
        Boot {
            device_id: DeviceId([n; 32]),
            nonce: [n; 16],
        }
    }
    #[test]
    fn lock_and_display_opt_out_never_promote_or_suppress_system_only_state() {
        let both = State {
            display: true,
            system: true,
            activity: true,
        };
        let system = State {
            system: true,
            ..State::default()
        };
        assert_eq!(policy(both, false, true, true), system);
        assert_eq!(policy(both, false, false, false), system);
        assert_eq!(policy(both, true, false, true), State::default());
        assert_eq!(policy(system, false, false, true), system);
    }
    #[test]
    fn leases_or_sources_and_never_echo_into_local_snapshot() {
        let now = Instant::now();
        let mut power = Power::default();
        power.receive(
            boot(1),
            State {
                display: true,
                ..State::default()
            },
            now,
        );
        power.receive(
            boot(2),
            State {
                system: true,
                ..State::default()
            },
            now + Duration::from_secs(1),
        );
        let active = |p: DeviceId| Some(boot(p.0[0]));
        assert_eq!(
            power.aggregate(now, active),
            State {
                display: true,
                system: true,
                activity: false
            }
        );
        assert_eq!(power.local, Snapshot::default());
        assert_eq!(
            power.aggregate(now + LEASE, active),
            State {
                system: true,
                ..State::default()
            }
        );
        assert_eq!(
            power.aggregate(now + LEASE + Duration::from_secs(1), active),
            State::default()
        );
    }
    #[test]
    fn disconnect_boot_change_pause_and_delayed_resume_snapshot_release() {
        let now = Instant::now();
        let mut p = Power::default();
        p.receive(
            boot(1),
            State {
                system: true,
                ..State::default()
            },
            now,
        );
        assert_eq!(p.aggregate(now, |_| None), State::default());
        p.receive(
            boot(1),
            State {
                activity: true,
                ..State::default()
            },
            now,
        );
        assert_eq!(
            p.aggregate(now, |_| Some(Boot {
                nonce: [9; 16],
                ..boot(1)
            })),
            State::default()
        );
        let group = GroupId([1; 16]);
        p.reset(group, true, now);
        p.receive(
            boot(1),
            State {
                display: true,
                ..State::default()
            },
            now,
        );
        assert!(p.remote.is_empty());
        p.reset(group, false, now + Duration::from_secs(2));
        p.receive(
            boot(1),
            State {
                display: true,
                ..State::default()
            },
            now,
        );
        assert!(p.remote.is_empty());
        p.receive(
            boot(1),
            State {
                display: true,
                ..State::default()
            },
            now + Duration::from_secs(3),
        );
        assert_eq!(p.remote.len(), 1);
    }
}

#[cfg(all(test, debug_assertions))]
mod network_tests {
    use super::*;
    use crate::api::AgentApi;
    use crate::snapshot_tests::{Displays, pair, start_with_clip, until};
    fn source(node: &Node, state: State) {
        let mut p = lock(&node.inner.power);
        p.fake_source = Snapshot { available: true, state };
        p.observed = None;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn authenticated_mirroring_preserves_semantics_clears_and_does_not_echo() {
        let ah = tempfile::tempdir().unwrap();
        let bh = tempfile::tempdir().unwrap();
        let ac = oink_test_support::network_config(ah.path().into(), "synthetic-a", 0);
        let bc = oink_test_support::network_config(bh.path().into(), "synthetic-b", 0);
        let a = start_with_clip(&ac, &Displays::new("a"), None).await;
        let b = start_with_clip(&bc, &Displays::new("b"), None).await;
        pair(&a, &b).await;
        until("power agreement", || {
            a.link_config()
                .unwrap()
                .peer_agreement(b.id())
                .is_some_and(|a| a.contains(CAPABILITY))
        })
        .await;
        assert!(a.link_config().unwrap().peer_supports(b.id(), CAPABILITY));
        assert!(!a.link_config().unwrap().peer_supports(b.id(), "unsupported/1"));
        let system = State {
            system: true,
            ..State::default()
        };
        source(&a, system);
        until("system-only state", || lock(&b.inner.power).applied == system).await;
        assert_eq!(lock(&a.inner.power).applied, State::default(), "no feedback");
        source(
            &a,
            State {
                display: true,
                ..State::default()
            },
        );
        until("display-only state", || {
            lock(&b.inner.power).applied
                == State {
                    display: true,
                    ..State::default()
                }
        })
        .await;
        let mut settings = b.input_settings().unwrap();
        settings.wake_displays_on_input = false;
        b.set_input_settings(settings.into()).unwrap();
        until("display opt-out", || lock(&b.inner.power).applied == State::default()).await;
        source(&a, system);
        until("system survives display opt-out", || {
            lock(&b.inner.power).applied == system
        })
        .await;
        // A source failure sends an explicit unavailable clear, without waiting
        // for the previous active lease to expire.
        {
            let mut p = lock(&a.inner.power);
            p.fake_source = Snapshot::default();
            p.observed = None;
        }
        until("unavailable source released", || {
            lock(&b.inner.power).applied == State::default()
        })
        .await;
        source(&a, system);
        until("available source recovered", || lock(&b.inner.power).applied == system).await;
        b.set_local_paused(crate::device_controls::runtime::fresh_request_id(), true)
            .unwrap();
        until("paused receiver released", || {
            b.pause_status().paused && lock(&b.inner.power).applied == State::default()
        })
        .await;
        b.set_local_paused(crate::device_controls::runtime::fresh_request_id(), false)
            .unwrap();
        until("fresh snapshot after resume", || lock(&b.inner.power).applied == system).await;
        source(&a, State::default());
        until("source ended", || lock(&b.inner.power).applied == State::default()).await;
        source(&a, system);
        until("system restarted", || lock(&b.inner.power).applied == system).await;
        a.shutdown().await;
        until("disconnect released", || {
            lock(&b.inner.power).applied == State::default()
        })
        .await;
        b.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn released_peer_never_receives_new_power_message() {
        let ah = tempfile::tempdir().unwrap();
        let bh = tempfile::tempdir().unwrap();
        let ac = oink_test_support::network_config(ah.path().into(), "synthetic-a", 0);
        let bc = oink_test_support::network_config(bh.path().into(), "synthetic-b", 0);
        let a = start_with_clip(&ac, &Displays::new("a"), None).await;
        let b = Node::prepare(NodeOptions {
            net: bc,
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: Some(Displays::new("b")),
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .unwrap();
        let mut profile = Node::offer_profile();
        profile.capabilities.retain(|cap| cap != CAPABILITY);
        b.start_sharing_with_profile(profile).await.unwrap();
        pair(&a, &b).await;
        until("old peer agreement", || {
            a.link_config().unwrap().peer_agreement(b.id()).is_some()
        })
        .await;
        assert!(!a.link_config().unwrap().peer_supports(b.id(), CAPABILITY));
        source(
            &a,
            State {
                display: true,
                system: true,
                activity: true,
            },
        );
        until("source sampled", || lock(&a.inner.power).local.state.display).await;
        assert!(!lock(&a.inner.power).sent.contains_key(&b.id()));
        assert!(
            a.link_config()
                .unwrap()
                .send_device_control(b.id(), Snapshot::default().message())
                .is_err()
        );
        assert_eq!(lock(&b.inner.power).applied, State::default());
        a.shutdown().await;
        b.shutdown().await;
    }
}
