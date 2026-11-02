use super::*;

fn inject(os: Os, amount: ScrollAmount, phase: ScrollPhase) -> InjectEvent {
    inject_from(os, amount, phase, SourceKind::Touchpad)
}

fn inject_from(os: Os, amount: ScrollAmount, phase: ScrollPhase, source: SourceKind) -> InjectEvent {
    let mut cfg = RouterConfig::new(
        oink_foundation::session::Boot {
            device_id: DeviceId([1; 32]),
            nonce: [DeviceId([1; 32]).0[0]; 16],
        },
        "device",
        os,
    );
    cfg.settings.fallback_reference_per_native_unit = 0.2;
    cfg.settings.line_height_ratio = 8.0;
    cfg.settings.remote_scroll_speed = 1.5;
    let mut r = Router::new(cfg, Arc::new(crate::SimpleLayout::default()));
    r.feel.wheel_lines_per_notch = Some(3.0);
    r.inject_scroll(DeviceId([2; 32]), source, 2.0, -4.0, amount, phase, None);
    match r.out.pop().unwrap() {
        Output::Inject(event) => event,
        other => panic!("{other:?}"),
    }
}

#[test]
fn windows_phased_reference_uses_fixed_canonical_scale() {
    for phase in [
        ScrollPhase::Began,
        ScrollPhase::Changed,
        ScrollPhase::Ended,
        ScrollPhase::Cancelled,
        ScrollPhase::MomentumBegan,
        ScrollPhase::Momentum,
        ScrollPhase::MomentumEnded,
    ] {
        let InjectEvent::TouchpadScroll {
            dx,
            dy,
            pixels_per_reference,
            wheel,
        } = inject(Os::Windows, ScrollAmount::Reference, phase)
        else {
            panic!("missing precision metadata")
        };
        assert_eq!(
            (dx, dy, pixels_per_reference),
            (3.0, -6.0, 1.0 / LEGACY_SCROLL_REFERENCE_PER_PIXEL)
        );
        assert_eq!(
            wheel,
            oink_foundation::input::ScrollInjection {
                dx: 85.75,
                dy: -171.5,
                unit: ScrollUnit::Line,
                phase
            }
        );
    }
}

#[test]
fn legacy_reference_and_line_delivery_use_fixed_units() {
    assert_eq!(
        inject(Os::Windows, ScrollAmount::Reference, ScrollPhase::None),
        InjectEvent::Scroll {
            dx: 85.75,
            dy: -171.5,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::None
        }
    );
    assert_eq!(
        inject(Os::Windows, ScrollAmount::Lines, ScrollPhase::Began),
        InjectEvent::Scroll {
            dx: 1.0,
            dy: -2.0,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::Began
        }
    );
    assert_eq!(
        inject(Os::MacOS, ScrollAmount::Reference, ScrollPhase::Began),
        InjectEvent::Scroll {
            dx: 8232.0,
            dy: -16464.0,
            unit: ScrollUnit::Pixel,
            phase: ScrollPhase::Began
        }
    );
}

#[test]
fn non_touchpad_sources_do_not_create_synthetic_contacts() {
    for source in [SourceKind::Mouse, SourceKind::Unknown] {
        assert!(matches!(
            inject_from(Os::Windows, ScrollAmount::Reference, ScrollPhase::Began, source),
            InjectEvent::Scroll { .. }
        ));
    }
}

#[test]
fn source_scroll_pixels_ignore_display_size() {
    for scale in [0.0001, 0.2, 4.0] {
        let mut r = Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: DeviceId([1; 32]),
                    nonce: [1; 16],
                },
                "source",
                Os::MacOS,
            ),
            Arc::new(crate::SimpleLayout::default()),
        );
        let peer = DeviceId([2; 32]);
        r.test_connect(peer, 0);
        r.set_peer_scroll_pixels(peer, true);
        r.holder = peer;
        r.claim = peer;
        r.cfg.settings.fallback_reference_per_native_unit = scale;
        r.out.clear();
        r.local_scroll(
            SourceKind::Touchpad,
            12.0,
            -37.0,
            ScrollUnit::Pixel,
            ScrollPhase::Began,
            None,
        );
        let deltas: Vec<_> = r
            .out
            .iter()
            .filter_map(|o| match o {
                Output::Send {
                    msg: Message::Input(InputMsg::Scroll { dx, dy, .. }),
                    ..
                } => Some((*dx, *dy)),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, [(12.0, -37.0)], "display scale {scale}");
    }
}

