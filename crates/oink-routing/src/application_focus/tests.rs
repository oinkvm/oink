use super::*;

#[test]
fn receiver_park_is_authenticated_conditional_and_cancelled_by_local_activation() {
    let mut m = Model::new(boot(2));
    m.configure(context(1, 1), 0);
    observe(&mut m, 2, 1, Foreground::Windowed);
    m.drain_effects();
    let request = ParkRequest {
        claim: m.state().unwrap().claim.clone(),
        revision: 0,
        operation: 1,
        target: boot(2),
        expected: observation(1, Foreground::Windowed).stamp,
    };
    assert!(!m.accept_prepare(boot(3), request.clone(), 2));
    assert!(m.accept_prepare(boot(1), request.clone(), 2));
    assert_eq!(m.drain_effects(), vec![Effect::Park(request.clone())]);
    assert!(!m.accept_prepare(boot(1), request.clone(), 3));
    observe(&mut m, 2, 2, Foreground::Windowed);
    assert_eq!(
        m.drain_effects(),
        vec![
            Effect::CancelPark(request.clone()),
            Effect::ParkFinished {
                request: request.clone(),
                result: ParkResult::Stale
            },
        ]
    );
    assert!(!m.park_completed(&request, ParkResult::Confirmed));
    assert_eq!(m.state().unwrap().keyboard, None, "non-P activation cannot claim K");
}

#[test]
fn receiver_park_cannot_survive_claim_change_or_fullscreen_uncertainty() {
    let mut m = Model::new(boot(2));
    m.configure(context(1, 1), 0);
    observe(&mut m, 2, 1, Foreground::OrdinaryUnknown);
    m.drain_effects();
    let mut request = ParkRequest {
        claim: m.state().unwrap().claim.clone(),
        revision: 0,
        operation: 1,
        target: boot(2),
        expected: observation(1, Foreground::Windowed).stamp,
    };
    assert!(m.accept_prepare(boot(1), request.clone(), 2));
    assert_eq!(
        m.drain_effects(),
        vec![Effect::ParkFinished {
            request: request.clone(),
            result: ParkResult::UnknownExempt
        }]
    );
    observe(&mut m, 2, 2, Foreground::Windowed);
    request.operation = 2;
    request.expected.sequence = 2;
    assert!(m.accept_prepare(boot(1), request.clone(), 3));
    m.drain_effects();
    m.configure(context(3, 2), 4);
    assert!(m.drain_effects().contains(&Effect::CancelPark(request.clone())));
    assert!(!m.park_completed(&request, ParkResult::Confirmed));
    assert!(!m.accept_prepare(boot(1), request, 5));
}

