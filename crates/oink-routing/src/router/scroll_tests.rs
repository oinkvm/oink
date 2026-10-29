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
    r.inject_scroll(DeviceId([2; 32]), source, 2.0, -4.0, amount, phase);
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
        r.local_scroll(SourceKind::Touchpad, 12.0, -37.0, ScrollUnit::Pixel, ScrollPhase::Began);
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
        r.local_scroll(SourceKind::Touchpad, 10., -20., ScrollUnit::Pixel, ScrollPhase::Began);
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
                            source.out.clear();
                            source.local_scroll(kind, 0.25, -0.5, source_unit, ScrollPhase::None);
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
                            target.inject_scroll(from, kind, dx, dy, unit, ScrollPhase::None);
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
