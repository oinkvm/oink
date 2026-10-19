use super::*;
fn sid(device: DeviceId, display: &str) -> ScreenId {
    ScreenId::new(device, display)
}
fn screen(device: DeviceId, key: &str, rect: Rect, upm: f64) -> ScreenPlacement {
    ScreenPlacement {
        id: sid(device, key),
        name: key.into(),
        local: Rect::new(rect.x * upm, rect.y * upm, rect.w * upm, rect.h * upm),
        rect,
        size_source: SizeSource::Manual,

        primary: false,
        builtin: false,
    }
}
/// Trigger zones are off by default; these tests exercise them at the former 8/686 reference units reach.
fn layout(screens: Vec<ScreenPlacement>) -> Layout {
    let mut l = Layout {
        screens,
        ..Default::default()
    };
    l.settings.endpoint_reach = 8. / 686.0;
    l
}
fn at(t: &Topology, id: &ScreenId, plane: Point) -> ScreenPos {
    ScreenPos {
        screen: id.clone(),
        local: t.from_plane(id, plane).unwrap(),
    }
}
fn plane(t: &Topology, pos: &ScreenPos) -> Point {
    t.to_plane(pos).unwrap()
}

const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
const C: DeviceId = DeviceId([3; 32]);

fn pair() -> Layout {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0., 0., 200. / 686.0, 200. / 686.0), 4. * 686.0),
        screen(
            B,
            "b",
            Rect::new(220. / 686.0, 50. / 686.0, 100. / 686.0, 100. / 686.0),
            2. * 686.0,
        ),
    ]);
    l.settings.portal_snap = 0.;
    l.settings.endpoint_reach = 12. / 686.0;
    l
}

#[test]
fn zones_have_physical_size_and_map_to_overlap_ends() {
    let t = pair().topology(&Default::default());
    let zones = t.trigger_zones();
    let right: Vec<_> = zones
        .iter()
        .filter(|z| z.screen == sid(A, "a") && z.side == Side::Right)
        .collect();
    assert_eq!(right.len(), 2);
    assert_eq!(
        (right[0].from, right[0].to, right[0].entry_along),
        ((38. / 686.0), (50. / 686.0), (50. / 686.0))
    );
    assert_eq!(
        (right[1].from, right[1].to, right[1].entry_along),
        ((150. / 686.0), (162. / 686.0), (150. / 686.0))
    );
    for (y, expected) in [((40. / 686.0), (50. / 686.0)), ((160. / 686.0), (150. / 686.0))] {
        let from = at(&t, &sid(A, "a"), Point::new(199. / 686.0, y));
        let exit = t.first_portal(&from, ((2. / 686.0), 0.)).unwrap();
        assert!(exit.trigger_zone);
        let entry = plane(&t, &exit.to);
        assert!(
            (entry.x - (220. / 686.0)).abs() < (1e-5 / 686.0) && (entry.y - expected).abs() < (1e-5 / 686.0),
            "{entry:?}"
        );
    }
    let from = at(&t, &sid(A, "a"), Point::new(199. / 686.0, 37. / 686.0));
    assert!(t.first_portal(&from, ((2. / 686.0), 0.)).is_none());
}

#[test]
fn competing_zones_split_only_the_wall_and_preserve_channels() {
    let mut l = pair();
    l.screens[1].rect.h = 40. / 686.0;
    l.screens.push(screen(
        C,
        "c",
        Rect::new(220. / 686.0, 110. / 686.0, 100. / 686.0, 40. / 686.0),
        2. * 686.0,
    ));
    let t = l.topology(&Default::default());
    for (y, target, entry, zone) in [
        ((89. / 686.0), B, (89. / 686.0), false),
        ((99. / 686.0), B, (90. / 686.0), true),
        ((100. / 686.0), B, (90. / 686.0), true),
        ((101. / 686.0), C, (110. / 686.0), true),
        ((111. / 686.0), C, (111. / 686.0), false),
    ] {
        let from = at(&t, &sid(A, "a"), Point::new(199. / 686.0, y));
        let exit = t.first_portal(&from, ((2. / 686.0), 0.)).unwrap();
        assert_eq!(exit.to.screen.device, target);
        assert_eq!(exit.trigger_zone, zone);
        assert!((plane(&t, &exit.to).y - entry).abs() < (1e-5 / 686.0));
    }
}

