use super::*;
use oink_foundation::Point;
fn p(x: f64) -> Point {
    Point::new(x, 100.)
}
// Keep warp accounting active when an accelerated child supplies routed motion.
fn frame_motion(
    f: &mut WarpDeltaFix,
    raw: (f64, f64),
    at: Point,
    ts: u64,
    hid: bool,
    held: bool,
    child: Option<(f64, f64)>,
) -> (f64, f64) {
    motion(
        f,
        raw,
        at,
        ts,
        hid,
        Frame {
            physical: hid,
            ordinary: !held,
            buttons: if held { Buttons::Held } else { Buttons::Released },
        },
        || child.ok_or(Reason::NoAttachedEvent),
    )
    .routed
}
#[test]
fn warp_accounting_consumes_pre_press_fold_on_ordinary_frame() {
    let mut f = WarpDeltaFix::new();
    f.motion((0., 0.), p(100.), 1, true);
    f.posted_move(p(200.));
    assert_eq!(
        frame_motion(&mut f, (101., 0.), p(201.), 2, true, false, Some((1., 0.))),
        (1., 0.)
    );
    assert_eq!(frame_motion(&mut f, (1., 0.), p(202.), 3, true, true, None), (1., 0.));
}
#[test]
fn placement_before_press_is_consumed_on_drag() {
    let mut old = WarpDeltaFix::new();
    let mut f = WarpDeltaFix::new();
    for x in [&mut old, &mut f] {
        x.motion((0., 0.), p(100.), 1, true);
        x.posted_move(p(200.));
    }
    let want = old.motion((101., 0.), p(201.), 3, true);
    assert_eq!(frame_motion(&mut f, (101., 0.), p(201.), 3, true, true, None), want);
    assert_eq!(f.snapshot(), old.snapshot());
}
#[test]
fn release_before_consumption_protects_ordinary_and_retains_same_next_press_state() {
    let mut old = WarpDeltaFix::new();
    let mut f = WarpDeltaFix::new();
    for x in [&mut old, &mut f] {
        x.motion((0., 0.), p(100.), 1, true);
        x.warped(p(104.), 2);
    }
    // Ordinary motion uses its authenticated accelerated child.
    assert_eq!(old.motion((1., 0.), p(101.), 3, true), (1., 0.));
    assert_eq!(
        frame_motion(&mut f, (1., 0.), p(101.), 3, true, false, Some((-3., 0.))),
        (-3., 0.)
    );
    assert_eq!(f.snapshot(), old.snapshot());
    assert_eq!(
        frame_motion(&mut f, (-2., 0.), p(99.), 4, true, true, None),
        old.motion((-2., 0.), p(99.), 4, true)
    );
}
#[test]
fn only_ordinary_physical_motion_uses_child_displacement() {
    for held in [false, true] {
        let mut f = WarpDeltaFix::new();
        f.motion((0., 0.), p(100.), 1, true);
        f.warped(p(500.), 2);
        // Nonphysical placement accounting applies regardless of button state.
        assert_eq!(frame_motion(&mut f, (7., 0.), p(500.), 3, false, held, None), (0., 0.));
        assert_eq!(
            frame_motion(&mut f, (401., 0.), p(501.), 4, true, held, Some((1., 0.))),
            if held { (401., 0.) } else { (1., 0.) }
        );
        let mut f = WarpDeltaFix::new();
        f.motion((0., 0.), p(100.), 1, true);
        f.warped(p(104.), 2);
        assert_eq!(
            frame_motion(&mut f, (1., 0.), p(101.), 3, true, held, Some((-3., 0.))),
            if held { (1., 0.) } else { (-3., 0.) }
        );
        let mut f = WarpDeltaFix::new();
        f.motion((0., 0.), p(100.), 1, true);
        f.warped(p(500.), 2);
        f.enter_remote();
        assert_eq!(
            frame_motion(&mut f, (401., 0.), p(501.), 4, true, held, Some((1., 0.))),
            if held { (401., 0.) } else { (1., 0.) }
        );
    }
}
fn parent() -> HidNode {
    HidNode {
        kind: POINTER_TYPE,
        flags: 0,
        timestamp: 12,
        sender: 9,
        delta: (2., 0.),
        buttons: Some(0),
    }
}
fn child(delta: (f64, f64)) -> HidNode {
    HidNode {
        flags: ACCELERATED,
        delta,
        ..parent()
    }
}
fn frame() -> Frame {
    Frame {
        physical: true,
        ordinary: true,
        buttons: Buttons::Released,
    }
}

#[test]
fn only_proven_ordinary_frames_may_read_the_accelerated_child() {
    for candidate in [
        Frame {
            physical: false,
            ..frame()
        },
        Frame {
            ordinary: false,
            ..frame()
        }, // left/right/other, including injected-button drags
        Frame {
            buttons: Buttons::Held,
            ..frame()
        },
        Frame {
            buttons: Buttons::Unknown,
            ..frame()
        },
    ] {
        let mut old = WarpDeltaFix::new();
        let mut actual = WarpDeltaFix::new();
        for f in [&mut old, &mut actual] {
            f.warped(p(104.), 2);
        }
        let want = old.motion((1., 0.), p(101.), 3, true);
        let got = motion(&mut actual, (1., 0.), p(101.), 3, true, candidate, || {
            panic!("ineligible source read")
        });
        assert_eq!(got.routed, want);
        assert_eq!(format!("{actual:?}"), format!("{old:?}"));
        assert_ne!(got.reason, Reason::Accelerated);
    }
    let mut f = WarpDeltaFix::new();
    assert_eq!(
        motion(&mut f, (7., 0.), p(100.), 3, false, frame(), || panic!(
            "unknown CG state read"
        ))
        .reason,
        Reason::NotPhysicalHid
    );
}