fn boot(n: u8) -> Boot {
    Boot {
        device_id: DeviceId([n; 32]),
        nonce: [n; 16],
    }
}
fn member(n: u8) -> Member {
    Member {
        boot: boot(n),
        routing_instance: [n; 16],
        capable: true,
        parkable: true,
        controllable: true,
    }
}
fn context(pointer: u8, epoch: u64) -> Context {
    Context {
        enabled: true,
        claim: Some(Claim {
            epoch,
            pointer: boot(pointer),
            scope: FocusScope {
                members: vec![
                    (boot(1).device_id, [1; 16]),
                    (boot(2).device_id, [2; 16]),
                    (boot(3).device_id, [3; 16]),
                ],
            },
        }),
        members: vec![member(1), member(2), member(3)],
        suspended: None,
        game: false,
    }
}
fn observation(sequence: u64, foreground: Foreground) -> Observation {
    Observation {
        stamp: ObservationStamp {
            generation: 1,
            sequence,
        },
        foreground,
        hidden_lost: false,
        park_operation: None,
    }
}
fn observe(model: &mut Model, n: u8, seq: u64, foreground: Foreground) {
    let claim = model.state().unwrap().claim.clone();
    assert!(model.observe(&claim, boot(n), observation(seq, foreground), seq));
}
fn finish_parks(model: &mut Model) {
    let requests: Vec<_> = model.pending.values().map(|p| p.request.clone()).collect();
    for request in requests {
        assert!(model.park_completed(&request, ParkResult::Confirmed));
    }
}
fn active() -> Model {
    let mut m = Model::new(boot(1));
    m.configure(context(1, 1), 0);
    observe(&mut m, 1, 1, Foreground::Windowed);
    observe(&mut m, 2, 1, Foreground::Windowed);
    observe(&mut m, 3, 1, Foreground::Fullscreen);
    finish_parks(&mut m);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    m.drain_effects();
    m
}
fn entered(pointer: u8, foreground: Foreground, mut previous: Snapshot, capable_old: bool) -> Model {
    let mut m = Model::new(boot(pointer));
    let old_pointer = previous.claim.pointer.device_id.0[0];
    let mut old_context = context(old_pointer, previous.claim.epoch);
    if !capable_old {
        old_context.members[(old_pointer - 1) as usize].capable = false;
    }
    m.configure(old_context.clone(), 0);
    if capable_old {
        assert!(m.accept_snapshot(previous.claim.pointer, previous.clone()));
    } else {
        // A legacy P cannot send the new message: every capable observer derives
        // its P=K state from the existing accepted pointer claim instead.
        previous = m.state().unwrap().clone();
        assert_eq!(previous.keyboard, Some(boot(old_pointer)));
    }
    let mut next = context(pointer, previous.claim.epoch + 1);
    if !capable_old {
        next.members[(old_pointer - 1) as usize].capable = false;
    }
    observe(&mut m, pointer, 1, foreground);
    m.configure(next, 1);
    observe(&mut m, pointer, 1, foreground);
    for n in 1..=3 {
        if n != pointer && (capable_old || n != old_pointer) {
            observe(&mut m, n, 1, Foreground::Windowed);
        }
    }
    finish_parks(&mut m);
    m.drain_effects();
    m
}

#[test]
fn disabled_group_never_parks_or_forwards() {
    let mut m = Model::new(boot(1));
    let mut c = context(1, 1);
    c.enabled = false;
    m.configure(c, 0);
    assert_eq!(m.state().unwrap().phase, Phase::Off);
    assert!(m.forward_route().is_none());
    assert!(
        !m.drain_effects()
            .iter()
            .any(|e| matches!(e, Effect::Park(_) | Effect::Sample(_)))
    );
}

#[test]
fn preparation_parks_non_keyboard_members_once_and_exempts_fullscreen() {
    let mut m = Model::new(boot(1));
    m.configure(context(1, 1), 0);
    observe(&mut m, 1, 1, Foreground::Windowed);
    observe(&mut m, 2, 1, Foreground::Windowed);
    observe(&mut m, 3, 1, Foreground::Fullscreen);
    let effects = m.drain_effects();
    assert_eq!(effects.iter().filter(|e| matches!(e, Effect::Park(_))).count(), 1);
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::ParkFinished {
            result: ParkResult::FullscreenExempt,
            ..
        }
    )));
    assert!(m.forward_route().is_none());
    finish_parks(&mut m);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    m.drain_effects();
    observe(&mut m, 3, 2, Foreground::Windowed);
    observe(&mut m, 2, 2, Foreground::Windowed);
    assert!(!m.drain_effects().iter().any(|e| matches!(e, Effect::Park(_))));
    assert_eq!(m.state().unwrap().keyboard, Some(boot(1)));
}

#[test]
fn hidden_entry_keeps_keyboard_but_external_entry_claims_it() {
    let previous = active().state().unwrap().clone();
    let hidden = entered(2, Foreground::Hidden, previous.clone(), true);
    assert_eq!(hidden.forward_route().unwrap().destination, boot(1));
    for foreground in [
        Foreground::Windowed,
        Foreground::Fullscreen,
        Foreground::OrdinaryUnknown,
    ] {
        let m = entered(2, foreground, previous.clone(), true);
        assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
        assert!(m.forward_route().is_none());
    }
}