#[test]
fn corners_clip_zones_and_choose_channels_before_zones_then_x() {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0., 0., 200. / 686.0, 200. / 686.0), 1. * 686.0),
        screen(
            B,
            "b",
            Rect::new(220. / 686.0, 0., 100. / 686.0, 195. / 686.0),
            1. * 686.0,
        ),
        screen(
            C,
            "c",
            Rect::new(0., 220. / 686.0, 195. / 686.0, 100. / 686.0),
            1. * 686.0,
        ),
    ]);
    l.settings.portal_snap = 0.;
    let t = l.topology(&Default::default());
    assert!(t.trigger_zones().iter().all(|z| z.to <= (200. / 686.0) && z.from >= 0.));
    let from = at(&t, &sid(A, "a"), Point::new(199. / 686.0, 199. / 686.0));
    assert_eq!(
        t.first_portal(&from, ((2. / 686.0), (2. / 686.0)))
            .unwrap()
            .to
            .screen
            .device,
        B
    );
    l.screens[2].rect.w = 200. / 686.0;
    let t = l.topology(&Default::default());
    let exit = t.first_portal(&from, ((2. / 686.0), (2. / 686.0))).unwrap();
    assert_eq!(exit.to.screen.device, C);
    assert!(!exit.trigger_zone);
    // A pair touching only at a corner still has no channel to extend.
    l.screens = vec![
        l.screens[0].clone(),
        screen(
            B,
            "b",
            Rect::new(200. / 686.0, 200. / 686.0, 100. / 686.0, 100. / 686.0),
            1. * 686.0,
        ),
    ];
    assert!(l.topology(&Default::default()).trigger_zones().is_empty());
}

#[test]
fn zone_entry_preserves_normal_remainder_and_target_gain() {
    let mut l = pair();
    let owners = BTreeMap::from([(
        B,
        OwnerLayoutPreferences {
            device_gain: 3.,
            ..Default::default()
        },
    )]);
    let t = l.topology(&owners);
    let from = at(&t, &sid(A, "a"), Point::new(199. / 686.0, 40. / 686.0));
    let result = t.step(&from, ((5. / 686.0), 0.)).unwrap();
    let p = plane(&t, &result.to);
    assert!((p.x - (232. / 686.0)).abs() < (1e-5 / 686.0) && (p.y - (50. / 686.0)).abs() < (1e-5 / 686.0));
    // An ordinary channel gives exactly the same result with zones disabled.
    let normal = at(&t, &sid(A, "a"), Point::new(199. / 686.0, 80. / 686.0));
    let before = t.step(&normal, ((5. / 686.0), (2. / 686.0))).unwrap();
    l.settings.endpoint_reach = 0.;
    assert_eq!(
        before,
        l.topology(&owners).step(&normal, ((5. / 686.0), (2. / 686.0))).unwrap()
    );
    assert!(l.topology(&owners).first_portal(&from, ((5. / 686.0), 0.)).is_none());
}

#[test]
fn zones_follow_enabled_online_and_multi_monitor_geometry() {
    let mut owners = BTreeMap::from([(A, OwnerLayoutPreferences::default())]);
    let mut l = pair();
    l.screens[1].id.device = A;
    let t = l.topology(&owners);
    let from = at(&t, &sid(A, "a"), Point::new(199.0 / 686.0, 40.0 / 686.0));
    assert!(
        t.first_portal(&from, (2.0 / 686.0, 0.0)).is_none(),
        "Native gaps are OS walls"
    );
    owners
        .get_mut(&A)
        .unwrap()
        .screens
        .entry("b".into())
        .or_default()
        .enabled = false;
    assert!(l.topology(&owners).trigger_zones().is_empty());
    owners
        .get_mut(&A)
        .unwrap()
        .screens
        .entry("b".into())
        .or_default()
        .enabled = true;
    assert!(l.topology_online(&owners, |_| false).trigger_zones().is_empty());
    owners.get_mut(&A).unwrap().screen_shared = false;
    assert!(l.topology(&owners).trigger_zones().is_empty());
}

