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
fn windows_phased_mm_retains_distance_and_existing_wheel_conversion() {
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
        assert_eq!((dx, dy, pixels_per_reference), (3.0, -6.0, 5.0));
        assert_eq!(
            wheel,
            oink_foundation::input::ScrollInjection {
                dx: 0.125,
                dy: -0.25,
                unit: ScrollUnit::Line,
                phase
            }
        );
    }
}

#[test]
fn wheel_and_mac_delivery_are_unchanged() {
    assert_eq!(
        inject(Os::Windows, ScrollAmount::Reference, ScrollPhase::None),
        InjectEvent::Scroll {
            dx: 0.125,
            dy: -0.25,
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
            dx: 15.0,
            dy: -30.0,
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
