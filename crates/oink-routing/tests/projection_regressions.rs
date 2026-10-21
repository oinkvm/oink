//! Read-only audit fixtures: pure production geometry, never native input or app IPC.
use oink_display::{
    layout::{self as dl, Layout, OwnerLayoutPreferences, ScreenPlacement, ScreenPreference, SizeSource},
    projection,
};
use oink_foundation::{DeviceId, Point, Rect};
use oink_routing::{LayoutQuery, ScreenId, ScreenPos, TopologyLayout};
const H: DeviceId = DeviceId([10; 32]);
const N: DeviceId = DeviceId([11; 32]);
const F: DeviceId = DeviceId([12; 32]);
fn s(device: DeviceId, name: &str, r: Rect) -> ScreenPlacement {
    ScreenPlacement {
        id: dl::ScreenId::new(device, name),
        name: name.into(),
        local: Rect::new(0., 0., 1000., 1000.),
        rect: r,
        size_source: SizeSource::Manual,
        primary: true,
        builtin: false,
    }
}
fn layout(screens: Vec<ScreenPlacement>) -> Layout {
    Layout {
        screens,
        ..Default::default()
    }
}
fn at(x: f64, y: f64) -> ScreenPos {
    ScreenPos::new(ScreenId::new(H, "H"), Point::new(x * 1000., y * 1000.))
}
fn target(l: &Layout, p: ScreenPos, d: (f64, f64)) -> Option<String> {
    TopologyLayout::from_layout(l, &Default::default())
        .first_portal_native(&p, d)
        .map(|e| e.to.screen.display)
}
#[test]
fn short_ineligible_neighbour_occludes_only_its_own_interval() {
    let l = layout(vec![
        s(H, "H", Rect::new(0., 0., 1., 1.)),
        s(N, "N", Rect::new(1.1, 0.99, 1., 1.)),
        s(F, "F", Rect::new(2.3, 0., 1., 1.)),
    ]);
    let others = l.screens[1..]
        .iter()
        .map(|s| (s.id.clone(), s.rect))
        .collect::<Vec<_>>();
    let spans = projection::edge_spans(l.screens[0].rect, &others);
    assert!(spans.iter().any(|p| p.neighbour.device == F && p.a < 0.8 && p.b > 0.8));
    assert!(spans.iter().any(|p| p.neighbour.device == N));
    assert_eq!(target(&l, at(0.999, 0.8), (10., 0.)).as_deref(), Some("F"));
    assert_eq!(target(&l, at(0.999, 0.995), (10., 0.)), None);
}
#[test]
fn hidden_or_disabled_screen_does_not_block_native_projection_edit() {
    let l = layout(vec![
        s(H, "H", Rect::new(0., 0., 1., 1.)),
        s(N, "N", Rect::new(1., 0., 1., 1.)),
        s(F, "Hidden", Rect::new(1., 1.1, 1., 1.)),
    ]);
    for disabled_device in [true, false] {
        let mut owner = OwnerLayoutPreferences {
            screen_shared: !disabled_device,
            ..Default::default()
        };
        if !disabled_device {
            owner.screens.insert(
                "Hidden".into(),
                ScreenPreference {
                    enabled: false,
                    ..Default::default()
                },
            );
        }
        let owners = std::collections::BTreeMap::from([(F, owner)]);
        let p = projection::compute(&l.effective(&owners), H, |_| true, None).remove(0);
        let edited = p
            .edit_with_preferences(&l, &owners, projection::Handle::Middle, 500.)
            .unwrap();
        assert!((edited.screen(&dl::ScreenId::new(N, "N")).unwrap().rect.y - 0.5).abs() < 1e-9);
    }
}