#[test]
fn missing_child_keeps_today_output_and_a_typed_reason_never_unknown_as_zero() {
    for reason in Reason::ALL.into_iter().filter(|r| *r != Reason::Accelerated) {
        let mut old = WarpDeltaFix::new();
        let mut f = WarpDeltaFix::new();
        for x in [&mut old, &mut f] {
            x.motion((0., 0.), p(100.), 1, true);
            x.warped(p(104.), 2);
        }
        let expected = old.motion((1., 0.), p(101.), 3, true);
        let output = motion(&mut f, (1., 0.), p(101.), 3, true, frame(), || Err(reason));
        assert_eq!(output.routed, expected);
        assert_eq!(output.reason, reason);
        assert_eq!(format!("{f:?}"), format!("{old:?}"));
    }
}
#[test]
fn same_frame_selector_requires_unique_flagged_finite_pointer_and_provenance() {
    let p = parent();
    let c = child((0.125, -0.25));
    assert_eq!(select(p, [p, HidNode { kind: 1, ..c }, c]), Ok(c.delta));
    assert_eq!(select(p, [child((0., 0.))]), Ok((0., 0.)), "measured zero is real");
    assert_eq!(select(p, []), Err(Reason::NoAcceleratedChild));
    assert_eq!(
        select(HidNode { delta: (0., 0.), ..p }, []),
        Err(Reason::ZeroRawWithoutChild)
    );
    assert_eq!(select(c, []), Err(Reason::AcceleratedRoot));
    assert_eq!(select(c, [c]), Err(Reason::AcceleratedRoot));
    assert_eq!(
        select(
            HidNode {
                delta: (f64::NAN, 0.),
                ..p
            },
            [c]
        ),
        Err(Reason::Nonfinite)
    );
    assert_eq!(select(p, [c, c]), Err(Reason::AmbiguousChildren));
    for root in [
        HidNode { kind: 1, ..p },
        HidNode { sender: 0, ..p },
        HidNode { timestamp: 0, ..p },
    ] {
        assert_eq!(select(root, [c]), Err(Reason::ProvenanceMismatch));
    }
    for bad in [HidNode { sender: 8, ..c }, HidNode { timestamp: 11, ..c }] {
        assert_eq!(select(p, [bad]), Err(Reason::ProvenanceMismatch));
    }
    for delta in [(f64::NAN, 0.), (0., f64::INFINITY), (f64::NEG_INFINITY, 0.)] {
        assert_eq!(select(p, [child(delta)]), Err(Reason::Nonfinite));
    }
}
#[test]
fn restart_held_and_unknown_buttons_never_qualify_as_released() {
    let mut g = ButtonGate::default();
    assert_eq!(g.state(100), Buttons::Unknown);
    g.seed(1, 10, 20);
    assert_eq!(g.state(19), Buttons::Unknown);
    assert_eq!(g.state(21), Buttons::Held);
    // A queued pre-seed up must not erase the restart's held snapshot.
    g.transition(0, false, 9);
    assert_eq!(g.state(22), Buttons::Held);
    g.transition(0, false, 23);
    assert_eq!(g.state(24), Buttons::Released);
    g.transition(31, true, 25);
    assert_eq!(g.state(26), Buttons::Held);
    g.transition(31, false, 27);
    assert_eq!(g.state(28), Buttons::Released);
    g.invalidate();
    assert_eq!(g.state(29), Buttons::Unknown);
    g.transition(0, false, 30);
    assert_eq!(g.state(31), Buttons::Unknown, "up cannot prove a stream gap empty");
    g.seed(0, 40, 45);
    g.transition(32, true, 46);
    assert_eq!(g.state(47), Buttons::Unknown);
}
#[test]
fn queued_button_transitions_during_seed_reconcile_both_directions() {
    // Sample saw up, but a later down arrived during the 32-button scan.
    let mut g = ButtonGate::default();
    g.seed(0, 10, 20);
    g.transition(0, true, 15);
    assert_eq!(g.state(18), Buttons::Unknown);
    assert_eq!(g.state(21), Buttons::Held);
    // Sample saw down, but the later physical up is already queued.
    g.seed(1, 30, 40);
    g.transition(0, false, 35);
    assert_eq!(g.state(41), Buttons::Released);
}

#[test]
fn authority_reason_codes_are_appended_numeric_and_dense() {
    for (index, reason) in Reason::ALL.into_iter().enumerate() {
        assert_eq!(index as u32, reason as u32);
    }
    assert_eq!(Reason::Accelerated as u32, 0);
    assert_eq!(Reason::ButtonsUnknown as u32, 13);
}

#[test]
fn attached_physical_buttons_veto_a_released_mixed_system_snapshot() {
    let p = parent();
    let c = child((0.125, 0.));
    // A native injected UP can clear mixed WindowServer state while the device
    // is still held. The same-frame pointer mask must also be empty.
    for bit in [1, 2, 4, 1u32 << 31] {
        assert_eq!(
            select(
                HidNode {
                    buttons: Some(bit),
                    ..p
                },
                [c]
            ),
            Err(Reason::ButtonsHeld)
        );
        assert_eq!(
            select(
                p,
                [HidNode {
                    buttons: Some(bit),
                    ..c
                }]
            ),
            Err(Reason::ButtonsHeld)
        );
    }
    assert_eq!(select(HidNode { buttons: None, ..p }, [c]), Err(Reason::ButtonsUnknown));
    assert_eq!(select(p, [HidNode { buttons: None, ..c }]), Err(Reason::ButtonsUnknown));
}