#[test]
fn confirmed_loss_retires_old_route_before_publishing_and_does_not_migrate_keys() {
    let mut m = entered(2, Foreground::Hidden, active().state().unwrap().clone(), true);
    let route = m.forward_route().unwrap();
    observe(&mut m, 2, 2, Foreground::Windowed);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
    assert!(m.forward_route().is_none());
    let effects = m.drain_effects();
    assert_eq!(effects.first(), Some(&Effect::Retire(route)));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Publish(s) if s.keyboard == Some(boot(2))))
    );
    // Model effects cannot inject, replay DOWN, mirror modifiers, or wait for keys.
    assert!(effects.iter().all(|e| !matches!(e, Effect::Dismiss)));
}

#[test]
fn unknown_foreground_immediately_claims_keyboard() {
    let mut m = entered(2, Foreground::Hidden, active().state().unwrap().clone(), true);
    observe(&mut m, 2, 2, Foreground::Unknown);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
    assert!(m.forward_route().is_none());
    observe(&mut m, 2, 3, Foreground::Windowed);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
}

#[test]
fn non_pointer_local_use_never_claims_keyboard_or_resteals() {
    let mut m = active();
    observe(&mut m, 2, 2, Foreground::Windowed);
    observe(&mut m, 3, 2, Foreground::Windowed);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(1)));
    assert!(m.drain_effects().is_empty());
}

#[test]
fn mixed_group_excludes_old_peer_without_disabling_capable_peers() {
    let mut m = Model::new(boot(1));
    let mut c = context(1, 1);
    c.members[2].capable = false;
    m.configure(c, 0);
    observe(&mut m, 1, 1, Foreground::Windowed);
    observe(&mut m, 2, 1, Foreground::Windowed);
    finish_parks(&mut m);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    assert_eq!(m.state().unwrap().participants, vec![boot(1), boot(2)]);
    assert!(!m.drain_effects().iter().any(
        |e| matches!(e, Effect::Sample(b) if *b == boot(3)) || matches!(e, Effect::Park(p) if p.target == boot(3))
    ));
}

#[test]
fn legacy_pointer_then_hidden_capable_entry_has_no_special_claim_or_restore() {
    let mut previous = active().state().unwrap().clone();
    previous.claim = context(3, 2).claim.unwrap();
    let mut m = entered(2, Foreground::Hidden, previous, false);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(3)));
    assert!(m.forward_route().is_none());
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    observe(&mut m, 2, 2, Foreground::Windowed);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
    assert!(
        !m.drain_effects()
            .iter()
            .any(|e| matches!(e, Effect::Park(p) if p.target == boot(3)))
    );
}

#[test]
fn late_park_completion_cannot_override_a_new_local_observation() {
    let mut m = Model::new(boot(1));
    m.configure(context(1, 1), 0);
    observe(&mut m, 1, 1, Foreground::Windowed);
    observe(&mut m, 2, 1, Foreground::Windowed);
    let request = m.pending[&boot(2).device_id].request.clone();
    observe(&mut m, 2, 2, Foreground::Windowed);
    assert!(!m.park_completed(&request, ParkResult::Confirmed));
    assert!(
        m.drain_effects()
            .iter()
            .any(|e| *e == Effect::CancelPark(request.clone()))
    );
    observe(&mut m, 2, 3, Foreground::Windowed);
    assert!(!m.drain_effects().iter().any(|e| matches!(e, Effect::Park(_))));
}

#[test]
fn timed_out_parks_do_not_retry_or_hold_up_local_keyboard_delivery() {
    let mut m = Model::new(boot(1));
    m.configure(context(1, 1), 0);
    observe(&mut m, 1, 1, Foreground::Windowed);
    observe(&mut m, 2, 1, Foreground::Windowed);
    m.tick(PARK_TIMEOUT_US + 2);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    assert!(m.forward_route().is_none());
    m.drain_effects();
    m.tick(PARK_TIMEOUT_US * 10);
    observe(&mut m, 3, 1, Foreground::Windowed);
    assert!(!m.drain_effects().iter().any(|e| matches!(e, Effect::Park(_))));
}