#[test]
fn native_group_growth_stops_at_first_intermediate_collision() {
    let main = s(N, "N", Rect::new(1., 0., 1., 1.));
    let mut side = s(N, "Side", Rect::new(3., 0., 1., 1.));
    side.local = Rect::new(2000., 0., 1000., 1000.);
    side.primary = false;
    let l = layout(vec![
        s(H, "H", Rect::new(0., 0., 1., 1.)),
        main,
        side,
        s(F, "Obstacle", Rect::new(4.1, 0.2, 0.1, 0.1)),
    ]);
    let p = projection::compute(&l.effective(&Default::default()), H, |_| true, None)
        .into_iter()
        .find(|p| p.neighbour.display == "N")
        .unwrap();
    let near = p.edit(&l, projection::Handle::End, 50.).unwrap();
    let far = p.edit(&l, projection::Handle::End, 1000.).unwrap();
    let near_w = near.screen(&dl::ScreenId::new(N, "N")).unwrap().rect.w;
    let far_w = far.screen(&dl::ScreenId::new(N, "N")).unwrap().rect.w;
    assert!(near_w < 1.04);
    assert!((far_w - near_w).abs() < 1e-9);
    let obstacle = l.screens[3].rect;
    let intermediate = Rect::new(1. + 2. * 1.1, 0., 1.1, 1.1);
    assert!(dl::facing::overlaps(intermediate, obstacle, 1e-10));
}

#[test]
fn native_group_shrink_stops_before_a_crossed_obstacle_too() {
    let mut side = s(N, "Side", Rect::new(3., 0., 1., 1.));
    side.local = Rect::new(2000., 0., 1000., 1000.);
    side.primary = false;
    let l = layout(vec![
        s(H, "H", Rect::new(0., 0., 1., 1.)),
        s(N, "N", Rect::new(1., 0., 1., 1.)),
        side,
        s(F, "Obstacle", Rect::new(2.7, 0.2, 0.1, 0.1)),
    ]);
    let p = projection::compute(&l.effective(&Default::default()), H, |_| true, None)
        .into_iter()
        .find(|p| p.neighbour.display == "N")
        .unwrap();
    let edited = p.edit(&l, projection::Handle::End, -500.).unwrap();
    let scale = edited.screen(&dl::ScreenId::new(N, "N")).unwrap().rect.w;
    assert!((scale - 0.9).abs() < 1e-9);
    assert!(edited.overlaps().is_empty());
}

#[test]
fn nearer_screen_splits_a_far_portal_into_both_visible_intervals() {
    let l = layout(vec![
        s(H, "H", Rect::new(0., 0., 1., 1.)),
        s(N, "N", Rect::new(1.1, 0.3, 1., 0.2)),
        s(F, "F", Rect::new(2.3, 0., 1., 1.)),
    ]);
    for (along, expected) in [(0.1, "F"), (0.4, "N"), (0.8, "F")] {
        assert_eq!(target(&l, at(0.999, along), (10., 0.)).as_deref(), Some(expected));
    }
}

#[test]
fn local_annotations_leave_native_and_remote_routes_unchanged() {
    let mut second = s(H, "Second", Rect::new(1., 0., 1., 1.));
    second.local.x = 1000.;
    second.primary = false;
    let l = layout(vec![
        s(H, "H", Rect::new(0., 0., 1., 1.)),
        second,
        s(F, "F", Rect::new(2.3, 0., 1., 1.)),
    ]);
    let stored = l.clone();
    let route = TopologyLayout::from_layout(&l, &Default::default());
    let points = [
        at(0.999, 0.5),
        ScreenPos::new(ScreenId::new(H, "Second"), Point::new(1999., 500.)),
    ];
    let before = points
        .iter()
        .map(|p| route.first_portal_native(p, (10., 0.)).unwrap().to.screen)
        .collect::<Vec<_>>();
    assert_eq!(before, [dl::ScreenId::new(H, "Second"), dl::ScreenId::new(F, "F")]);
    let projections = projection::compute(&l.effective(&Default::default()), H, |_| true, None);
    assert!(projections.iter().any(|p| p.host.device == p.neighbour.device));
    for p in projections.iter().filter(|p| p.host.device == p.neighbour.device) {
        assert!(p.handles().is_empty());
        assert_eq!(p.edit(&l, projection::Handle::Middle, 100.), None);
    }
    let after = points
        .iter()
        .map(|p| route.first_portal_native(p, (10., 0.)).unwrap().to.screen)
        .collect::<Vec<_>>();
    assert_eq!(before, after);
    assert_eq!(l, stored);
}
