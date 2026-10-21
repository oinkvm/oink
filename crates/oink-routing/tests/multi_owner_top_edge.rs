//! Investigation fixture only: no native input, network or app-state writes.
use oink_display::layout::{
    self as dl, Layout, OwnerLayoutPreferences, ScreenPlacement, ScreenPreference, Side, SizeSource,
};
use oink_foundation::{DeviceId, Point, Rect};
use oink_routing::{LayoutQuery, ScreenId, ScreenPos, TopologyLayout};
use serde_json::Value;
use std::collections::BTreeMap;
const C: DeviceId = DeviceId([1; 32]);
const S: DeviceId = DeviceId([2; 32]);
const A: DeviceId = DeviceId([3; 32]);
fn rect(v: &Value) -> Rect {
    Rect::new(
        v["x"].as_f64().unwrap(),
        v["y"].as_f64().unwrap(),
        v["w"].as_f64().unwrap(),
        v["h"].as_f64().unwrap(),
    )
}
fn fixture() -> (Layout, BTreeMap<DeviceId, OwnerLayoutPreferences>) {
    let v: Value = serde_json::from_str(include_str!("fixtures/multi_owner_top_edge.json")).unwrap();
    assert_eq!(v["revision"], 110);
    let mut layout = Layout {
        screens: vec![],
        settings: serde_json::from_value(v["settings"].clone()).unwrap(),
    };
    let mut owners = BTreeMap::<DeviceId, OwnerLayoutPreferences>::new();
    for s in v["screens"].as_array().unwrap() {
        let device = match s["device"].as_str().unwrap() {
            "C" => C,
            "S" => S,
            "A" => A,
            _ => unreachable!(),
        };
        assert!(s["online"].as_bool().unwrap());
        let code = s["code"].as_str().unwrap();
        layout.screens.push(ScreenPlacement {
            id: dl::ScreenId::new(device, code),
            name: code.into(),
            local: rect(&s["local"]),
            rect: rect(&s["rect"]),
            size_source: SizeSource::Manual,
            primary: code.ends_with('1'),
            builtin: device != A && code.ends_with('1'),
        });
        let owner = owners.entry(device).or_default();
        owner.screen_shared = !s["disabled"].as_bool().unwrap();
        owner.screens.insert(
            code.into(),
            ScreenPreference {
                enabled: !s["hidden"].as_bool().unwrap(),
                gain: s["gain"].as_f64().unwrap(),
                remote_gain: s["remote_gain"].as_f64().unwrap(),
            },
        );
    }
    (layout, owners)
}
fn native_at(x: f64, y: f64) -> ScreenPos {
    ScreenPos::new(ScreenId::new(C, "C1"), Point::new(x, y))
}
fn code(t: &TopologyLayout, p: ScreenPos, d: (f64, f64)) -> Option<String> {
    t.first_portal_native(&p, d).map(|e| e.to.screen.display)
}
#[test]
fn unoccluded_top_edge_reaches_the_visible_remote_owner() {
    let (layout, owners) = fixture();
    let route = TopologyLayout::from_layout(&layout, &owners);
    let host = layout.screens.iter().find(|s| s.id.device == C).unwrap();
    let others = layout
        .screens
        .iter()
        .filter(|s| s.id.device != C)
        .map(|s| (s.id.clone(), s.rect))
        .collect::<Vec<_>>();
    let spans = oink_display::projection::edge_spans(host.rect, &others);
    let top = spans.iter().filter(|s| s.side == Side::Top).collect::<Vec<_>>();
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].neighbour.display, "S2");
    assert_eq!(top[1].neighbour.display, "A1");
    assert!((top[1].a - 0.748420378818).abs() < 1e-10);
    // Compare every native point along C's top edge with the visible neighbour.
    for x in 0..1728 {
        let x = f64::from(x) + 0.5;
        let span = top.iter().find(|s| s.a * 1728. <= x && x < s.b * 1728.).unwrap();
        assert_eq!(
            code(&route, native_at(x, 0.5), (0., -10.)).as_deref(),
            Some(span.neighbour.display.as_str())
        );
    }
    let result = route.step(&native_at(1500., 0.5), (0., -10. * host.physical_scale().1));
    assert_eq!(result.to.screen.device, A);
    assert!(!result.blocked_y);
}