#[test]
fn pause_drag_and_unavailable_session_fence_routes_and_pending_operations() {
    for reason in [Reason::DesktopUnavailable, Reason::Drag] {
        let mut m = entered(2, Foreground::Hidden, active().state().unwrap().clone(), true);
        let old = m.state().unwrap().claim.clone();
        let mut c = context(2, 2);
        c.suspended = Some(reason);
        m.configure(c, 20);
        assert!(m.forward_route().is_none());
        assert_eq!(m.state().unwrap().phase, Phase::Suspended);
        let effects = m.drain_effects();
        assert!(matches!(effects.first(), Some(Effect::Retire(_))));
        assert!(effects.contains(&Effect::Dismiss));
        assert!(m.observe(&old, boot(2), observation(5, Foreground::Windowed), 21));
        assert_eq!(m.state().unwrap().phase, Phase::Suspended);
    }
}

#[test]
fn game_requires_keyboard_on_pointer_and_local_mode_claims_keyboard() {
    let mut m = entered(2, Foreground::Hidden, active().state().unwrap().clone(), true);
    let mut c = context(2, 2);
    c.game = true;
    m.configure(c, 10);
    assert_eq!(m.state().unwrap().reason, Reason::Game);
    assert!(m.forward_route().is_none());
    let mut c = context(2, 2);
    c.members[1].parkable = false;
    m.configure(c, 11);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
    assert!(m.state().unwrap().participants.contains(&boot(2)));
    assert!(m.forward_route().is_none());
}

#[test]
fn snapshots_cannot_win_with_an_old_claim_wrong_author_or_rebooted_keyboard() {
    let snapshot = active().state().unwrap().clone();
    let mut m = Model::new(boot(2));
    m.configure(context(1, 1), 0);
    assert!(!m.accept_snapshot(boot(3), snapshot.clone()));
    assert!(m.accept_snapshot(boot(1), snapshot.clone()));
    assert!(!m.accept_snapshot(boot(1), snapshot.clone()));
    m.configure(context(2, 2), 1);
    let mut stale = snapshot;
    stale.revision = u64::MAX;
    assert!(!m.accept_snapshot(boot(1), stale));
    let claim = m.state().unwrap().claim.clone();
    let reboot = Boot {
        nonce: [99; 16],
        ..boot(1)
    };
    assert!(!m.observe(&claim, reboot, observation(100, Foreground::Windowed), 2));
}

#[test]
fn stale_or_duplicate_observations_do_not_change_the_state() {
    let mut m = active();
    let before = m.state().unwrap().clone();
    assert!(!m.observe(&before.claim, boot(1), observation(1, Foreground::Fullscreen), 5));
    let mut old = before.claim.clone();
    old.epoch = 0;
    assert!(!m.observe(&old, boot(1), observation(200, Foreground::Fullscreen), 6));
    assert_eq!(m.state(), Some(&before));
    assert!(m.drain_effects().is_empty());
}

#[test]
fn native_desktop_recovery_resumes_without_a_new_pointer_claim_or_config_edit() {
    let mut m = active();
    let claim = m.state().unwrap().claim.clone();
    observe(&mut m, 1, 2, Foreground::Unavailable);
    assert_eq!(m.state().unwrap().phase, Phase::Suspended);
    m.drain_effects();
    observe(&mut m, 1, 3, Foreground::Windowed);
    assert_eq!(m.state().unwrap().claim, claim);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(1)));
    assert!(!m.drain_effects().iter().any(|e| matches!(e, Effect::Park(_))));
}