#[test]
fn source_pixels_reach_target_without_physical_scaling() {
    for scale in [0.0001, 0.2, 4.0] {
        for os in [Os::Windows, Os::MacOS] {
            let mut cfg = RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: DeviceId([1; 32]),
                    nonce: [1; 16],
                },
                "target",
                os,
            );
            cfg.settings.fallback_reference_per_native_unit = scale;
            cfg.settings.remote_scroll_speed = 1.5;
            let mut r = Router::new(cfg, Arc::new(crate::SimpleLayout::default()));
            r.inject_scroll(
                DeviceId([2; 32]),
                SourceKind::Touchpad,
                12.,
                -37.,
                ScrollAmount::Pixels,
                ScrollPhase::Changed,
                None,
            );
            match r.out.pop().unwrap() {
                Output::Inject(InjectEvent::TouchpadScroll {
                    dx,
                    dy,
                    pixels_per_reference,
                    ..
                }) => {
                    assert_eq!((dx * pixels_per_reference, dy * pixels_per_reference), (18., -55.5));
                }
                Output::Inject(InjectEvent::Scroll {
                    dx,
                    dy,
                    unit: ScrollUnit::Pixel,
                    ..
                }) => {
                    assert_eq!((dx, dy), (18., -55.5));
                }
                other => panic!("{other:?}"),
            }
        }
    }
}

#[test]
fn source_pixel_wheel_fallback_has_fixed_line_conversion() {
    assert_eq!(
        inject(Os::Windows, ScrollAmount::Pixels, ScrollPhase::None),
        InjectEvent::Scroll {
            dx: 3. / 96.,
            dy: -6. / 96.,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::None
        }
    );
}

#[test]
fn peer_without_pixel_capability_retains_legacy_reference_units() {
    let mut cfg = RouterConfig::new(
        oink_foundation::session::Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        },
        "source",
        Os::MacOS,
    );
    cfg.settings.fallback_reference_per_native_unit = 0.2;
    let mut r = Router::new(cfg, Arc::new(crate::SimpleLayout::default()));
    let peer = DeviceId([2; 32]);
    r.test_connect(peer, 0);
    r.holder = peer;
    r.claim = peer;
    for capable in [false, true, false] {
        r.set_peer_scroll_pixels(peer, capable);
        r.out.clear();
        r.local_scroll(
            SourceKind::Touchpad,
            10.,
            -20.,
            ScrollUnit::Pixel,
            ScrollPhase::Began,
            None,
        );
        let expected = if capable {
            (10., -20., ScrollAmount::Pixels)
        } else {
            (
                10. * LEGACY_SCROLL_REFERENCE_PER_PIXEL,
                -20. * LEGACY_SCROLL_REFERENCE_PER_PIXEL,
                ScrollAmount::Reference,
            )
        };
        assert!(r.out.iter().any(|o| matches!(o, Output::Send {
            msg: Message::Input(InputMsg::Scroll { dx, dy, unit, .. }), ..
        } if (*dx, *dy, *unit) == expected)));
    }
}