#[test]
fn each_edge_snaps_to_the_nearest_overlap_endpoint() {
    for (target, start, delta, expected) in [
        (
            Rect::new(220. / 686.0, 50. / 686.0, 100. / 686.0, 100. / 686.0),
            Point::new(199. / 686.0, 45. / 686.0),
            ((2. / 686.0), 0.),
            Point::new(220. / 686.0, 50. / 686.0),
        ),
        (
            Rect::new(-(120. / 686.0), 50. / 686.0, 100. / 686.0, 100. / 686.0),
            Point::new(1. / 686.0, 155. / 686.0),
            (-(2. / 686.0), 0.),
            Point::new(-(20. / 686.0), 150. / 686.0),
        ),
        (
            Rect::new(50. / 686.0, 220. / 686.0, 100. / 686.0, 100. / 686.0),
            Point::new(45. / 686.0, 199. / 686.0),
            (0., (2. / 686.0)),
            Point::new(50. / 686.0, 220. / 686.0),
        ),
        (
            Rect::new(50. / 686.0, -(120. / 686.0), 100. / 686.0, 100. / 686.0),
            Point::new(155. / 686.0, 1. / 686.0),
            (0., -(2. / 686.0)),
            Point::new(150. / 686.0, -(20. / 686.0)),
        ),
    ] {
        let t = layout(vec![
            screen(A, "a", Rect::new(0., 0., 200. / 686.0, 200. / 686.0), 4. * 686.0),
            screen(B, "b", target, 7. * 686.0),
        ])
        .topology(&Default::default());
        let exit = t.first_portal(&at(&t, &sid(A, "a"), start), delta).unwrap();
        assert!(exit.trigger_zone);
        let entry = plane(&t, &exit.to);
        assert!(
            (entry.x - expected.x).hypot(entry.y - expected.y) < (1e-4 / 686.0),
            "{entry:?}"
        );
    }
}

#[test]
fn real_channel_owns_shared_zone_boundary_even_when_masked_offline() {
    let mut l = pair();
    l.screens[1].rect.h = 40. / 686.0;
    l.screens.push(screen(
        C,
        "c",
        Rect::new(220. / 686.0, 110. / 686.0, 100. / 686.0, 40. / 686.0),
        2. * 686.0,
    ));
    let t = l.topology(&Default::default());
    let p = at(&t, &sid(A, "a"), Point::new(199. / 686.0, 90. / 686.0));
    let exit = t.first_portal(&p, ((2. / 686.0), 0.)).unwrap();
    assert_eq!(exit.to.screen.device, B);
    assert!(!exit.trigger_zone);
    assert!(t.first_portal_filtered(&p, ((2. / 686.0), 0.), |d| *d != B).is_none());
}

#[test]
fn native_first_portal_shares_axis_conversion_corner_choice_and_online_mask() {
    let mut source = screen(A, "a", Rect::new(0., 0., 200. / 686., 200. / 686.), 1.);
    source.local = Rect::new(-50., -20., 1200., 600.);
    let mut target = screen(B, "b", Rect::new(0., 220. / 686., 200. / 686., 200. / 686.), 1.);
    target.local = Rect::new(0., 0., 900., 1200.);
    let t = layout(vec![source, target]).topology(&Default::default());
    let from = ScreenPos {
        screen: sid(A, "a"),
        local: Point::new(-40., 579.5),
    };
    let delta = (-100., 2.);
    let portal = t.first_portal_native_filtered(&from, delta, |_| true).unwrap();
    let exit = t.exit_through_edge_filtered(&from, delta, |_| true).unwrap();
    assert_eq!(portal.side, Side::Bottom);
    assert_eq!(portal.to.screen, exit.screen);
    assert!(t.first_portal_native_filtered(&from, delta, |d| *d != B).is_none());
    assert!(t.exit_through_edge_filtered(&from, delta, |d| *d != B).is_none());
}