#[test]
fn native_timeout_generation_retires_old_evidence_and_accepts_recovery() {
    let mut m = active();
    let claim = m.state().unwrap().claim.clone();
    let mut timeout = observation(1, Foreground::Unavailable);
    timeout.stamp.generation = 2;
    assert!(m.observe(&claim, boot(1), timeout, 10));
    assert_eq!(m.state().unwrap().phase, Phase::Suspended);
    assert!(!m.observe(&claim, boot(1), observation(99, Foreground::Windowed), 11));
    let mut recovered = observation(2, Foreground::Windowed);
    recovered.stamp.generation = 2;
    assert!(m.observe(&claim, boot(1), recovered, 12));
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(1)));
}

#[test]
fn group_disable_retires_and_reenable_prepares_capable_members_again() {
    let mut m = active();
    let mut c = context(1, 1);
    c.enabled = false;
    m.configure(c.clone(), 10);
    assert_eq!(m.state().unwrap().phase, Phase::Off);
    assert!(m.state().unwrap().participants.is_empty());
    assert!(m.drain_effects().iter().any(|e| matches!(e, Effect::Dismiss)));
    c.enabled = true;
    c.members[2].capable = false;
    m.configure(c, 20);
    let effects = m.drain_effects();
    assert!(effects.iter().any(|e| matches!(e, Effect::Sample(b) if *b == boot(2))));
    assert!(!effects.iter().any(|e| matches!(e, Effect::Sample(b) if *b == boot(3))));
    observe(&mut m, 1, 3, Foreground::Windowed);
    observe(&mut m, 2, 3, Foreground::Windowed);
    assert!(
        m.drain_effects()
            .iter()
            .any(|e| matches!(e, Effect::Park(p) if p.target == boot(2)))
    );
    finish_parks(&mut m);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
}

#[test]
fn follower_enabled_after_authority_sample_announces_fresh_observation() {
    let mut follower = Model::new(boot(2));
    let mut disabled = context(1, 1);
    disabled.enabled = false;
    follower.configure(disabled, 0);
    follower.drain_effects();
    let claim = context(1, 1).claim.unwrap();
    assert!(!follower.observe(&claim, boot(2), observation(1, Foreground::Windowed), 1));
    // Group state and input control use different streams: the authority's
    // initial Sample can arrive while this member is still disabled.
    follower.configure(context(1, 1), 2);
    assert!(follower.drain_effects().contains(&Effect::Sample(boot(2))));
    assert!(follower.observe(&claim, boot(2), observation(2, Foreground::Windowed), 3));
}

#[test]
fn unchanged_sample_can_prepare_again_without_replaying_activation() {
    let mut m = active();
    let before = m.state().unwrap().clone();
    assert!(m.observe(&before.claim, boot(1), observation(1, Foreground::Windowed), 5));
    assert_eq!(m.state(), Some(&before));
    assert!(m.drain_effects().is_empty());
    let mut c = context(1, 1);
    c.enabled = false;
    m.configure(c.clone(), 10);
    c.enabled = true;
    m.configure(c, 11);
    observe(&mut m, 1, 1, Foreground::Windowed);
    observe(&mut m, 2, 1, Foreground::Windowed);
    observe(&mut m, 3, 1, Foreground::Fullscreen);
    finish_parks(&mut m);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
}

#[test]
fn observed_loss_during_pointer_button_suspension_claims_keyboard_on_release() {
    click_during_suspension(false);
}

#[test]
fn latched_loss_during_pointer_button_suspension_claims_keyboard_on_release() {
    click_during_suspension(true);
}