#[test]
fn every_scroll_source_and_direction_ignores_physical_display_size() {
    for source_os in [Os::MacOS, Os::Windows] {
        for target_os in [Os::MacOS, Os::Windows] {
            for kind in [SourceKind::Mouse, SourceKind::Touchpad, SourceKind::Unknown] {
                for source_unit in [ScrollUnit::Pixel, ScrollUnit::Line] {
                    for capable in [false, true] {
                        for (source_scale, target_scale) in [(0.0001, 4.), (4., 0.0001), (0.2, 0.2)] {
                            let from = DeviceId([1; 32]);
                            let to = DeviceId([2; 32]);
                            let make = |id, os, scale| {
                                let mut cfg = RouterConfig::new(
                                    oink_foundation::session::Boot {
                                        device_id: id,
                                        nonce: [1; 16],
                                    },
                                    "test",
                                    os,
                                );
                                cfg.settings.fallback_reference_per_native_unit = scale;
                                cfg.settings.line_height_ratio = scale * 100.;
                                Router::new(cfg, Arc::new(crate::SimpleLayout::default()))
                            };
                            let mut source = make(from, source_os, source_scale);
                            let mut target = make(to, target_os, target_scale);
                            source.test_connect(to, 0);
                            source.set_peer_scroll_pixels(to, capable);
                            source.holder = to;
                            source.claim = to;
                            source.feel.wheel_lines_per_notch = Some(3.);
                            target.feel.wheel_lines_per_notch = Some(2.);
                            target.test_connect(from, 0);
                            target.set_peer_os(from, source_os, 0);
                            target.peers.get_mut(&from).unwrap().feel = source.feel.clone();
                            source.out.clear();
                            source.local_scroll(kind, 0.25, -0.5, source_unit, ScrollPhase::None, None);
                            let (dx, dy, unit) = source
                                .out
                                .iter()
                                .find_map(|o| match o {
                                    Output::Send {
                                        msg: Message::Input(InputMsg::Scroll { dx, dy, unit, .. }),
                                        ..
                                    } => Some((*dx, *dy, *unit)),
                                    _ => None,
                                })
                                .unwrap();
                            target.inject_scroll(from, kind, dx, dy, unit, ScrollPhase::None, None);
                            let expected_pixels = if source_unit == ScrollUnit::Pixel {
                                (0.25, -0.5)
                            } else {
                                (24., -48.)
                            };
                            let Output::Inject(InjectEvent::Scroll { dx, dy, unit, .. }) = target.out.pop().unwrap()
                            else {
                                panic!("ordinary scroll");
                            };
                            let factor = if target_os == Os::Windows { 64. } else { 1. };
                            assert_eq!(
                                unit,
                                if target_os == Os::Windows {
                                    ScrollUnit::Line
                                } else {
                                    ScrollUnit::Pixel
                                }
                            );
                            assert!(
                                (dx * factor - expected_pixels.0).abs() < 1e-10
                                    && (dy * factor - expected_pixels.1).abs() < 1e-10,
                                "{source_os:?}→{target_os:?} {kind:?} {source_unit:?} cap={capable} scales={source_scale}/{target_scale}: {dx}/{dy}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn ordinary_line_wheel_to_pixels_uses_fixed_line_height() {
    assert_eq!(
        inject_from(Os::MacOS, ScrollAmount::Lines, ScrollPhase::None, SourceKind::Mouse),
        InjectEvent::Scroll {
            dx: 96.,
            dy: -192.,
            unit: ScrollUnit::Pixel,
            phase: ScrollPhase::None
        }
    );
}

#[test]
fn mouse_notches_survive_different_wheel_line_settings_in_every_game_mode() {
    let from = DeviceId([1; 32]);
    let to = DeviceId([2; 32]);
    let make = |id, os| {
        Router::new(
            RouterConfig::new(
                oink_foundation::session::Boot {
                    device_id: id,
                    nonce: [id.0[0]; 16],
                },
                "test",
                os,
            ),
            Arc::new(crate::SimpleLayout::default()),
        )
    };
    for (source_os, source_lines) in [
        (Os::Windows, Some(0.)),
        (Os::Windows, Some(1.)),
        (Os::Windows, Some(3.)),
        (Os::Windows, Some(6.)),
        (Os::MacOS, None),
    ] {
        for target_lines in [1., 3., 6.] {
            for mode in [GameMode::Auto, GameMode::On, GameMode::Off] {
                let mut source = make(from, source_os);
                let mut target = make(to, Os::Windows);
                source.test_connect(to, 0);
                target.test_connect(from, 0);
                source.holder = to;
                source.claim = to;
                source.feel.wheel_lines_per_notch = source_lines;
                source.cfg.settings.forward_wheel_by_lines = false;
                target.feel.wheel_lines_per_notch = Some(target_lines);
                let peer = target.peers.get_mut(&from).unwrap();
                peer.os = source_os;
                peer.feel = source.feel.clone();
                target.set_game_mode(mode);
                assert_eq!(target.game_mode().active, mode == GameMode::On);
                // Both whole detents and native high-resolution reports retain
                // their source units; changing system line counts cannot split
                // a detent into sub-120 SendInput messages or multiply it.
                for notch in [1., -1., 2., -2., 0.25, -0.25] {
                    source.out.clear();
                    target.out.clear();
                    source.local_scroll(
                        SourceKind::Mouse,
                        notch,
                        -notch,
                        ScrollUnit::Line,
                        ScrollPhase::None,
                        None,
                    );
                    let (dx, dy, unit, steps) = source
                        .out
                        .iter()
                        .find_map(|o| match o {
                            Output::Send {
                                msg:
                                    Message::Input(InputMsg::Scroll {
                                        dx,
                                        dy,
                                        unit,
                                        wheel_steps,
                                        ..
                                    }),
                                ..
                            } => Some((*dx, *dy, *unit, *wheel_steps)),
                            _ => None,
                        })
                        .unwrap();
                    target.inject_scroll(from, SourceKind::Mouse, dx, dy, unit, ScrollPhase::None, steps);
                    assert_eq!(
                        target.out.pop(),
                        Some(Output::Inject(InjectEvent::Scroll {
                            dx: notch,
                            dy: -notch,
                            unit: ScrollUnit::Line,
                            phase: ScrollPhase::None,
                        })),
                        "source={source_os:?}, source lines={source_lines:?}, target lines={target_lines}, mode={mode:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn explicit_steps_ignore_peer_profiles_and_retain_speed_and_direction() {
    use oink_foundation::input_transform::InputTransform;
    let original = DeviceId([2; 32]);
    let relay = DeviceId([3; 32]);
    let mut cfg = RouterConfig::new(
        oink_foundation::session::Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        },
        "target",
        Os::Windows,
    );
    cfg.settings.remote_scroll_speed = 2.;
    let mut target = Router::new(cfg, Arc::new(crate::SimpleLayout::default()));
    target.feel.wheel_lines_per_notch = Some(3.);
    for (id, lines) in [(original, 6.), (relay, 1.)] {
        target.test_connect(id, 0);
        let peer = target.peers.get_mut(&id).unwrap();
        peer.os = Os::Windows;
        peer.feel.wheel_lines_per_notch = Some(lines);
    }
    let mut transform = InputTransform::initial("windows");
    transform.windows.reverse_wheel = true;
    target.set_input_transforms([(original, transform)].into());
    target.out.clear();
    target.inject_scroll(
        original,
        SourceKind::Mouse,
        12.,
        -6.,
        ScrollAmount::Lines,
        ScrollPhase::None,
        Some(Point::new(2., -1.)),
    );
    assert_eq!(
        target.out.pop(),
        Some(Output::Inject(InjectEvent::Scroll {
            dx: -4.,
            dy: 2.,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::None,
        }))
    );
    // The same known Windows source's continuous input still uses target
    // line counts. Restoring notches must not quantize the smooth fallback.
    target.inject_scroll(
        original,
        SourceKind::Touchpad,
        0.75,
        -1.5,
        ScrollAmount::Lines,
        ScrollPhase::None,
        None,
    );
    assert_eq!(
        target.out.pop(),
        Some(Output::Inject(InjectEvent::Scroll {
            dx: 0.5,
            dy: -1.,
            unit: ScrollUnit::Line,
            phase: ScrollPhase::None,
        }))
    );
}

#[test]
fn source_line_preference_controls_both_platforms_without_target_preference_adoption() {
    let from = DeviceId([1; 32]);
    let to = DeviceId([2; 32]);
    for by_lines in [true, false] {
        for source_os in [Os::Windows, Os::MacOS] {
            for target_os in [Os::Windows, Os::MacOS] {
                let make = |id, os| {
                    Router::new(
                        RouterConfig::new(
                            oink_foundation::session::Boot {
                                device_id: id,
                                nonce: [id.0[0]; 16],
                            },
                            "test",
                            os,
                        ),
                        Arc::new(crate::SimpleLayout::default()),
                    )
                };
                let mut source = make(from, source_os);
                let mut target = make(to, target_os);
                source.cfg.settings.forward_wheel_by_lines = by_lines;
                // The receiving device's source preference must not change this event.
                target.cfg.settings.forward_wheel_by_lines = !by_lines;
                source.feel.wheel_lines_per_notch = (source_os == Os::Windows).then_some(3.);
                target.feel.wheel_lines_per_notch = (target_os == Os::Windows).then_some(6.);
                source.test_connect(to, 0);
                source.holder = to;
                source.claim = to;
                source.out.clear();
                let amount = if source_os == Os::Windows { 1. } else { 4.5 };
                // Mac's accelerated line amount can differ from the raw step.
                source.local_scroll(
                    SourceKind::Mouse,
                    amount,
                    -amount * 2.,
                    ScrollUnit::Line,
                    ScrollPhase::None,
                    Some(Point::new(1., -2.)),
                );
                let (dx, dy, unit, steps) = source
                    .out
                    .iter()
                    .find_map(|o| match o {
                        Output::Send {
                            msg:
                                Message::Input(InputMsg::Scroll {
                                    dx,
                                    dy,
                                    unit,
                                    wheel_steps,
                                    ..
                                }),
                            ..
                        } => Some((*dx, *dy, *unit, *wheel_steps)),
                        _ => None,
                    })
                    .unwrap();
                let lines = if source_os == Os::Windows { 3. } else { 4.5 };
                assert_eq!((dx, dy, unit), (lines, -lines * 2., ScrollAmount::Lines));
                assert_eq!(steps, (!by_lines).then_some(Point::new(1., -2.)));
                let factor = if target_os == Os::Windows { 1. / 6. } else { 32. };
                for old_receiver in [false, true] {
                    target.out.clear();
                    // An old receiver ignores key 103: the unchanged base fields
                    // still yield source lines, exactly the ON fallback.
                    target.inject_scroll(
                        from,
                        SourceKind::Mouse,
                        dx,
                        dy,
                        unit,
                        ScrollPhase::None,
                        if old_receiver { None } else { steps },
                    );
                    let native = !by_lines && !old_receiver;
                    assert_eq!(
                        target.out.pop(),
                        Some(Output::Inject(InjectEvent::Scroll {
                            dx: if native { 1. } else { lines * factor },
                            dy: if native { -2. } else { -lines * 2. * factor },
                            unit: if native || target_os == Os::Windows {
                                ScrollUnit::Line
                            } else {
                                ScrollUnit::Pixel
                            },
                            phase: ScrollPhase::None,
                        })),
                        "lines={by_lines}, source={source_os:?}, target={target_os:?}, old={old_receiver}"
                    );
                }
            }
        }
    }
}

#[test]
fn source_wheel_preference_never_marks_continuous_or_pixel_events_as_steps() {
    for by_lines in [true, false] {
        for (kind, unit, phase) in [
            (SourceKind::Touchpad, ScrollUnit::Pixel, ScrollPhase::None),
            (SourceKind::Touchpad, ScrollUnit::Pixel, ScrollPhase::Began),
            (SourceKind::Mouse, ScrollUnit::Pixel, ScrollPhase::None),
            (SourceKind::Mouse, ScrollUnit::Line, ScrollPhase::Began),
        ] {
            let mut r = Router::new(
                RouterConfig::new(
                    oink_foundation::session::Boot {
                        device_id: DeviceId([1; 32]),
                        nonce: [1; 16],
                    },
                    "source",
                    Os::MacOS,
                ),
                Arc::new(crate::SimpleLayout::default()),
            );
            let peer = DeviceId([2; 32]);
            r.test_connect(peer, 0);
            r.holder = peer;
            r.cfg.settings.forward_wheel_by_lines = by_lines;
            r.out.clear();
            r.local_scroll(kind, 0.25, -0.5, unit, phase, Some(Point::new(1., -2.)));
            assert!(r.out.iter().any(|o| matches!(
                o,
                Output::Send {
                    msg: Message::Input(InputMsg::Scroll { wheel_steps: None, .. }),
                    ..
                }
            )));
        }
    }
}