fn click_during_suspension(native_latch: bool) {
    let mut m = entered(2, Foreground::Hidden, active().state().unwrap().clone(), true);
    assert_eq!(m.forward_route().unwrap().destination, boot(1));
    let mut c = context(2, 2);
    c.suspended = Some(Reason::Drag);
    m.configure(c.clone(), 20);
    m.drain_effects();
    // Mouse-down suspends routing before the OS reports the clicked app.
    // Test both observed Hidden -> Windowed and explicit native loss evidence.
    let claim = m.state().unwrap().claim.clone();
    let mut clicked = observation(5, Foreground::Windowed);
    clicked.hidden_lost = native_latch;
    assert!(m.observe(&claim, boot(2), clicked, 21));
    assert_eq!(m.state().unwrap().phase, Phase::Suspended);
    c.suspended = None;
    m.configure(c, 30);
    // A stable sample is valid preparation evidence too: the transition was
    // already observed while forwarding was suspended and must not be lost.
    assert!(m.observe(&claim, boot(2), clicked, 31));
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
    assert!(m.forward_route().is_none());
}

#[test]
fn entry_without_any_native_observation_immediately_claims_keyboard() {
    let previous = active().state().unwrap().clone();
    let mut m = Model::new(boot(2));
    m.configure(context(1, 1), 0);
    assert!(m.accept_snapshot(boot(1), previous));
    m.configure(context(2, 2), 10);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
    assert!(m.forward_route().is_none());
    // A later successful observation cannot resurrect old K.
    observe(&mut m, 2, 1, Foreground::Hidden);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(2)));
}

#[test]
fn local_mode_cancels_pending_native_park_and_rejects_late_work() {
    let mut m = Model::new(boot(2));
    m.configure(context(1, 1), 0);
    observe(&mut m, 2, 1, Foreground::Windowed);
    m.drain_effects();
    let request = ParkRequest {
        claim: m.state().unwrap().claim.clone(),
        revision: 0,
        operation: 1,
        target: boot(2),
        expected: observation(1, Foreground::Windowed).stamp,
    };
    assert!(m.accept_prepare(boot(1), request.clone(), 2));
    m.drain_effects();
    let mut c = context(1, 1);
    c.members[1].parkable = false;
    m.configure(c, 3);
    let effects = m.drain_effects();
    assert!(effects.contains(&Effect::CancelPark(request.clone())));
    assert!(effects.contains(&Effect::Dismiss));
    assert!(!effects.iter().any(|e| matches!(e, Effect::Sample(_) | Effect::Park(_))));
    assert!(!m.accept_prepare(boot(1), request.clone(), 4));
    assert!(!m.park_completed(&request, ParkResult::Confirmed));
    assert!(!m.observe(&request.claim, boot(2), observation(2, Foreground::Hidden), 5));
    assert!(m.state().unwrap().participants.contains(&boot(2)));
}

#[test]
fn all_local_members_become_active_without_native_work() {
    let mut m = Model::new(boot(1));
    let mut c = context(1, 1);
    for member in &mut c.members {
        member.parkable = false;
    }
    m.configure(c, 0);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(1)));
    assert_eq!(m.state().unwrap().participants.len(), 3);
    assert_eq!(m.deadline(), None);
    assert!(
        !m.drain_effects()
            .iter()
            .any(|e| matches!(e, Effect::Sample(_) | Effect::Park(_)))
    );
}

#[test]
#[cfg(debug_assertions)]
fn manual_clock_expires_preparation_and_late_sample_cannot_retry_parking() {
    let clock = oink_foundation::application_focus::TestClock::default();
    let mut m = Model::new(boot(1));
    m.set_test_clock(clock.clone());
    m.configure(context(1, 1), 1_000_000);
    observe(&mut m, 1, 1, Foreground::Windowed);
    m.drain_effects();
    m.tick(u64::MAX);
    assert_eq!(m.state().unwrap().phase, Phase::Preparing);
    assert_eq!(m.deadline(), None, "manual timers must not busy-spin a native owner");
    clock.advance(PARK_TIMEOUT_US);
    m.tick(u64::MAX);
    assert_eq!(m.state().unwrap().phase, Phase::Active);
    assert_eq!(m.state().unwrap().keyboard, Some(boot(1)));
    observe(&mut m, 2, 1, Foreground::Windowed);
    assert!(!m.drain_effects().iter().any(|e| matches!(e, Effect::Park(_))));
}
